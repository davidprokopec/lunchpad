//! macOS backend.
//!
//! Devices: every Core Audio device with streams, per direction, with real
//! volume and mute properties (`kAudioDevicePropertyVolumeScalar` /
//! `kAudioDevicePropertyMute`, input scope for microphones). Property listener
//! blocks on the system object (device list, default output, default input)
//! and on each device's controls only ask the worker for a refresh.
//!
//! Apps come from Core Audio process objects (macOS 14.2+), which need no TCC
//! permission. App volume and mute use process taps and are only enabled with
//! the `audio.macosAppVolume` setting (see `macos_taps`).
//!
//! Core Audio declarations are hand-written from the SDK headers
//! (AudioHardware.h, AudioHardwareBase.h).

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::time::Duration;

use super::engine::Notifier;
use super::model::{valid_device_id, Device, DeviceKind};
use block2::RcBlock;
use objc2::rc::{autoreleasepool, Retained};
use objc2_foundation::{NSBundle, NSString};

use super::engine::{AppStatus, Backend, BackendKind, Error, ErrorKind, MakeBackend, Permission};
use super::macos_taps::Taps;
use super::model::{
    app_bundle_name, basename, group, is_xpc_service, outermost_app, slugify, App, Member,
};
use super::Options;

pub type ObjId = u32;
pub type OSStatus = i32;

const fn fcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

pub const SYSTEM_OBJECT: ObjId = 1;
pub const SCOPE_GLOBAL: u32 = fcc(b"glob");
pub const HW_PROCESS_LIST: u32 = fcc(b"prs#");
pub const HW_DEFAULT_OUTPUT: u32 = fcc(b"dOut");
pub const PROC_PID: u32 = fcc(b"ppid");
pub const PROC_BUNDLE_ID: u32 = fcc(b"pbid");
pub const PROC_RUNNING_OUTPUT: u32 = fcc(b"piro");
pub const DEV_UID: u32 = fcc(b"uid ");
pub const TAP_FORMAT: u32 = fcc(b"tfmt");
const HW_DEVICES: u32 = fcc(b"dev#");
const HW_DEFAULT_INPUT: u32 = fcc(b"dIn ");
const DEV_NAME: u32 = fcc(b"lnam");
const DEV_STREAMS: u32 = fcc(b"stm#");
const DEV_STREAM_CONFIG: u32 = fcc(b"slay");
const DEV_VOLUME: u32 = fcc(b"volm");
const DEV_MUTE: u32 = fcc(b"mute");
const SCOPE_OUTPUT: u32 = fcc(b"outp");
const SCOPE_INPUT: u32 = fcc(b"inpt");

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PropAddr {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectIsPropertySettable(o: ObjId, a: *const PropAddr, out: *mut u8) -> OSStatus;
    fn AudioObjectSetPropertyData(
        o: ObjId,
        a: *const PropAddr,
        qs: u32,
        q: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> OSStatus;
    fn AudioObjectAddPropertyListenerBlock(
        o: ObjId,
        a: *const PropAddr,
        queue: *const c_void,
        block: &block2::Block<dyn Fn(u32, *const c_void)>,
    ) -> OSStatus;
    fn AudioObjectRemovePropertyListenerBlock(
        o: ObjId,
        a: *const PropAddr,
        queue: *const c_void,
        block: &block2::Block<dyn Fn(u32, *const c_void)>,
    ) -> OSStatus;
    fn AudioObjectGetPropertyDataSize(
        o: ObjId,
        a: *const PropAddr,
        qs: u32,
        q: *const c_void,
        out: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
        o: ObjId,
        a: *const PropAddr,
        qs: u32,
        q: *const c_void,
        io: *mut u32,
        out: *mut c_void,
    ) -> OSStatus;
}

extern "C" {
    // libproc (libSystem), public: <libproc.h>.
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, size: u32) -> c_int;
    fn getpid() -> c_int;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// `RTLD_DEFAULT` from <dlfcn.h> on Darwin.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

#[derive(Debug)]
pub struct CaError {
    pub what: &'static str,
    pub status: OSStatus,
}

impl CaError {
    pub fn into_error(self, kind: ErrorKind) -> Error {
        Error::new(
            kind,
            format!("{} failed: OSStatus {}", self.what, self.status),
        )
    }
}

pub type CaResult<T> = Result<T, CaError>;

pub fn check(what: &'static str, status: OSStatus) -> CaResult<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(CaError { what, status })
    }
}

