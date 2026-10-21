//! Windows backend: Core Audio sessions on every active render endpoint, not
//! only the default one (e.g. Elgato Wave Link's per-channel virtual outputs).
//!
//! Every COM object lives on the engine's worker thread, which initialises COM
//! as multithreaded (MTA). API calls reach it through the engine's command
//! channel. Session and endpoint notifications arrive on COM worker threads;
//! they only set a flag and ask the worker for a refresh (`Notifier`), they
//! never touch session objects.
//!
//! Endpoint and session lifecycle, identity, aggregation and write fan-out live
//! in the platform-independent [`Inventory`]; this file is the thin COM layer
//! behind its [`Platform`] trait. Sessions are grouped by executable file name
//! across endpoints; the system-sounds sessions are exposed as the app
//! `system`. A process whose image path cannot be queried keeps the existing
//! `pid:<n>` fallback id (not stable across restarts); a session whose volume
//! or events interface cannot be obtained is skipped. No elevation is needed.

use std::ffi::c_void;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, Ref, BOOL, GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, PROPERTYKEY, S_OK};
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    eCapture, EDataFlow, ERole, IMMDevice, IMMNotificationClient, IMMNotificationClient_Impl,
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateActive,
    AudioSessionStateInactive, IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents,
    IAudioSessionEvents_Impl, IAudioSessionManager2, IAudioSessionNotification,
    IAudioSessionNotification_Impl, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::engine::{
    AppStatus, Backend, BackendKind, Error, ErrorKind, MakeBackend, Notifier, Permission,
};
use super::model::{
    slugify, valid_device_id, windows_app_id, windows_app_name, App, Device, DeviceKind,
};
use super::sessions::{Adopted, Inventory, Listed, Platform, SessionState};

const POLL_INTERVAL: Duration = Duration::from_secs(1);

fn native(what: &str, e: windows::core::Error) -> Error {
    Error::new(ErrorKind::Native, format!("{what} failed: {e}"))
}

fn device_error(what: &str, e: windows::core::Error) -> Error {
    let kind = if e.code() == windows::Win32::Foundation::E_ACCESSDENIED {
        ErrorKind::Permission
    } else if e.code() == windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED {
        ErrorKind::NotFound
    } else {
        ErrorKind::Native
    };
    Error::new(kind, format!("{what} failed: {e}"))
}

/// Takes ownership of a COM-allocated string.
///
/// # Safety
/// `p` must be null or a string allocated with CoTaskMemAlloc that the caller
/// owns; it is freed here.
unsafe fn take_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: `p` is a valid NUL-terminated UTF-16 string per the contract.
    let s = unsafe { p.to_string() }.unwrap_or_default();
    // SAFETY: we own the allocation and free it exactly once.
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s
}

#[implement(IAudioSessionNotification)]
struct SessionCreated {
    notifier: Notifier,
}

