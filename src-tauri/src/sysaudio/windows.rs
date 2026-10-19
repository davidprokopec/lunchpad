//! Windows backend: Core Audio sessions on the default render endpoint.
//!
//! Every COM object lives on the engine's worker thread, which initialises COM
//! as multithreaded (MTA). API calls reach it through the engine's command
//! channel. Session notifications arrive on COM worker threads; they only ask
//! the worker for a refresh (`Notifier`), they never touch session objects.
//!
//! Sessions are grouped by executable file name. The system-sounds session is
//! exposed as the app `system`.

use std::ffi::c_void;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, Ref, BOOL, GUID, PCWSTR, PWSTR};
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
    AudioSessionStateExpired, IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents,
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
    group, slugify, valid_device_id, windows_app_id, windows_app_name, App, Device, DeviceKind,
    Member,
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How often the default endpoint is re-checked (device switch, unplug).
const ENDPOINT_CHECK: Duration = Duration::from_secs(2);
const NO_ENDPOINT: &str = "no default audio output device";

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
struct Session {
    instance: String,
    app_id: String,
    name: String,
    control: IAudioSessionControl,
    volume: ISimpleAudioVolume,
    events: IAudioSessionEvents,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: `events` was registered on `control` when the session was added.
        let _ = unsafe {
            self.control
                .UnregisterAudioSessionNotification(&self.events)
        };
    }
}

/// The default render endpoint's session manager and our registrations.
/// Field order is drop order: sessions unregister before the manager goes.
struct Endpoint {
    device_id: String,
    sessions: Vec<Session>,
    notification: IAudioSessionNotification,
    manager: IAudioSessionManager2,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.sessions.clear();
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

/// Field order is drop order: COM objects before the apartment.
pub struct WinBackend {
    endpoint: Option<Endpoint>,
    device_controls: Vec<DeviceControl>,
    _device_notification: DeviceNotification,
    topology_dirty: Arc<AtomicBool>,
    enumerator: IMMDeviceEnumerator,
    notifier: Notifier,
    last_check: Instant,
    last_error: String,
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
        let topology_dirty = Arc::new(AtomicBool::new(true));
        let notification: IMMNotificationClient = DevicesChanged {
            notifier: notifier.clone(),
            dirty: Arc::clone(&topology_dirty),
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
            endpoint: None,
            device_controls: Vec::new(),
            _device_notification: device_notification,
            topology_dirty,
            enumerator,
            notifier,
            last_check: Instant::now(),
            last_error: NO_ENDPOINT.into(),
            _com: com,
        };
        backend.reopen();
        Ok(Box::new(backend) as Box<dyn Backend>)
    })
}

impl WinBackend {
    fn default_device_id(&self) -> Result<String, Error> {
        // SAFETY: plain COM calls on the owning thread; the id string is ours.
        unsafe {
            let device = self
                .enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| native("GetDefaultAudioEndpoint", e))?;
            Ok(take_string(
                device.GetId().map_err(|e| native("IMMDevice::GetId", e))?,
            ))
        }
    }

    fn open_endpoint(&self) -> Result<Endpoint, Error> {
        // SAFETY: COM calls on the owning thread with valid arguments.
        unsafe {
            let device = self
                .enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| native("GetDefaultAudioEndpoint", e))?;
            let device_id = take_string(device.GetId().map_err(|e| native("IMMDevice::GetId", e))?);
            let manager: IAudioSessionManager2 = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| native("Activate(IAudioSessionManager2)", e))?;
            // Session-created notifications only start once the session list
            // has been enumerated at least once (documented quirk).
            manager
                .GetSessionEnumerator()
                .map_err(|e| native("GetSessionEnumerator", e))?;
            let notification: IAudioSessionNotification = SessionCreated {
                notifier: self.notifier.clone(),
            }
            .into();
            manager
                .RegisterSessionNotification(&notification)
                .map_err(|e| native("RegisterSessionNotification", e))?;
            Ok(Endpoint {
                device_id,
                sessions: Vec::new(),
                notification,
                manager,
            })
        }
    }

    /// (Re)opens the default endpoint; on failure the backend is unavailable
    /// until a later check finds one.
    fn reopen(&mut self) {
        self.endpoint = None;
        match self.open_endpoint() {
            Ok(endpoint) => self.endpoint = Some(endpoint),
            Err(e) => self.last_error = format!("{NO_ENDPOINT} ({})", e.message),
        }
        self.last_check = Instant::now();
    }

    fn check_endpoint(&mut self) {
        if self.endpoint.is_some() && self.last_check.elapsed() < ENDPOINT_CHECK {
            return;
        }
        let current = self.default_device_id().ok();
        let open = self.endpoint.as_ref().map(|e| e.device_id.clone());
        if current.is_none() || current != open {
            self.reopen();
        } else {
            self.last_check = Instant::now();
        }
    }

    fn sessions_of(&self, app: &str) -> Result<Vec<&ISimpleAudioVolume>, Error> {
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, NO_ENDPOINT))?;
        let volumes: Vec<_> = endpoint
            .sessions
            .iter()
            .filter(|s| s.app_id == app)
            .map(|s| &s.volume)
            .collect();
        if volumes.is_empty() {
            return Err(Error::new(
                ErrorKind::NotFound,
                "the app has no audio session",
            ));
        }
        Ok(volumes)
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
    dirty: Arc<AtomicBool>,
}