fn addr(selector: u32) -> PropAddr {
    PropAddr {
        selector,
        scope: SCOPE_GLOBAL,
        element: 0,
    }
}

/// Fixed-size property read. `T` must be the plain-data type the HAL writes
/// for `selector` (u32/i32/f64/struct), never a reference-counted object.
pub fn get<T: Copy + Default>(obj: ObjId, selector: u32) -> CaResult<T> {
    let mut value = T::default();
    let mut size = size_of::<T>() as u32;
    // SAFETY: `value` is a valid, writable `T` of `size` bytes; the HAL writes
    // at most `size` bytes and no qualifier is passed.
    let status = unsafe {
        AudioObjectGetPropertyData(
            obj,
            &addr(selector),
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut T).cast(),
        )
    };
    check("AudioObjectGetPropertyData", status).map(|_| value)
}

/// Reads a property holding an array of object ids.
pub fn get_ids(obj: ObjId, selector: u32) -> CaResult<Vec<ObjId>> {
    let a = addr(selector);
    let mut size = 0u32;
    // SAFETY: valid address struct and out pointer; no qualifier.
    check("AudioObjectGetPropertyDataSize", unsafe {
        AudioObjectGetPropertyDataSize(obj, &a, 0, std::ptr::null(), &mut size)
    })?;
    let mut ids = vec![0u32; size as usize / 4];
    let mut size = (ids.len() * 4) as u32;
    // SAFETY: `ids` is writable for `size` bytes; the HAL updates `size` to the
    // bytes written (the list may shrink between the two calls).
    check("AudioObjectGetPropertyData", unsafe {
        AudioObjectGetPropertyData(
            obj,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr().cast(),
        )
    })?;
    ids.truncate(size as usize / 4);
    Ok(ids)
}

/// Reads a CFString property. The HAL returns a +1 reference, which we adopt.
pub fn get_string(obj: ObjId, selector: u32) -> CaResult<Option<String>> {
    let raw: usize = get(obj, selector)?;
    if raw == 0 {
        return Ok(None);
    }
    // SAFETY: the HAL hands out an owned (+1) CFStringRef for this property;
    // CFString is toll-free bridged to NSString, and `from_raw` takes over the
    // single reference so it is released exactly once.
    let s: Option<Retained<NSString>> = unsafe { Retained::from_raw(raw as *mut NSString) };
    Ok(s.map(|s| s.to_string()))
}

fn pid_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: `buf` is writable for its full length, which we pass as `size`.
    let len = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    String::from_utf8(buf).ok()
}

type ResponsibleFn = unsafe extern "C" fn(c_int) -> c_int;

/// `responsibility_get_pid_responsible_for_pid` is private libSystem SPI (the
/// attribution Activity Monitor and TCC use). Looked up at runtime so its
/// absence only disables helper grouping for XPC services.
fn responsible_fn() -> Option<ResponsibleFn> {
    // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name is always safe.
    let sym = unsafe {
        dlsym(
            RTLD_DEFAULT,
            c"responsibility_get_pid_responsible_for_pid".as_ptr(),
        )
    };
    if sym.is_null() {
        return None;
    }
    // SAFETY: the symbol's C signature is `pid_t (pid_t)`; a non-null data
    // pointer from dlsym for a function symbol is its entry address.
    Some(unsafe { std::mem::transmute::<*mut c_void, ResponsibleFn>(sym) })
}

/// Who an app bundle says it is: its bundle id (or a path slug) and name.
fn bundle_identity(bundle_path: &str) -> (String, String) {
    let ns_path = NSString::from_str(bundle_path);
    let id = NSBundle::bundleWithPath(&ns_path)
        .and_then(|b| b.bundleIdentifier())
        .map(|s| s.to_string());
    let name = app_bundle_name(bundle_path);
    (slugify(&id.unwrap_or_else(|| format!("app:{name}"))), name)
}

#[derive(Clone)]
struct Identity {
    pid: i32,
    app_id: String,
    name: String,
}