impl IAudioSessionNotification_Impl for SessionCreated_Impl {
    fn OnSessionCreated(
        &self,
        _session: Ref<'_, IAudioSessionControl>,
    ) -> windows::core::Result<()> {
        self.notifier.notify();
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct SessionChanged {
    notifier: Notifier,
}

impl IAudioSessionEvents_Impl for SessionChanged_Impl {
    fn OnDisplayNameChanged(
        &self,
        _name: &PCWSTR,
        _context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(
        &self,
        _path: &PCWSTR,
        _context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(
        &self,
        _volume: f32,
        _mute: BOOL,
        _context: *const GUID,
    ) -> windows::core::Result<()> {
        self.notifier.notify();
        Ok(())
    }
    fn OnChannelVolumeChanged(
        &self,
        _count: u32,
        _volumes: *const f32,
        _changed: u32,
        _context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(
        &self,
        _param: *const GUID,
        _context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _state: AudioSessionState) -> windows::core::Result<()> {
        self.notifier.notify();
        Ok(())
    }
    fn OnSessionDisconnected(
        &self,
        _reason: AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        self.notifier.notify();
        Ok(())
    }
}

/// One audio session we hold and listen to.
struct WinSession {
    control: IAudioSessionControl,
    volume: ISimpleAudioVolume,
    events: IAudioSessionEvents,
}

impl Drop for WinSession {
    fn drop(&mut self) {
        // SAFETY: `events` was registered on `control` when the session was added.
        let _ = unsafe {
            self.control
                .UnregisterAudioSessionNotification(&self.events)
        };
    }
}

/// One render endpoint's session manager and our registration on it. The
/// inventory drops the endpoint's sessions before it.
struct WinEndpoint {
    notification: IAudioSessionNotification,
    manager: IAudioSessionManager2,
}

impl Drop for WinEndpoint {
    fn drop(&mut self) {
        // SAFETY: `notification` was registered on `manager` in `open_endpoint`.
        let _ = unsafe {
            self.manager
                .UnregisterSessionNotification(&self.notification)
        };
    }
}

/// Balances the worker thread's CoInitializeEx.
struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: created only after CoInitializeEx succeeded on this thread,
        // and dropped on the same thread after every COM object.
        unsafe { CoUninitialize() };
    }
}

/// The COM side of the session inventory.
struct WinPlatform {
    enumerator: IMMDeviceEnumerator,
    notifier: Notifier,
}

/// Field order is drop order: sessions and endpoints (inventory) unregister
/// first, then device controls and the device callback, then the enumerator,
/// and the apartment last.
pub struct WinBackend {
    inventory: Inventory<WinPlatform>,
    device_controls: Vec<DeviceControl>,
    _device_notification: DeviceNotification,
    /// Set by device notifications; consumed by the device snapshot.
    devices_dirty: Arc<AtomicBool>,
    /// Set by device notifications; consumed by the app snapshot.
    sessions_dirty: Arc<AtomicBool>,
    platform: WinPlatform,
    _com: ComApartment,
}

pub fn make() -> MakeBackend {
    Box::new(|notifier| {
        let unavailable = |what: &str, e: windows::core::Error| {
            AppStatus::unavailable(BackendKind::WindowsCoreAudio, format!("{what} failed: {e}"))
        };
        // SAFETY: called once on the worker thread that will own every COM object.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| unavailable("CoInitializeEx", e))?;
        let com = ComApartment;
        // SAFETY: COM is initialised on this thread; MMDeviceEnumerator is the
        // documented in-box class for IMMDeviceEnumerator.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| unavailable("creating MMDeviceEnumerator", e))?;
        let devices_dirty = Arc::new(AtomicBool::new(true));
        let sessions_dirty = Arc::new(AtomicBool::new(true));
        let notification: IMMNotificationClient = DevicesChanged {
            notifier: notifier.clone(),
            devices: Arc::clone(&devices_dirty),
            sessions: Arc::clone(&sessions_dirty),
        }
        .into();
        // SAFETY: registration on the worker; callback only queues a dirty notification.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&notification) }
            .map_err(|e| unavailable("RegisterEndpointNotificationCallback", e))?;
        let device_notification = DeviceNotification {
            enumerator: enumerator.clone(),
            notification,
        };
        let mut backend = WinBackend {
            inventory: Inventory::default(),
            device_controls: Vec::new(),
            _device_notification: device_notification,
            devices_dirty,
            sessions_dirty,
            platform: WinPlatform {
                enumerator,
                notifier,
            },
            _com: com,
        };
        backend
            .inventory
            .sync_endpoints(&mut backend.platform, Instant::now());
        Ok(Box::new(backend) as Box<dyn Backend>)
    })
}

impl Platform for WinPlatform {
    type Endpoint = WinEndpoint;
    type Session = WinSession;
    type Listed = IAudioSessionControl;

    fn render_endpoints(&mut self) -> Result<Vec<String>, Error> {
        // SAFETY: COM calls on the owning thread; returned ids are ours.
        unsafe {
            let endpoints = self
                .enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .map_err(|e| native("EnumAudioEndpoints", e))?;
            let count = endpoints
                .GetCount()
                .map_err(|e| native("IMMDeviceCollection::GetCount", e))?;
            let mut ids = Vec::new();
            for i in 0..count {
                let Ok(endpoint) = endpoints.Item(i) else {
                    continue;
                };
                let Ok(raw_id) = endpoint.GetId() else {
                    continue;
                };
                let id = take_string(raw_id);
                if !id.is_empty() {
                    ids.push(id);
                }
            }
            Ok(ids)
        }
    }

    fn open_endpoint(&mut self, id: &str) -> Result<WinEndpoint, Error> {
        // SAFETY: COM calls on the owning thread with valid arguments.
        unsafe {
            let device = self
                .enumerator
                .GetDevice(&HSTRING::from(id))
                .map_err(|e| device_error("IMMDeviceEnumerator::GetDevice", e))?;
            let manager: IAudioSessionManager2 = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| device_error("Activate(IAudioSessionManager2)", e))?;
            // Session-created notifications only start once
            // IAudioSessionEnumerator::GetCount has been called (documented in
            // IAudioSessionManager2::RegisterSessionNotification).
            manager
                .GetSessionEnumerator()
                .and_then(|list| list.GetCount())
                .map_err(|e| device_error("IAudioSessionEnumerator::GetCount", e))?;
            let notification: IAudioSessionNotification = SessionCreated {
                notifier: self.notifier.clone(),
            }
            .into();
            manager
                .RegisterSessionNotification(&notification)
                .map_err(|e| device_error("RegisterSessionNotification", e))?;
            Ok(WinEndpoint {
                notification,
                manager,
            })
        }
    }

    fn list_sessions(
        &mut self,
        endpoint: &WinEndpoint,
    ) -> Result<Vec<Listed<IAudioSessionControl>>, Error> {
        // SAFETY: COM calls on the owning thread.
        let list = unsafe { endpoint.manager.GetSessionEnumerator() }
            .map_err(|e| device_error("GetSessionEnumerator", e))?;
        // SAFETY: as above.
        let count = unsafe { list.GetCount() }.map_err(|e| device_error("GetCount", e))?;
        let mut listed = Vec::new();
        for i in 0..count {
            // SAFETY: `i` is within the enumerator's count; COM calls on the owning thread.
            let Ok(control) = (unsafe { list.GetSession(i) }) else {
                continue;
            };
            // SAFETY: as above.
            let state = match unsafe { control.GetState() } {
                Ok(s) if s == AudioSessionStateActive => SessionState::Active,
                Ok(s) if s == AudioSessionStateInactive => SessionState::Inactive,
                _ => SessionState::Expired,
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            // SAFETY: as above; the returned string is ours.
            let instance = match unsafe { control2.GetSessionInstanceIdentifier() } {
                Ok(p) => unsafe { take_string(p) },
                Err(_) => continue,
            };
            listed.push(Listed {
                instance,
                state,
                handle: control,
            });
        }
        Ok(listed)
    }

    fn adopt(&mut self, control: IAudioSessionControl) -> Option<Adopted<WinSession>> {
        // SAFETY: we are on the COM worker thread.
        unsafe { adopt(control, &self.notifier) }
    }

    fn read(&mut self, session: &WinSession) -> (f32, bool) {
        // SAFETY: COM calls on the owning thread.
        unsafe {
            (
                session.volume.GetMasterVolume().unwrap_or(1.0),
                session
                    .volume
                    .GetMute()
                    .map(|m| m.as_bool())
                    .unwrap_or(false),
            )
        }
    }

    fn set_volume(&mut self, session: &WinSession, volume: f32) -> Result<(), Error> {
        // SAFETY: COM call on the owning thread; a null event context is allowed.
        unsafe { session.volume.SetMasterVolume(volume, std::ptr::null()) }
            .map_err(|e| device_error("SetMasterVolume", e))
    }

    fn set_mute(&mut self, session: &WinSession, muted: bool) -> Result<(), Error> {
        // SAFETY: COM call on the owning thread; a null event context is allowed.
        unsafe { session.volume.SetMute(muted, std::ptr::null()) }
            .map_err(|e| device_error("SetMute", e))
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeChanged {
    notifier: Notifier,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeChanged_Impl {
    fn OnNotify(&self, _data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        self.notifier.notify();
        Ok(())
    }
}

#[implement(IMMNotificationClient)]
struct DevicesChanged {
    notifier: Notifier,
    devices: Arc<AtomicBool>,
    sessions: Arc<AtomicBool>,
}

impl DevicesChanged_Impl {
    fn changed(&self) {
        self.devices.store(true, Ordering::Release);
        self.sessions.store(true, Ordering::Release);
        self.notifier.notify();
    }
}

impl IMMNotificationClient_Impl for DevicesChanged_Impl {
    fn OnDeviceStateChanged(
        &self,
        _id: &PCWSTR,
        _state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.changed();
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        self.changed();
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        self.changed();
        Ok(())
    }
    fn OnDefaultDeviceChanged(
        &self,
        _flow: EDataFlow,
        _role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        self.changed();
        Ok(())
    }
    fn OnPropertyValueChanged(
        &self,
        _id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        self.changed();
        Ok(())
    }
}

struct DeviceNotification {
    enumerator: IMMDeviceEnumerator,
    notification: IMMNotificationClient,
}
impl Drop for DeviceNotification {
    fn drop(&mut self) {
        // SAFETY: same registration; runs before enumerator/apartment are dropped.
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.notification)
        };
    }
}

struct DeviceControl {
    id: String,
    kind: DeviceKind,
    volume: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
}
impl Drop for DeviceControl {
    fn drop(&mut self) {
        // SAFETY: unregister on owning worker before releasing volume and COM apartment.
        let _ = unsafe { self.volume.UnregisterControlChangeNotify(&self.callback) };
    }
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: all interfaces used on the owning worker, and both returned
    // allocations (PROPVARIANT and allocated UTF-16) are released on every path.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let string = PropVariantToStringAlloc(&value)
            .ok()
            .map(|p| take_string(p));
        let _ = PropVariantClear(&mut value);
        string.filter(|s| !s.is_empty())
    }
}

impl WinBackend {
    fn device_control(&self, id: &str, kind: DeviceKind) -> Result<&IAudioEndpointVolume, Error> {
        self.device_controls
            .iter()
            .find(|d| d.id == id && d.kind == kind)
            .map(|d| &d.volume)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "audio endpoint disappeared"))
    }

    fn device_snapshot(&mut self) -> Result<Vec<Device>, Error> {
        let mut previous = std::mem::take(&mut self.device_controls);
        if self.devices_dirty.swap(false, Ordering::AcqRel) {
            previous.clear();
        }
        let mut controls = Vec::new();
        let mut devices = Vec::new();
        // SAFETY: every COM call here is on the MTA worker. Returned strings
        // are owned and freed by take_string; interfaces use RAII.
        let result = unsafe {
            (|| {
                for (kind, flow) in [(DeviceKind::Output, eRender), (DeviceKind::Input, eCapture)] {
                    let default = self
                        .platform
                        .enumerator
                        .GetDefaultAudioEndpoint(flow, eConsole)
                        .ok()
                        .and_then(|d| d.GetId().ok())
                        .map(|p| take_string(p));
                    let endpoints = self
                        .platform
                        .enumerator
                        .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
                        .map_err(|e| native("EnumAudioEndpoints", e))?;
                    let count = endpoints
                        .GetCount()
                        .map_err(|e| native("IMMDeviceCollection::GetCount", e))?;
                    for i in 0..count {
                        let Ok(endpoint) = endpoints.Item(i) else {
                            continue;
                        };
                        let Ok(raw_id) = endpoint.GetId() else {
                            continue;
                        };
                        let id = take_string(raw_id);
                        if !valid_device_id(&id) {
                            continue;
                        }
                        let name = friendly_name(&endpoint).unwrap_or_else(|| id.clone());
                        let control =
                            match previous.iter().position(|c| c.id == id && c.kind == kind) {
                                Some(at) => Some(previous.swap_remove(at)),
                                None => endpoint
                                    .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
                                    .ok()
                                    .and_then(|volume| {
                                        let callback: IAudioEndpointVolumeCallback =
                                            VolumeChanged {
                                                notifier: self.platform.notifier.clone(),
                                            }
                                            .into();
                                        volume.RegisterControlChangeNotify(&callback).ok()?;
                                        Some(DeviceControl {
                                            id: id.clone(),
                                            kind,
                                            volume,
                                            callback,
                                        })
                                    }),
                            };
                        let volume = control
                            .as_ref()
                            .and_then(|c| c.volume.GetMasterVolumeLevelScalar().ok());
                        let muted = control.as_ref().and_then(|c| c.volume.GetMute().ok());
                        devices.push(Device {
                            is_default: default.as_ref() == Some(&id),
                            id,
                            name,
                            kind,
                            volume: volume.unwrap_or(0.0).clamp(0.0, 1.0),
                            muted: muted.is_some_and(|v| v.as_bool()),
                            can_set_volume: volume.is_some(),
                            can_mute: muted.is_some(),
                            emulated: false,
                        });
                        if let Some(control) = control {
                            controls.push(control);
                        }
                    }
                }
                Ok(())
            })()
        };
        self.device_controls = controls;
        result?;
        devices.sort_by(|a, b| a.id.cmp(&b.id).then(a.kind.cmp(&b.kind)));
        Ok(devices)
    }
}

/// Executable path of a process, or None if it is gone or inaccessible.
fn image_path(pid: u32) -> Option<String> {
    // SAFETY: OpenProcess/QueryFullProcessImageNameW with a buffer we own and
    // its exact length; the handle is closed before returning.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Wraps a newly seen session: identifies its app and subscribes to its events.
///
/// # Safety
/// Must run on the COM worker thread.
unsafe fn adopt(control: IAudioSessionControl, notifier: &Notifier) -> Option<Adopted<WinSession>> {
    // SAFETY: COM calls on the owning thread.
    unsafe {
        let control2: IAudioSessionControl2 = control.cast().ok()?;
        let volume: ISimpleAudioVolume = control.cast().ok()?;
        let (app_id, name) = if control2.IsSystemSoundsSession() == S_OK {
            ("system".to_string(), "System sounds".to_string())
        } else {
            let pid = control2.GetProcessId().ok()?;
            match image_path(pid) {
                Some(path) => (windows_app_id(&path), windows_app_name(&path)),
                None => (slugify(&format!("pid:{pid}")), format!("Process {pid}")),
            }
        };
        let events: IAudioSessionEvents = SessionChanged {
            notifier: notifier.clone(),
        }
        .into();
        control.RegisterAudioSessionNotification(&events).ok()?;
        Some(Adopted {
            app_id,
            name,
            session: WinSession {
                control,
                volume,
                events,
            },
        })
    }
}

impl Backend for WinBackend {
    fn devices(&mut self) -> Result<Vec<Device>, Error> {
        self.device_snapshot()
    }
    fn set_device_volume(&mut self, id: &str, kind: DeviceKind, volume: f32) -> Result<(), Error> {
        let control = self.device_control(id, kind)?;
        // SAFETY: COM endpoint belongs to this worker; null event context allowed.
        unsafe { control.SetMasterVolumeLevelScalar(volume, std::ptr::null()) }
            .map_err(|e| device_error("SetMasterVolumeLevelScalar", e))
    }
    fn set_device_mute(&mut self, id: &str, kind: DeviceKind, muted: bool) -> Result<(), Error> {
        let control = self.device_control(id, kind)?;
        // SAFETY: COM endpoint belongs to this worker; null event context allowed.
        unsafe { control.SetMute(muted, std::ptr::null()) }
            .map_err(|e| device_error("IAudioEndpointVolume::SetMute", e))
    }
    fn status(&self) -> AppStatus {
        if self.inventory.available() {
            AppStatus {
                backend: BackendKind::WindowsCoreAudio,
                available: true,
                can_set_volume: true,
                can_mute: true,
                permission: Permission::Granted,
                message: String::new(),
            }
        } else {
            AppStatus::unavailable(
                BackendKind::WindowsCoreAudio,
                self.inventory.unavailable_reason(),
            )
        }
    }

    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn snapshot(&mut self) -> Result<Vec<App>, Error> {
        if self.sessions_dirty.swap(false, Ordering::AcqRel) {
            self.inventory.invalidate();
        }
        self.inventory.snapshot(&mut self.platform, Instant::now())
    }

    /// Every session of the app, on every endpoint, gets the new value.
    fn set_volume(&mut self, app: &str, volume: f32) -> Result<(), Error> {
        self.inventory.set_volume(&mut self.platform, app, volume)
    }

    fn set_mute(&mut self, app: &str, muted: bool) -> Result<(), Error> {
        self.inventory.set_mute(&mut self.platform, app, muted)
    }
}