impl DevicesChanged_Impl {
    fn changed(&self) {
        self.dirty.store(true, Ordering::Release);
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
        if self.topology_dirty.swap(false, Ordering::AcqRel) {
            previous.clear();
            // Session endpoint switching should also react immediately.
            self.last_check = Instant::now() - ENDPOINT_CHECK;
        }
        let mut controls = Vec::new();
        let mut devices = Vec::new();
        // SAFETY: every COM call here is on the MTA worker. Returned strings
        // are owned and freed by take_string; interfaces use RAII.
        let result = unsafe {
            (|| {
                for (kind, flow) in [(DeviceKind::Output, eRender), (DeviceKind::Input, eCapture)] {
                    let default = self
                        .enumerator
                        .GetDefaultAudioEndpoint(flow, eConsole)
                        .ok()
                        .and_then(|d| d.GetId().ok())
                        .map(|p| take_string(p));
                    let endpoints = self
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
                                                notifier: self.notifier.clone(),
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
unsafe fn adopt(
    control: IAudioSessionControl,
    instance: String,
    notifier: &Notifier,
) -> Option<Session> {
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
        Some(Session {
            instance,
            app_id,
            name,
            control,
            volume,
            events,
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
        match self.endpoint {
            Some(_) => AppStatus {
                backend: BackendKind::WindowsCoreAudio,
                available: true,
                can_set_volume: true,
                can_mute: true,
                permission: Permission::Granted,
                message: String::new(),
            },
            None => AppStatus::unavailable(BackendKind::WindowsCoreAudio, self.last_error.clone()),
        }
    }

    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn snapshot(&mut self) -> Result<Vec<App>, Error> {
        self.check_endpoint();
        let notifier = self.notifier.clone();
        let Some(endpoint) = self.endpoint.as_mut() else {
            return Ok(Vec::new());
        };
        // SAFETY: COM calls on the owning thread.
        let list = unsafe { endpoint.manager.GetSessionEnumerator() }
            .map_err(|e| native("GetSessionEnumerator", e))?;
        // SAFETY: as above.
        let count = unsafe { list.GetCount() }.map_err(|e| native("GetCount", e))?;
        let mut previous = std::mem::take(&mut endpoint.sessions);
        let mut members = Vec::new();
        for i in 0..count {
            // SAFETY: `i` is within the enumerator's count; COM calls on the owning thread.
            let Ok(control) = (unsafe { list.GetSession(i) }) else {
                continue;
            };
            // SAFETY: as above.
            let state = unsafe { control.GetState() }.unwrap_or(AudioSessionStateExpired);
            if state == AudioSessionStateExpired {
                continue;
            }
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            // SAFETY: as above; the returned string is ours.
            let instance = match unsafe { control2.GetSessionInstanceIdentifier() } {
                Ok(p) => unsafe { take_string(p) },
                Err(_) => continue,
            };
            let session = match previous.iter().position(|s| s.instance == instance) {
                Some(at) => previous.swap_remove(at),
                // SAFETY: we are on the COM worker thread.
                None => match unsafe { adopt(control, instance, &notifier) } {
                    Some(session) => session,
                    None => continue,
                },
            };
            // SAFETY: COM calls on the owning thread.
            let (volume, muted) = unsafe {
                (
                    session.volume.GetMasterVolume().unwrap_or(1.0),
                    session
                        .volume
                        .GetMute()
                        .map(|m| m.as_bool())
                        .unwrap_or(false),
                )
            };
            members.push(Member {
                app_id: session.app_id.clone(),
                name: session.name.clone(),
                active: state == AudioSessionStateActive,
                volume: volume.clamp(0.0, 1.0),
                muted,
            });
            endpoint.sessions.push(session);
        }
        // Sessions no longer listed are dropped here, which unregisters them.
        drop(previous);
        Ok(group(members))
    }

    /// Grouped sessions may disagree; every one of them gets the new value.
    fn set_volume(&mut self, app: &str, volume: f32) -> Result<(), Error> {
        for session in self.sessions_of(app)? {
            // SAFETY: COM call on the owning thread; a null event context is allowed.
            unsafe { session.SetMasterVolume(volume, std::ptr::null()) }
                .map_err(|e| native("SetMasterVolume", e))?;
        }
        Ok(())
    }

    fn set_mute(&mut self, app: &str, muted: bool) -> Result<(), Error> {
        for session in self.sessions_of(app)? {
            // SAFETY: COM call on the owning thread; a null event context is allowed.
            unsafe { session.SetMute(muted, std::ptr::null()) }
                .map_err(|e| native("SetMute", e))?;
        }
        Ok(())
    }
}