/// Attributes a process object to an app:
/// 1. its own outermost `.app` (Chrome/Electron helpers group under the app);
/// 2. for XPC services (Safari's `com.apple.WebKit.GPU`), the `.app` of the
///    responsible process;
/// 3. its bundle id, or `proc:<executable>`.
fn resolve(obj: ObjId, responsible: Option<ResponsibleFn>) -> Option<Identity> {
    let pid: i32 = get(obj, PROC_PID).ok()?;
    let path = pid_path(pid);
    if let Some(bundle) = path.as_deref().and_then(outermost_app) {
        let (app_id, name) = bundle_identity(bundle);
        return Some(Identity { pid, app_id, name });
    }
    if let (Some(path), Some(responsible)) = (path.as_deref(), responsible) {
        if is_xpc_service(path) {
            // SAFETY: plain C call taking and returning a pid.
            let owner = unsafe { responsible(pid) };
            if owner > 0 && owner != pid {
                if let Some(bundle) = pid_path(owner).as_deref().and_then(outermost_app) {
                    let (app_id, name) = bundle_identity(bundle);
                    return Some(Identity { pid, app_id, name });
                }
            }
        }
    }
    if let Ok(Some(bundle_id)) = get_string(obj, PROC_BUNDLE_ID) {
        if !bundle_id.is_empty() {
            let name = bundle_id
                .rsplit('.')
                .next()
                .unwrap_or(&bundle_id)
                .to_string();
            return Some(Identity {
                pid,
                app_id: slugify(&bundle_id),
                name,
            });
        }
    }
    let exe = path.as_deref().map(basename).unwrap_or("");
    let name = if exe.is_empty() {
        format!("pid {pid}")
    } else {
        exe.to_string()
    };
    Some(Identity {
        pid,
        app_id: slugify(&format!("proc:{name}")),
        name,
    })
}

pub struct MacBackend {
    process_available: bool,
    own_pid: i32,
    responsible: Option<ResponsibleFn>,
    identities: HashMap<ObjId, Identity>,
    /// Process objects of each app in the latest snapshot.
    members: HashMap<String, Vec<ObjId>>,
    taps: Option<Taps>,
    devices: DeviceAudio,
}

pub fn make(options: Options) -> MakeBackend {
    Box::new(move |notifier| {
        let process_available = get_ids(SYSTEM_OBJECT, HW_PROCESS_LIST).is_ok();
        tracing::info!(requested_opt_in = options.macos_taps, taps_supported = process_available,
            apps_available = process_available, can_set_app_volume = process_available && options.macos_taps,
            can_mute_apps = process_available && options.macos_taps, permission = ?Permission::Unknown,
            "app audio service start");
        Ok(Box::new(MacBackend {
            process_available,
            // SAFETY: getpid has no preconditions.
            own_pid: unsafe { getpid() },
            responsible: responsible_fn(),
            identities: HashMap::new(),
            members: HashMap::new(),
            taps: options.macos_taps.then(Taps::new),
            devices: DeviceAudio::new(notifier),
        }))
    })
}

impl Backend for MacBackend {
    fn configure(&mut self, options: Options) -> Result<(), Error> {
        tracing::info!(
            requested_opt_in = options.macos_taps,
            taps_supported = self.process_available,
            "app audio configure requested"
        );
        if !options.macos_taps {
            tracing::info!("app audio configure off: removing tap manager and controls");
            // Drop all controls on their owning worker; leave listing enabled.
            self.taps = None;
        } else if self.taps.is_none() && self.process_available {
            // Taps::new creates no native tap. Only an explicit setter probes.
            self.taps = Some(Taps::new());
        }
        let status = self.status();
        tracing::info!(apps_available = status.available, can_set_app_volume = status.can_set_volume,
            can_mute_apps = status.can_mute, permission = ?status.permission, "app audio configure applied");
        Ok(())
    }
    fn devices(&mut self) -> Result<Vec<Device>, Error> {
        self.devices.snapshot()
    }
    fn set_device_volume(&mut self, id: &str, kind: DeviceKind, volume: f32) -> Result<(), Error> {
        self.devices.set(id, kind, DEV_VOLUME, volume)
    }
    fn set_device_mute(&mut self, id: &str, kind: DeviceKind, muted: bool) -> Result<(), Error> {
        self.devices.set(id, kind, DEV_MUTE, u32::from(muted))
    }
    fn status(&self) -> AppStatus {
        if !self.process_available {
            return AppStatus::unavailable(BackendKind::MacosTap, "Core Audio process objects require macOS 14.2 or later; device audio remains available");
        }
        match &self.taps {
            None => AppStatus {
                backend: BackendKind::MacosTap,
                available: true,
                can_set_volume: false,
                can_mute: false,
                permission: Permission::Unknown,
                message: "Listing only: per-app volume needs process taps (turn on audio.macosAppVolume in the settings)".into(),
            },
            Some(taps) => AppStatus {
                backend: BackendKind::MacosTap,
                available: true,
                can_set_volume: true,
                can_mute: true,
                permission: taps.permission(),
                message: match (taps.problem(), taps.permission()) {
                    (Some(problem), _) => problem.to_string(),
                    (None, Permission::Granted) => "Experimental process taps enabled; capture/revocation/output-switch safety requires user-present verification".into(),
                    (None, Permission::Unconfirmed) => "The System Audio Recording permission could not be confirmed: play audio in the app, then retry".into(),
                    (None, _) => "Process taps need the System Audio Recording permission; it is confirmed on the first change".into(),
                },
            },
        }
    }

    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn snapshot(&mut self) -> Result<Vec<App>, Error> {
        if !self.process_available {
            return Ok(Vec::new());
        }
        autoreleasepool(|_| {
            let objects = match &mut self.taps {
                Some(taps) => taps.process_list(),
                None => get_ids(SYSTEM_OBJECT, HW_PROCESS_LIST)
                    .map_err(|e| e.into_error(ErrorKind::Native)),
            }?;
            self.identities.retain(|obj, _| objects.contains(obj));
            let mut members = Vec::with_capacity(objects.len());
            let mut by_app: HashMap<String, Vec<ObjId>> = HashMap::new();
            for obj in objects {
                let identity = match self.identities.get(&obj) {
                    Some(identity) => identity.clone(),
                    None => {
                        let Some(identity) = resolve(obj, self.responsible) else {
                            continue;
                        };
                        self.identities.insert(obj, identity.clone());
                        identity
                    }
                };
                if identity.pid == self.own_pid {
                    continue;
                }
                let active = match &mut self.taps {
                    Some(taps) => taps.active(obj),
                    None => get::<u32>(obj, PROC_RUNNING_OUTPUT)
                        .map(|v| v != 0)
                        .map_err(|e| e.into_error(ErrorKind::Native)),
                }?;
                by_app.entry(identity.app_id.clone()).or_default().push(obj);
                members.push(Member {
                    app_id: identity.app_id,
                    name: identity.name,
                    active,
                    volume: 1.0,
                    muted: false,
                });
            }
            // Sorted, so a reordered process list is not taken for a change.
            for processes in by_app.values_mut() {
                processes.sort_unstable();
            }
            if let Some(taps) = &mut self.taps {
                // May rebuild or forget taps (process set, output, silence),
                // so settings are read after it.
                taps.maintain(&by_app);
                taps.apply_settings(&mut members);
            }
            self.members = by_app;
            Ok(group(members))
        })
    }

    fn set_volume(&mut self, app: &str, volume: f32) -> Result<(), Error> {
        tracing::info!(app, requested_volume = volume, "app audio volume requested");
        let (taps, processes) = self.tap_target(app)?;
        autoreleasepool(|_| taps.set(app, &processes, Some(volume), None))
    }

    fn set_mute(&mut self, app: &str, muted: bool) -> Result<(), Error> {
        tracing::info!(app, requested_mute = muted, "app audio mute requested");
        let (taps, processes) = self.tap_target(app)?;
        autoreleasepool(|_| taps.set(app, &processes, None, Some(muted)))
    }
}

impl MacBackend {
    fn tap_target(&mut self, app: &str) -> Result<(&mut Taps, Vec<ObjId>), Error> {
        let processes = self.members.get(app).cloned().unwrap_or_default();
        tracing::info!(app, ?processes, "app audio setter resolved process objects");
        for object in &processes {
            let pid = get::<i32>(*object, PROC_PID);
            let path = pid.as_ref().ok().and_then(|pid| pid_path(*pid));
            let running_output = get::<u32>(*object, PROC_RUNNING_OUTPUT).map(|v| v != 0);
            tracing::info!(app, object, ?pid, executable_path = ?path, ?running_output,
                "app audio setter process identity");
        }
        let taps = self
            .taps
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, "process taps are disabled"))?;
        Ok((taps, processes))
    }

    /// Whether the given app is producing audio right now (any member process).
    /// Property errors remain unknown/error, never evidence of an idle app.
    pub fn is_running_output(processes: &[ObjId]) -> Result<bool, Error> {
        processes.iter().try_fold(false, |active, p| {
            get::<u32>(*p, PROC_RUNNING_OUTPUT)
                .map(|v| active || v != 0)
                .map_err(|e| e.into_error(ErrorKind::Native))
        })
    }
}

/// Diagnostic reads only: failures must not affect construction or routing.
pub(super) fn log_aggregate_streams(object: ObjId) {
    for (direction, scope) in [("input", SCOPE_INPUT), ("output", SCOPE_OUTPUT)] {
        let address = PropAddr {
            selector: DEV_STREAMS,
            scope,
            element: 0,
        };
        let mut size = 0;
        // SAFETY: valid address and writable size, with no qualifier.
        let result = check("diagnostic stream list size", unsafe {
            AudioObjectGetPropertyDataSize(object, &address, 0, std::ptr::null(), &mut size)
        })
        .and_then(|_| {
            let mut streams = vec![0u32; size as usize / size_of::<ObjId>()];
            // SAFETY: the vector has space for the requested stream object ids.
            check("diagnostic stream list", unsafe {
                AudioObjectGetPropertyData(
                    object,
                    &address,
                    0,
                    std::ptr::null(),
                    &mut size,
                    streams.as_mut_ptr().cast(),
                )
            })?;
            streams.truncate(size as usize / size_of::<ObjId>());
            Ok(streams)
        });
        match result {
            Ok(streams) => {
                tracing::info!(
                    aggregate = object,
                    direction,
                    ?streams,
                    "app audio aggregate streams (HAL order)"
                );
                for (index, stream) in streams.into_iter().enumerate() {
                    tracing::info!(aggregate = object, stream, direction, index,
                        starting_channel = ?get::<u32>(stream, fcc(b"schn")), "app audio aggregate stream channel mapping");
                    match get::<super::macos_taps::Asbd>(stream, fcc(b"sfmt")) {
                        Ok(format) => format.log(stream, direction, index),
                        Err(error) => tracing::warn!(
                            aggregate = object,
                            stream,
                            direction,
                            index,
                            ?error,
                            "app audio stream format unavailable"
                        ),
                    }
                }
            }
            Err(error) => tracing::warn!(
                aggregate = object,
                direction,
                ?error,
                "app audio stream list unavailable"
            ),
        }
    }
}

// The HAL copies listener blocks, including their owned Notifier. An in-flight
// callback therefore keeps its context alive even after removal/worker drop.
struct DeviceListener {
    object: ObjId,
    address: PropAddr,
    block: RcBlock<dyn Fn(u32, *const c_void)>,
}

impl Drop for DeviceListener {
    fn drop(&mut self) {
        // SAFETY: identical object/address/queue/block to the registration.
        // If removal fails the HAL still owns a copy, never a dangling context.
        let _ = unsafe {
            AudioObjectRemovePropertyListenerBlock(
                self.object,
                &self.address,
                std::ptr::null(),
                &self.block,
            )
        };
    }
}

struct DeviceAudio {
    notifier: Notifier,
    listeners: Vec<DeviceListener>,
    targets: Vec<(String, DeviceKind, ObjId)>,
}

fn scope(kind: DeviceKind) -> u32 {
    match kind {
        DeviceKind::Output => SCOPE_OUTPUT,
        DeviceKind::Input => SCOPE_INPUT,
    }
}

fn scoped_get<T: Copy + Default>(object: ObjId, address: &PropAddr) -> CaResult<T> {
    let mut value = T::default();
    let mut size = size_of::<T>() as u32;
    // SAFETY: plain property T, with a writable buffer of its exact size.
    check("AudioObjectGetPropertyData", unsafe {
        AudioObjectGetPropertyData(
            object,
            address,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut T).cast(),
        )
    })?;
    Ok(value)
}

fn settable(object: ObjId, address: &PropAddr) -> bool {
    let mut settable = 0u8;
    // SAFETY: valid address and Boolean output.
    unsafe { AudioObjectIsPropertySettable(object, address, &mut settable) == 0 && settable != 0 }
}

fn channel_count(object: ObjId, kind: DeviceKind) -> u32 {
    let address = PropAddr {
        selector: DEV_STREAM_CONFIG,
        scope: scope(kind),
        element: 0,
    };
    let mut size = 0;
    // SAFETY: valid address and output size.
    if unsafe { AudioObjectGetPropertyDataSize(object, &address, 0, std::ptr::null(), &mut size) }
        != 0
    {
        return 0;
    }
    // AudioBufferList: u32 count, padding, then AudioBuffer {u32,u32,void*}.
    let mut words = vec![0u64; (size as usize).div_ceil(8)];
    if size < 8 {
        return 0;
    }
    // SAFETY: aligned buffer with at least size bytes. No pointers in the list are dereferenced.
    if unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            words.as_mut_ptr().cast(),
        )
    } != 0
    {
        return 0;
    }
    let bytes = words.as_ptr().cast::<u8>();
    // SAFETY: first u32 is present; subsequent reads are bounded by returned size.
    let count = unsafe { bytes.cast::<u32>().read() } as usize;
    if count > (size as usize).saturating_sub(8) / 16 {
        return 0;
    }
    (0..count)
        .map(|i| {
            // SAFETY: checked AudioBuffer array bounds above.
            unsafe { bytes.add(8 + i * 16).cast::<u32>().read() }
        })
        .sum()
}

/// Prefer the master; otherwise control every channel, never just the left one.
fn controls<T: Copy + Default>(
    object: ObjId,
    kind: DeviceKind,
    selector: u32,
) -> Vec<(PropAddr, T)> {
    let main = PropAddr {
        selector,
        scope: scope(kind),
        element: 0,
    };
    let master = scoped_get(object, &main).ok();
    if let Some(value) = master.filter(|_| settable(object, &main)) {
        return vec![(main, value)];
    }
    let count = channel_count(object, kind);
    let channels: Vec<_> = (1..=count)
        .filter_map(|element| {
            let address = PropAddr { element, ..main };
            scoped_get(object, &address)
                .ok()
                .map(|value| (address, value))
        })
        .collect();
    if count > 0 && channels.len() == count as usize {
        channels
    } else {
        master.map(|value| vec![(main, value)]).unwrap_or_default()
    }
}

impl DeviceAudio {
    fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            listeners: Vec::new(),
            targets: Vec::new(),
        }
    }

    fn listen(&mut self, object: ObjId, address: PropAddr) {
        if self
            .listeners
            .iter()
            .any(|l| l.object == object && l.address == address)
        {
            return;
        }
        let notifier = self.notifier.clone();
        let block = RcBlock::new(move |_: u32, _: *const c_void| {
            notifier.notify();
        });
        // SAFETY: the HAL copies this block, owning its captured context.
        if unsafe {
            AudioObjectAddPropertyListenerBlock(object, &address, std::ptr::null(), &block)
        } == 0
        {
            self.listeners.push(DeviceListener {
                object,
                address,
                block,
            });
        }
    }

    fn snapshot(&mut self) -> Result<Vec<Device>, Error> {
        for selector in [HW_DEVICES, HW_DEFAULT_OUTPUT, HW_DEFAULT_INPUT] {
            self.listen(SYSTEM_OBJECT, addr(selector));
        }
        let objects =
            get_ids(SYSTEM_OBJECT, HW_DEVICES).map_err(|e| e.into_error(ErrorKind::Native))?;
        self.listeners
            .retain(|l| l.object == SYSTEM_OBJECT || objects.contains(&l.object));
        let default_output = get::<u32>(SYSTEM_OBJECT, HW_DEFAULT_OUTPUT).unwrap_or(0);
        let default_input = get::<u32>(SYSTEM_OBJECT, HW_DEFAULT_INPUT).unwrap_or(0);
        let mut devices = Vec::new();
        let mut targets = Vec::new();
        for object in objects {
            let Some(id) = get_string(object, DEV_UID)
                .ok()
                .flatten()
                .filter(|id| valid_device_id(id))
            else {
                continue;
            };
            let name = get_string(object, DEV_NAME)
                .ok()
                .flatten()
                .unwrap_or_else(|| id.clone());
            for kind in [DeviceKind::Output, DeviceKind::Input] {
                let address = PropAddr {
                    selector: DEV_STREAMS,
                    scope: scope(kind),
                    element: 0,
                };
                let mut size = 0;
                // SAFETY: valid scoped property and output size, only checking stream presence.
                if unsafe {
                    AudioObjectGetPropertyDataSize(object, &address, 0, std::ptr::null(), &mut size)
                } != 0
                    || size == 0
                {
                    continue;
                }
                let volumes = controls::<f32>(object, kind, DEV_VOLUME);
                let mutes = controls::<u32>(object, kind, DEV_MUTE);
                for (a, _) in &volumes {
                    self.listen(object, *a);
                }
                for (a, _) in &mutes {
                    self.listen(object, *a);
                }
                // Listen to main even when currently unsupported, for capability changes.
                for selector in [DEV_VOLUME, DEV_MUTE] {
                    self.listen(
                        object,
                        PropAddr {
                            selector,
                            scope: scope(kind),
                            element: 0,
                        },
                    );
                }
                devices.push(Device {
                    id: id.clone(),
                    name: name.clone(),
                    kind,
                    volume: volumes
                        .iter()
                        .map(|(_, v)| *v)
                        .reduce(f32::max)
                        .unwrap_or(0.0)
                        .clamp(0.0, 1.0),
                    muted: !mutes.is_empty() && mutes.iter().all(|(_, v)| *v != 0),
                    is_default: object
                        == if kind == DeviceKind::Output {
                            default_output
                        } else {
                            default_input
                        },
                    can_set_volume: !volumes.is_empty()
                        && volumes.iter().all(|(a, _)| settable(object, a)),
                    can_mute: !mutes.is_empty() && mutes.iter().all(|(a, _)| settable(object, a)),
                    emulated: false,
                });
                targets.push((id.clone(), kind, object));
            }
        }
        devices.sort_by(|a, b| a.id.cmp(&b.id).then(a.kind.cmp(&b.kind)));
        self.targets = targets;
        Ok(devices)
    }

    fn set<T: Copy + Default>(
        &self,
        id: &str,
        kind: DeviceKind,
        selector: u32,
        value: T,
    ) -> Result<(), Error> {
        let object = self
            .targets
            .iter()
            .find(|(key, k, _)| key == id && *k == kind)
            .map(|(_, _, object)| *object)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "device disappeared"))?;
        // HAL object ids can be reused after unplug; never write to a new UID.
        if get_string(object, DEV_UID).ok().flatten().as_deref() != Some(id) {
            return Err(Error::new(ErrorKind::NotFound, "device disappeared"));
        }
        let controls = controls::<T>(object, kind, selector);
        if controls.is_empty() || !controls.iter().all(|(a, _)| settable(object, a)) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "device property is not settable",
            ));
        }
        for (address, _) in controls {
            // SAFETY: T is the property's scalar type; all control writes stay on this worker.
            check("AudioObjectSetPropertyData", unsafe {
                AudioObjectSetPropertyData(
                    object,
                    &address,
                    0,
                    std::ptr::null(),
                    size_of::<T>() as u32,
                    (&value as *const T).cast(),
                )
            })
            .map_err(|e| e.into_error(ErrorKind::Native))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysaudio::model::is_valid_id;

    /// Exercises the real, permission-free listing path (no taps).
    #[test]
    fn lists_process_objects_without_taps() {
        let Ok(mut backend) = make(Options::default())(test_notifier()) else {
            // Pre-14.2 hosts have no process objects; nothing to test.
            return;
        };
        let status = backend.status();
        if !status.available {
            return;
        }
        assert!(status.available);
        assert!(!status.can_set_volume && !status.can_mute);
        let apps = backend.snapshot().expect("snapshot");
        for app in &apps {
            assert!(is_valid_id(&app.id), "{}", app.id);
            assert!(app.process_count >= 1);
            assert_eq!(app.volume, 1.0);
        }
    }

    #[test]
    fn lists_devices_without_taps_or_audio_writes() {
        let mut devices = DeviceAudio::new(test_notifier());
        let snapshot = devices.snapshot().expect("device snapshot");
        let mut keys = std::collections::HashSet::new();
        for device in snapshot {
            assert!(valid_device_id(&device.id));
            assert!(keys.insert((device.id, device.kind as u32)));
            assert!((0.0..=1.0).contains(&device.volume));
        }
    }

    fn test_notifier() -> crate::sysaudio::engine::Notifier {
        crate::sysaudio::engine::test_notifier()
    }
}
