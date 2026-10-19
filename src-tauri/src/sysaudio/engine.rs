//! Backend-independent engine: one worker thread owns the platform backend (on
//! Windows, every COM object; on macOS, every tap and listener), keeps the app
//! and device snapshots, executes commands sent from other threads, and tells
//! subscribers what changed.
//!
//! Rules that keep it safe:
//! - OS calls happen only on the worker, never under a lock.
//! - A command waits at most `COMMAND_TIMEOUT` for its reply. A command that
//!   times out invalidates the device snapshot until the worker has served
//!   that very request and re-read the devices (request-identity recovery).
//! - App and device snapshots start and fail independently.
//! - Shutdown is bounded; a stuck worker is detached, not waited for forever.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;

use super::hub::{Changes, Hub, Subscription};
use super::model::{App, Device, DeviceKind};

const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    InvalidArgument,
    /// The platform, the backend or the device cannot do this.
    Unsupported,
    NotFound,
    /// The OS refused (e.g. the System Audio Recording permission for taps).
    Permission,
    Native,
    /// The audio thread did not answer in time.
    Timeout,
}

#[derive(Clone, Debug, PartialEq, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BackendKind {
    None,
    MacosTap,
    WindowsCoreAudio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
/// Whether the OS lets Lunchpad change app volume (frozen UI values
/// `unknown | granted | denied | unconfirmed`).
pub enum Permission {
    /// Not checked yet, or not relevant because per-app control is off.
    Unknown,
    /// Confirmed, or no permission is needed (Windows sessions).
    Granted,
    /// Evidence of denial: a muted tap stayed silent while its app played (G1).
    Denied,
    /// A check ran but could not prove the permission (silence is ambiguous).
    Unconfirmed,
}

/// The per-app side of the backend. Devices have their own availability: the
/// device list is an error when they are unavailable.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub backend: BackendKind,
    /// App listing works. When false, `message` says why.
    pub available: bool,
    /// App volume can be changed.
    pub can_set_volume: bool,
    /// Apps can be muted.
    pub can_mute: bool,
    pub permission: Permission,
    pub message: String,
}

impl AppStatus {
    pub fn unavailable(backend: BackendKind, message: impl Into<String>) -> AppStatus {
        AppStatus {
            backend,
            available: false,
            can_set_volume: false,
            can_mute: false,
            permission: Permission::Unknown,
            message: message.into(),
        }
    }
}

/// A platform backend. Constructed and used only on the engine's worker thread.
pub trait Backend {
    fn status(&self) -> AppStatus;
    /// How often to refresh without a `Dirty` notification.
    fn poll_interval(&self) -> Duration;
    /// The complete, grouped app list.
    fn snapshot(&mut self) -> Result<Vec<App>, Error>;
    /// `app` is known to exist in the latest snapshot.
    fn set_volume(&mut self, app: &str, volume: f32) -> Result<(), Error>;
    fn set_mute(&mut self, app: &str, muted: bool) -> Result<(), Error>;
    fn devices(&mut self) -> Result<Vec<Device>, Error> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "device audio is unsupported",
        ))
    }
    fn set_device_volume(
        &mut self,
        _id: &str,
        _kind: DeviceKind,
        _volume: f32,
    ) -> Result<(), Error> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "device volume is unsupported",
        ))
    }
    fn set_device_mute(&mut self, _id: &str, _kind: DeviceKind, _muted: bool) -> Result<(), Error> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "device mute is unsupported",
        ))
    }
}

pub enum Command {
    AppVolume(String, f32),
    AppMute(String, bool),
    DeviceVolume(String, DeviceKind, f32),
    DeviceMute(String, DeviceKind, bool),
}

impl Command {
    fn is_device(&self) -> bool {
        matches!(self, Command::DeviceVolume(..) | Command::DeviceMute(..))
    }
}

enum Msg {
    Command(Command, SyncSender<Result<(), Error>>, u64),
    Dirty,
    Shutdown,
}

/// Lets notification callbacks on foreign threads ask for a refresh. Repeated
/// notifications before the worker gets to them collapse into one.
#[derive(Clone)]
pub struct Notifier {
    tx: Sender<Msg>,
    pending: Arc<AtomicBool>,
}

impl Notifier {
    pub fn notify(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            // The worker may already be gone during shutdown; nothing to do then.
            let _ = self.tx.send(Msg::Dirty);
        }
    }
}

/// A notifier whose notifications go nowhere, for backend unit tests.
#[cfg(all(test, target_os = "macos"))]
pub fn test_notifier() -> Notifier {
    let (tx, _rx) = mpsc::channel();
    Notifier {
        tx,
        pending: Arc::new(AtomicBool::new(false)),
    }
}

struct Shared {
    hub: Hub,
    apps: Mutex<Vec<App>>,
    devices: Mutex<Result<Vec<Device>, Error>>,
    status: Mutex<AppStatus>,
    /// Set (under the `status` lock) when `start` gave up waiting for the
    /// worker. From then on the worker may no longer publish a status, so a
    /// late start cannot flip the handle back to available.
    detached: AtomicBool,
    requests: Mutex<Requests>,
    running: AtomicBool,
}

/// Command ids are issued under this lock together with the send, so the
/// worker serves them in id order: once it has refreshed after command `n`,
/// every command up to `n` has returned from the backend.
#[derive(Default)]
struct Requests {
    issued: u64,
    /// Highest command id whose device refresh the worker has published.
    refreshed: u64,
    /// Highest timed-out command id not yet refreshed. While set, the device
    /// cache stays invalid and further commands fail fast instead of each
    /// blocking the caller for the full timeout. Only a refresh after this
    /// command (not an earlier dirty, poll or command refresh) clears it.
    wedged_by: Option<u64>,
}

impl Shared {
    fn new(kind: BackendKind) -> Shared {
        Shared {
            hub: Hub::default(),
            apps: Mutex::new(Vec::new()),
            devices: Mutex::new(Err(Error::new(ErrorKind::Unsupported, "starting"))),
            status: Mutex::new(AppStatus::unavailable(kind, "starting")),
            detached: AtomicBool::new(false),
            requests: Mutex::new(Requests::default()),
            running: AtomicBool::new(false),
        }
    }

    /// Command `id` was not answered in time: the worker may be blocked
    /// indefinitely, so its cached devices are no longer authoritative. A
    /// timeout that loses the race with the command's own refresh is a no-op.
    fn time_out(&self, id: u64) {
        let mut requests = lock(&self.requests);
        if requests.refreshed >= id {
            return;
        }
        let was_wedged = requests.wedged_by.is_some();
        requests.wedged_by = Some(requests.wedged_by.map_or(id, |w| w.max(id)));
        if !was_wedged {
            *lock(&self.devices) = Err(Error::new(ErrorKind::Timeout, "the audio thread is busy"));
            self.hub.notify(Changes::DEVICES);
        }
    }

    /// Publishes a device snapshot the worker just read. `serviced` is the
    /// command this refresh follows, if any.
    fn publish_devices(
        &self,
        serviced: Option<u64>,
        next: Result<Vec<Device>, Error>,
        applied: bool,
    ) {
        let mut requests = lock(&self.requests);
        if let Some(id) = serviced {
            requests.refreshed = requests.refreshed.max(id);
        }
        if let Some(wedged_by) = requests.wedged_by {
            if requests.refreshed < wedged_by {
                return;
            }
            requests.wedged_by = None;
        }
        let changed = {
            let mut devices = lock(&self.devices);
            let changed = *devices != next;
            *devices = next;
            changed
        };
        if changed || applied {
            self.hub.notify(Changes::DEVICES);
        }
    }

    fn publish_apps(&self, next: Vec<App>, applied: bool) {
        let changed = {
            let mut apps = lock(&self.apps);
            let changed = *apps != next;
            *apps = next;
            changed
        };
        if changed || applied {
            self.hub.notify(Changes::APPS);
        }
    }

    /// The worker's only way to change the status (see `detached`).
    fn publish_status(&self, status: AppStatus) {
        let mut current = lock(&self.status);
        if !self.detached.load(Ordering::Acquire) && *current != status {
            *current = status;
            self.hub.notify(Changes::STATUS);
        }
    }
}

struct Worker {
    tx: Sender<Msg>,
    join: JoinHandle<()>,
}

/// The engine behind a [`super::SystemAudio`] handle.
pub struct Engine {
    shared: Arc<Shared>,
    worker: Mutex<Option<Worker>>,
    command_timeout: Duration,
}

/// Builds the backend on the worker thread. On failure, the status explains why
/// the backend is unavailable.
pub type MakeBackend = Box<dyn FnOnce(Notifier) -> Result<Box<dyn Backend>, AppStatus> + Send>;

impl Engine {
    /// Starts the worker, builds the backend there and waits (at most 5 s) for
    /// the first snapshot. A backend that cannot start yields an engine whose
    /// status is unavailable; that is not an error.
    pub fn start(kind: BackendKind, make: MakeBackend) -> Engine {
        Engine::start_with(kind, make, OPEN_TIMEOUT)
    }

    fn start_with(kind: BackendKind, make: MakeBackend, open_timeout: Duration) -> Engine {
        let shared = Arc::new(Shared::new(kind));
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<bool>(1);
        let notifier = Notifier {
            tx: tx.clone(),
            pending: Arc::new(AtomicBool::new(false)),
        };
        let worker_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("lunchpad-sysaudio".into())
            .spawn(move || {
                // A panicking backend must not leave a handle that claims to
                // be available while nothing serves it.
                let panicked = Arc::clone(&worker_shared);
                let ready_on_panic = ready_tx.clone();
                let run = AssertUnwindSafe(move || {
                    run_worker(worker_shared, make, notifier, rx, ready_tx);
                });
                if let Err(panic) = catch_unwind(run) {
                    panicked.running.store(false, Ordering::Release);
                    *lock(&panicked.devices) =
                        Err(Error::new(ErrorKind::Native, "the audio thread panicked"));
                    panicked.hub.notify(Changes::DEVICES | Changes::APPS);
                    panicked.publish_status(AppStatus::unavailable(
                        kind,
                        format!("the audio thread panicked: {}", panic_message(&*panic)),
                    ));
                    let _ = ready_on_panic.try_send(false);
                }
            });
        let engine = |worker| Engine {
            shared: Arc::clone(&shared),
            worker: Mutex::new(worker),
            command_timeout: COMMAND_TIMEOUT,
        };
        let join = match spawned {
            Ok(join) => join,
            Err(e) => {
                *lock(&shared.status) =
                    AppStatus::unavailable(kind, format!("cannot start the audio thread: {e}"));
                return engine(None);
            }
        };
        match ready_rx.recv_timeout(open_timeout) {
            Ok(true) => engine(Some(Worker { tx, join })),
            Ok(false) => {
                // The worker reported an unavailable backend and has exited.
                let _ = join.join();
                engine(None)
            }
            Err(_) => {
                {
                    let mut status = lock(&shared.status);
                    shared.detached.store(true, Ordering::Release);
                    *status = AppStatus::unavailable(
                        kind,
                        format!(
                            "the audio backend did not start within {} s",
                            open_timeout.as_secs_f32()
                        ),
                    );
                }
                let _ = tx.send(Msg::Shutdown);
                engine(None)
            }
        }
    }

    pub fn status(&self) -> AppStatus {
        lock(&self.shared.status).clone()
    }

    pub fn list(&self) -> Result<Vec<App>, Error> {
        let status = self.status();
        if !status.available {
            return Err(Error::new(ErrorKind::Unsupported, status.message));
        }
        Ok(lock(&self.shared.apps).clone())
    }

    pub fn list_devices(&self) -> Result<Vec<Device>, Error> {
        if self.shared.detached.load(Ordering::Acquire)
            || !self.shared.running.load(Ordering::Acquire)
        {
            return Err(Error::new(ErrorKind::Unsupported, self.status().message));
        }
        lock(&self.shared.devices).clone()
    }

    pub fn subscribe(&self) -> Subscription {
        self.shared.hub.subscribe()
    }

    pub fn command(&self, command: Command) -> Result<(), Error> {
        let status = self.status();
        let capable = match command {
            Command::AppVolume(..) => status.can_set_volume,
            Command::AppMute(..) => status.can_mute,
            Command::DeviceVolume(..) | Command::DeviceMute(..) => true,
        };
        if (!status.available && !command.is_device()) || !capable {
            let why = if status.message.is_empty() {
                "the audio backend cannot change this setting".to_string()
            } else {
                status.message
            };
            return Err(Error::new(ErrorKind::Unsupported, why));
        }
        let Some(tx) = lock(&self.worker).as_ref().map(|w| w.tx.clone()) else {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "the audio backend is not running",
            ));
        };
        if !self.shared.running.load(Ordering::Acquire) {
            return Err(Error::new(ErrorKind::Unsupported, self.status().message));
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let id = {
            let mut requests = lock(&self.shared.requests);
            if requests.wedged_by.is_some() {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    "the audio thread is still busy with an earlier request",
                ));
            }
            requests.issued += 1;
            let id = requests.issued;
            tx.send(Msg::Command(command, reply_tx, id))
                .map_err(|_| Error::new(ErrorKind::Native, "the audio thread has stopped"))?;
            id
        };
        match reply_rx.recv_timeout(self.command_timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                self.shared.time_out(id);
                Err(Error::new(
                    ErrorKind::Timeout,
                    format!(
                        "the audio thread did not answer within {} s",
                        self.command_timeout.as_secs_f32()
                    ),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => Err(Error::new(
                ErrorKind::Native,
                "the audio thread has stopped",
            )),
        }
    }

    /// Stops the worker, which drops the backend (taps, listeners, COM) on its
    /// own thread. Waits at most `CLOSE_TIMEOUT`; a worker stuck in an OS call
    /// is detached rather than blocking the caller forever. Idempotent.
    pub fn shutdown(&self) {
        let Some(Worker { tx, join }) = lock(&self.worker).take() else {
            return;
        };
        let _ = tx.send(Msg::Shutdown);
        let (done_tx, done_rx) = mpsc::sync_channel::<()>(1);
        let waiter = std::thread::spawn(move || {
            let _ = join.join();
            let _ = done_tx.send(());
        });
        if done_rx.recv_timeout(CLOSE_TIMEOUT).is_ok() {
            let _ = waiter.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_worker(
    shared: Arc<Shared>,
    make: MakeBackend,
    notifier: Notifier,
    rx: Receiver<Msg>,
    ready: SyncSender<bool>,
) {
    let pending = Arc::clone(&notifier.pending);
    let mut backend = match make(notifier) {
        Ok(backend) => backend,
        Err(status) => {
            shared.publish_status(status);
            let _ = ready.send(false);
            return;
        }
    };
    // Apps and devices start independently: a failing app enumeration keeps
    // the devices usable and is retried by the refresh loop.
    match backend.snapshot() {
        Ok(apps) => {
            *lock(&shared.apps) = apps;
            shared.publish_status(backend.status());
        }
        Err(e) => {
            shared.publish_status(AppStatus::unavailable(backend.status().backend, e.message))
        }
    }
    let devices = backend.devices();
    *lock(&shared.devices) = devices;
    shared.running.store(true, Ordering::Release);
    if ready.send(true).is_err() || shared.detached.load(Ordering::Acquire) {
        // `start` timed out and already reported the backend as unavailable.
        shared.running.store(false, Ordering::Release);
        return;
    }

    loop {
        match rx.recv_timeout(backend.poll_interval()) {
            Ok(Msg::Command(command, reply, id)) => {
                let device_command = command.is_device();
                let result = execute(&shared, backend.as_mut(), command);
                let applied = result.is_ok();
                // Answer before refreshing: a slow snapshot must not count
                // against the caller's command timeout.
                let _ = reply.send(result);
                // A successful setter always publishes the state it produced,
                // even when the OS value did not change.
                let applied = Applied {
                    devices: applied && device_command,
                    apps: applied && !device_command,
                };
                refresh_with(&shared, backend.as_mut(), applied, Some(id));
            }
            Ok(Msg::Dirty) => {
                pending.store(false, Ordering::Release);
                refresh(&shared, backend.as_mut());
            }
            Err(RecvTimeoutError::Timeout) => refresh(&shared, backend.as_mut()),
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    shared.running.store(false, Ordering::Release);
}

fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

fn execute(shared: &Shared, backend: &mut dyn Backend, command: Command) -> Result<(), Error> {
    let (target, result) = match command {
        Command::AppVolume(app, volume) => {
            ensure_known(shared, &app)?;
            let result = backend.set_volume(&app, volume);
            (app, result)
        }
        Command::AppMute(app, muted) => {
            ensure_known(shared, &app)?;
            let result = backend.set_mute(&app, muted);
            (app, result)
        }
        Command::DeviceVolume(id, kind, volume) => {
            ensure_device(shared, &id, kind, false)?;
            let result = backend.set_device_volume(&id, kind, volume);
            (id, result)
        }
        Command::DeviceMute(id, kind, muted) => {
            ensure_device(shared, &id, kind, true)?;
            let result = backend.set_device_mute(&id, kind, muted);
            (id, result)
        }
    };
    result.map_err(|e| Error::new(e.kind, format!("{target}: {}", e.message)))
}

fn ensure_device(shared: &Shared, id: &str, kind: DeviceKind, mute: bool) -> Result<(), Error> {
    let devices = lock(&shared.devices);
    let devices = devices.as_ref().map_err(Clone::clone)?;
    let device = devices
        .iter()
        .find(|d| d.id == id && d.kind == kind)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "no audio device with this id and kind"))?;
    if if mute {
        device.can_mute
    } else {
        device.can_set_volume
    } {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::Unsupported,
            "the device cannot change this setting",
        ))
    }
}

fn ensure_known(shared: &Shared, app: &str) -> Result<(), Error> {
    if lock(&shared.apps).iter().any(|a| a.id == app) {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::NotFound,
            format!("no audio app with id {app}"),
        ))
    }
}

/// Which snapshot a successful setter changed.
#[derive(Clone, Copy, Default)]
struct Applied {
    devices: bool,
    apps: bool,
}

/// Takes fresh snapshots and publishes what changed. A failed app snapshot
/// keeps the previous list and reports the app side unavailable; the next poll
/// tries again. The status is refreshed too (e.g. a Windows output device
/// appearing or disappearing).
fn refresh(shared: &Shared, backend: &mut dyn Backend) {
    refresh_with(shared, backend, Applied::default(), None);
}

fn refresh_with(
    shared: &Shared,
    backend: &mut dyn Backend,
    applied: Applied,
    serviced: Option<u64>,
) {
    // The OS read happens before, never under, the publication locks.
    let next_devices = backend.devices();
    shared.publish_devices(serviced, next_devices, applied.devices);
    match backend.snapshot() {
        Ok(next) => {
            shared.publish_apps(next, applied.apps);
            shared.publish_status(backend.status());
        }
        Err(e) => {
            shared.publish_status(AppStatus::unavailable(backend.status().backend, e.message))
        }
    }
}

/// Every critical section above is a plain assignment, comparison or clone, so
/// a poisoned lock still holds consistent data.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone)]
    struct DeviceScript {
        devices: Arc<Mutex<Vec<Device>>>,
        calls: Arc<Mutex<Vec<(String, DeviceKind, bool)>>>,
        fail: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl Backend for DeviceScript {
        fn status(&self) -> AppStatus {
            AppStatus::unavailable(BackendKind::None, "no app endpoint")
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_secs(60)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            Ok(Vec::new())
        }
        fn set_volume(&mut self, _: &str, _: f32) -> Result<(), Error> {
            unreachable!()
        }
        fn set_mute(&mut self, _: &str, _: bool) -> Result<(), Error> {
            unreachable!()
        }
        fn devices(&mut self) -> Result<Vec<Device>, Error> {
            if self.fail.load(Ordering::Acquire) {
                return Err(Error::new(ErrorKind::Native, "device read failed"));
            }
            Ok(lock(&self.devices).clone())
        }
        fn set_device_volume(
            &mut self,
            id: &str,
            kind: DeviceKind,
            volume: f32,
        ) -> Result<(), Error> {
            lock(&self.calls).push((id.into(), kind, false));
            for device in lock(&self.devices)
                .iter_mut()
                .filter(|d| d.id == id && d.kind == kind)
            {
                device.volume = volume;
            }
            Ok(())
        }
        fn set_device_mute(
            &mut self,
            id: &str,
            kind: DeviceKind,
            muted: bool,
        ) -> Result<(), Error> {
            lock(&self.calls).push((id.into(), kind, true));
            for device in lock(&self.devices)
                .iter_mut()
                .filter(|d| d.id == id && d.kind == kind)
            {
                device.muted = muted;
            }
            Ok(())
        }
    }

    impl Drop for DeviceScript {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    fn device_script() -> DeviceScript {
        DeviceScript {
            devices: Arc::new(Mutex::new(
                [DeviceKind::Output, DeviceKind::Input]
                    .map(|kind| Device {
                        id: "opaque UID / é".into(),
                        name: "Duplex".into(),
                        kind,
                        volume: 0.5,
                        muted: false,
                        is_default: true,
                        can_set_volume: true,
                        can_mute: kind == DeviceKind::Input,
                        emulated: false,
                    })
                    .to_vec(),
            )),
            calls: Arc::new(Mutex::new(Vec::new())),
            fail: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    struct GatedDevices {
        script: DeviceScript,
        entered: Sender<()>,
        release: Receiver<()>,
        fail_apps: Arc<AtomicBool>,
        app_reads: Arc<AtomicUsize>,
    }

    impl Backend for GatedDevices {
        fn status(&self) -> AppStatus {
            Tunable::default().status()
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_secs(60)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            let failed = self.fail_apps.load(Ordering::Acquire);
            self.app_reads.fetch_add(1, Ordering::Release);
            if failed {
                return Err(Error::new(ErrorKind::Native, "app enumeration failed"));
            }
            Ok(vec![app("a")])
        }
        fn devices(&mut self) -> Result<Vec<Device>, Error> {
            self.script.devices()
        }
        fn set_volume(&mut self, _: &str, _: f32) -> Result<(), Error> {
            unreachable!()
        }
        fn set_mute(&mut self, _: &str, _: bool) -> Result<(), Error> {
            unreachable!()
        }
        fn set_device_volume(
            &mut self,
            id: &str,
            kind: DeviceKind,
            volume: f32,
        ) -> Result<(), Error> {
            self.entered.send(()).expect("setter entered");
            self.release.recv().expect("release setter");
            self.script.set_device_volume(id, kind, volume)
        }
    }

    fn is_wedged(engine: &Engine) -> bool {
        lock(&engine.shared.requests).wedged_by.is_some()
    }

    fn wait_until(check: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !check() {
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not progress"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn initial_app_failure_keeps_devices_usable_and_recovers_on_dirty() {
        let script = device_script();
        let backend = script.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (notifier_tx, notifier_rx) = mpsc::channel();
        let fail_apps = Arc::new(AtomicBool::new(true));
        let backend_fail_apps = Arc::clone(&fail_apps);
        let app_reads = Arc::new(AtomicUsize::new(0));
        let backend_app_reads = Arc::clone(&app_reads);
        let engine = Engine::start(
            BackendKind::None,
            Box::new(move |notifier| {
                notifier_tx.send(notifier).expect("notifier");
                Ok(Box::new(GatedDevices {
                    script: backend,
                    entered: entered_tx,
                    release: release_rx,
                    fail_apps: backend_fail_apps,
                    app_reads: backend_app_reads,
                }))
            }),
        );
        assert!(!engine.status().available);
        assert_eq!(engine.status().message, "app enumeration failed");
        assert!(engine.list().is_err());
        assert_eq!(engine.list_devices().expect("devices").len(), 2);
        release_tx.send(()).expect("allow setter");
        assert_eq!(
            engine.command(Command::DeviceVolume(
                "opaque UID / é".into(),
                DeviceKind::Output,
                0.2,
            )),
            Ok(())
        );
        entered_rx.recv().expect("setter ran");
        wait_until(|| engine.list_devices().is_ok_and(|d| d[0].volume == 0.2));
        assert!(!engine.status().available);
        assert!(engine.list().is_err());
        wait_until(|| app_reads.load(Ordering::Acquire) >= 2);
        fail_apps.store(false, Ordering::Release);
        notifier_rx.recv().expect("notifier").notify();
        wait_until(|| engine.status().available);
        assert_eq!(engine.list().expect("recovered apps"), vec![app("a")]);
        assert_eq!(engine.list_devices().expect("devices")[0].volume, 0.2);
    }

    #[test]
    fn devices_are_cached_keyed_and_emit_one_event_per_refresh_including_successful_noops() {
        let script = device_script();
        let backend = script.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let engine = Engine::start(
            BackendKind::None,
            Box::new(move |notifier| {
                tx.send(notifier).expect("notifier");
                Ok(Box::new(backend))
            }),
        );
        let notifier = rx.recv().expect("notifier");
        let sub = engine.subscribe();
        assert!(
            !engine.status().available,
            "device support is independent of app endpoint"
        );
        assert_eq!(engine.list_devices().expect("devices").len(), 2);
        lock(&script.devices)[0].volume = 0.8;
        assert_eq!(engine.list_devices().expect("cached")[0].volume, 0.5);
        notifier.notify();
        assert!(eventually(|| engine
            .list_devices()
            .is_ok_and(|d| d[0].volume == 0.8)));
        assert_eq!(sub.recv_timeout(WAIT), Ok(Changes::DEVICES));
        assert_eq!(
            sub.try_recv(),
            Ok(Changes::default()),
            "one refresh, one notification"
        );
        assert_eq!(
            engine.command(Command::DeviceVolume(
                "opaque UID / é".into(),
                DeviceKind::Input,
                0.2
            )),
            Ok(())
        );
        assert!(eventually(|| engine
            .list_devices()
            .is_ok_and(|d| d[1].volume == 0.2)));
        assert_eq!(sub.recv_timeout(WAIT), Ok(Changes::DEVICES));
        assert_eq!(engine.list_devices().expect("devices")[0].volume, 0.8);
        // A successful setter must publish even if its OS value was unchanged.
        assert_eq!(
            engine.command(Command::DeviceVolume(
                "opaque UID / é".into(),
                DeviceKind::Input,
                0.2
            )),
            Ok(())
        );
        assert_eq!(sub.recv_timeout(WAIT), Ok(Changes::DEVICES));
        assert_eq!(
            engine
                .command(Command::DeviceMute(
                    "opaque UID / é".into(),
                    DeviceKind::Output,
                    true
                ))
                .map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );
        assert_eq!(
            engine
                .command(Command::DeviceMute(
                    "missing".into(),
                    DeviceKind::Input,
                    true
                ))
                .map_err(|e| e.kind),
            Err(ErrorKind::NotFound)
        );
        assert_eq!(
            engine.command(Command::DeviceMute(
                "opaque UID / é".into(),
                DeviceKind::Input,
                true
            )),
            Ok(())
        );
        assert!(eventually(|| engine
            .list_devices()
            .is_ok_and(|d| d[1].muted)));
        script.fail.store(true, Ordering::Release);
        notifier.notify();
        assert!(eventually(|| engine.list_devices().is_err()));
        assert_eq!(
            engine
                .command(Command::DeviceVolume(
                    "opaque UID / é".into(),
                    DeviceKind::Input,
                    0.4
                ))
                .map_err(|e| e.kind),
            Err(ErrorKind::Native)
        );
        drop(engine);
        assert!(script.dropped.load(Ordering::Acquire));
    }

    #[test]
    fn callback_notifications_coalesce_without_os_reads() {
        let (tx, rx) = mpsc::channel();
        let notifier = Notifier {
            tx,
            pending: Arc::new(AtomicBool::new(false)),
        };
        for _ in 0..10_000 {
            notifier.notify();
        }
        assert_eq!(rx.try_iter().count(), 1);
        notifier.pending.store(false, Ordering::Release);
        notifier.notify();
        assert_eq!(rx.try_iter().count(), 1);
    }

    struct Scripted {
        snapshots: Arc<Mutex<Vec<App>>>,
        volumes: Arc<Mutex<Vec<(String, f32)>>>,
    }

    impl Backend for Scripted {
        fn status(&self) -> AppStatus {
            AppStatus {
                backend: BackendKind::None,
                available: true,
                can_set_volume: true,
                can_mute: false,
                permission: Permission::Granted,
                message: String::new(),
            }
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_millis(5)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            Ok(lock(&self.snapshots).clone())
        }
        fn set_volume(&mut self, app: &str, volume: f32) -> Result<(), Error> {
            lock(&self.volumes).push((app.into(), volume));
            for a in lock(&self.snapshots).iter_mut().filter(|a| a.id == app) {
                a.volume = volume;
            }
            Ok(())
        }
        fn set_mute(&mut self, _app: &str, _muted: bool) -> Result<(), Error> {
            unreachable!("can_mute is false")
        }
    }

    fn app(id: &str) -> App {
        App {
            id: id.into(),
            name: id.into(),
            volume: 1.0,
            muted: false,
            active: false,
            process_count: 1,
        }
    }

    #[test]
    fn engine_lists_diffs_and_executes_commands() {
        let snapshots = Arc::new(Mutex::new(vec![app("a")]));
        let volumes = Arc::new(Mutex::new(Vec::new()));
        let (s, v) = (Arc::clone(&snapshots), Arc::clone(&volumes));
        let engine = Engine::start(
            BackendKind::None,
            Box::new(move |_| {
                Ok(Box::new(Scripted {
                    snapshots: s,
                    volumes: v,
                }))
            }),
        );
        assert!(engine.status().available);
        assert_eq!(engine.list().map(|apps| apps.len()), Ok(1));
        let sub = engine.subscribe();

        lock(&snapshots).push(app("b"));
        assert_eq!(sub.recv_timeout(WAIT), Ok(Changes::APPS));
        assert_eq!(engine.list().map(|apps| apps.len()), Ok(2));

        assert_eq!(engine.command(Command::AppVolume("a".into(), 0.25)), Ok(()));
        assert_eq!(*lock(&volumes), [("a".to_string(), 0.25)]);
        assert_eq!(sub.recv_timeout(WAIT), Ok(Changes::APPS));
        assert_eq!(engine.list().expect("apps")[0].volume, 0.25);
        // A successful app setter publishes even when nothing changed.
        assert_eq!(engine.command(Command::AppVolume("a".into(), 0.25)), Ok(()));
        assert_eq!(sub.recv_timeout(WAIT), Ok(Changes::APPS));
        assert_eq!(
            engine
                .command(Command::AppVolume("missing".into(), 0.5))
                .map_err(|e| e.kind),
            Err(ErrorKind::NotFound)
        );
        assert_eq!(
            engine
                .command(Command::AppMute("a".into(), true))
                .map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );
    }

    /// A backend whose calls can be slowed down or made to panic.
    #[derive(Clone, Default)]
    struct Tunable {
        set_delay: Arc<Mutex<Duration>>,
        snapshot_delay: Arc<Mutex<Duration>>,
        panic_on_snapshot: Arc<AtomicBool>,
    }

    impl Backend for Tunable {
        fn devices(&mut self) -> Result<Vec<Device>, Error> {
            Ok(vec![Device {
                id: "device".into(),
                name: "Device".into(),
                kind: DeviceKind::Output,
                volume: 0.5,
                muted: false,
                is_default: true,
                can_set_volume: true,
                can_mute: true,
                emulated: false,
            }])
        }
        fn set_device_volume(&mut self, _: &str, _: DeviceKind, volume: f32) -> Result<(), Error> {
            self.set_volume("a", volume)
        }
        fn status(&self) -> AppStatus {
            AppStatus {
                backend: BackendKind::None,
                available: true,
                can_set_volume: true,
                can_mute: false,
                permission: Permission::Granted,
                message: String::new(),
            }
        }
        /// Long enough that only commands trigger a refresh during a test.
        fn poll_interval(&self) -> Duration {
            Duration::from_secs(60)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            if self.panic_on_snapshot.load(Ordering::Acquire) {
                panic!("backend exploded");
            }
            std::thread::sleep(*lock(&self.snapshot_delay));
            Ok(vec![app("a")])
        }
        fn set_volume(&mut self, _app: &str, _volume: f32) -> Result<(), Error> {
            std::thread::sleep(*lock(&self.set_delay));
            Ok(())
        }
        fn set_mute(&mut self, _app: &str, _muted: bool) -> Result<(), Error> {
            unreachable!("can_mute is false")
        }
    }

    fn start_tunable(tunable: &Tunable) -> Engine {
        let backend = tunable.clone();
        Engine::start(BackendKind::None, Box::new(move |_| Ok(Box::new(backend))))
    }

    fn set_a(engine: &Engine) -> Result<(), Error> {
        engine.command(Command::AppVolume("a".into(), 0.5))
    }

    const WAIT: Duration = Duration::from_secs(2);

    /// Records every write and holds it until released.
    struct Ordered {
        calls: Arc<Mutex<Vec<String>>>,
        entered: Sender<()>,
        release: Receiver<()>,
    }

    impl Ordered {
        fn write(&mut self, call: String) -> Result<(), Error> {
            lock(&self.calls).push(call);
            self.entered.send(()).expect("write entered");
            self.release.recv().expect("release write");
            Ok(())
        }
    }

    impl Backend for Ordered {
        fn status(&self) -> AppStatus {
            Tunable::default().status()
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_secs(60)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            Ok(vec![app("a")])
        }
        fn devices(&mut self) -> Result<Vec<Device>, Error> {
            Tunable::default().devices()
        }
        fn set_volume(&mut self, _: &str, volume: f32) -> Result<(), Error> {
            self.write(format!("app {volume}"))
        }
        fn set_mute(&mut self, _: &str, _: bool) -> Result<(), Error> {
            unreachable!("can_mute is false")
        }
        fn set_device_volume(&mut self, _: &str, _: DeviceKind, volume: f32) -> Result<(), Error> {
            self.write(format!("device {volume}"))
        }
    }

    #[test]
    fn writes_are_applied_in_the_order_they_were_accepted() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let backend = Ordered {
            calls: Arc::clone(&calls),
            entered: entered_tx,
            release: release_rx,
        };
        let mut engine = Engine::start(BackendKind::None, Box::new(move |_| Ok(Box::new(backend))));
        // Long enough that no write in this test times out.
        engine.command_timeout = Duration::from_secs(10);
        let issued = |engine: &Engine| lock(&engine.shared.requests).issued;
        let writes = [
            Command::AppVolume("a".into(), 0.1),
            Command::DeviceVolume("device".into(), DeviceKind::Output, 0.2),
            Command::AppVolume("a".into(), 0.3),
            Command::DeviceVolume("device".into(), DeviceKind::Output, 0.4),
        ];
        std::thread::scope(|scope| {
            let mut requests = Vec::new();
            for (n, write) in writes.into_iter().enumerate() {
                let engine = &engine;
                requests.push(scope.spawn(move || engine.command(write)));
                // The next write is only started once this one was accepted.
                wait_until(|| issued(engine) == n as u64 + 1);
                if n == 0 {
                    entered
                        .recv_timeout(WAIT)
                        .expect("first write holds the worker");
                }
            }
            for _ in 0..4 {
                release.send(()).expect("release a write");
            }
            for request in requests {
                assert_eq!(request.join().expect("request"), Ok(()));
            }
        });
        assert_eq!(
            *lock(&calls),
            ["app 0.1", "device 0.2", "app 0.3", "device 0.4"]
        );
    }

    #[test]
    fn shutdown_is_bounded_idempotent_and_stops_service() {
        let script = device_script();
        let backend = script.clone();
        let engine = Engine::start(BackendKind::None, Box::new(move |_| Ok(Box::new(backend))));
        assert!(engine.list_devices().is_ok());
        engine.shutdown();
        assert!(
            script.dropped.load(Ordering::Acquire),
            "the backend is dropped on its worker before shutdown returns"
        );
        engine.shutdown();
        assert!(engine.list_devices().is_err());
        assert_eq!(
            engine
                .command(Command::DeviceVolume(
                    "opaque UID / é".into(),
                    DeviceKind::Input,
                    0.1
                ))
                .map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );
    }

    /// Polls `check` for up to 2 s.
    fn eventually(check: impl Fn() -> bool) -> bool {
        (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            check()
        })
    }

    #[test]
    fn a_late_start_after_the_open_timeout_stays_unavailable() {
        let engine = Engine::start_with(
            BackendKind::None,
            Box::new(|_| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(Box::new(Tunable::default()))
            }),
            Duration::from_millis(30),
        );
        assert!(!engine.status().available);
        assert!(engine.status().message.contains("did not start"));
        // The worker finishes starting after the timeout; it must not flip
        // the handle back to available.
        std::thread::sleep(Duration::from_millis(500));
        let status = engine.status();
        assert!(!status.available, "status flipped back: {status:?}");
        assert!(status.message.contains("did not start"));
        assert_eq!(
            engine.list().map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );
        assert_eq!(
            set_a(&engine).map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );
    }

    #[test]
    fn a_command_is_answered_before_the_refresh_that_follows_it() {
        let tunable = Tunable::default();
        let mut engine = start_tunable(&tunable);
        engine.command_timeout = Duration::from_millis(300);
        *lock(&tunable.snapshot_delay) = Duration::from_millis(600);
        let started = std::time::Instant::now();
        assert_eq!(set_a(&engine), Ok(()));
        assert!(started.elapsed() < Duration::from_millis(300));
    }

    #[test]
    fn commands_fail_fast_while_the_worker_is_wedged_and_recover_after() {
        let tunable = Tunable::default();
        let mut engine = start_tunable(&tunable);
        engine.command_timeout = Duration::from_millis(50);
        *lock(&tunable.set_delay) = Duration::from_millis(400);

        let first = set_a(&engine).map_err(|e| e.kind);
        assert_eq!(first, Err(ErrorKind::Timeout));

        let started = std::time::Instant::now();
        let second = set_a(&engine);
        assert!(started.elapsed() < Duration::from_millis(40));
        let second = second.map_err(|e| e.kind);
        assert_eq!(second, Err(ErrorKind::Timeout));

        // Once the stalled command finishes, commands go through again.
        *lock(&tunable.set_delay) = Duration::ZERO;
        assert!(eventually(|| !is_wedged(&engine)));
        assert_eq!(set_a(&engine), Ok(()));
    }

    #[test]
    fn device_commands_keep_timeout_fast_failure_and_reply_before_refresh() {
        let tunable = Tunable::default();
        let mut engine = start_tunable(&tunable);
        engine.command_timeout = Duration::from_millis(50);
        *lock(&tunable.set_delay) = Duration::from_millis(400);
        let command = || Command::DeviceVolume("device".into(), DeviceKind::Output, 0.5);
        assert_eq!(
            engine.command(command()).map_err(|e| e.kind),
            Err(ErrorKind::Timeout)
        );
        let started = std::time::Instant::now();
        assert_eq!(
            engine.command(command()).map_err(|e| e.kind),
            Err(ErrorKind::Timeout)
        );
        assert!(started.elapsed() < Duration::from_millis(40));
        *lock(&tunable.set_delay) = Duration::ZERO;
        assert!(eventually(|| !is_wedged(&engine)));
        *lock(&tunable.snapshot_delay) = Duration::from_millis(600);
        assert_eq!(engine.command(command()), Ok(()));
    }

    #[test]
    fn device_timeout_invalidates_then_refresh_recovers_without_queuing_more_commands() {
        let script = device_script();
        let backend = script.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut engine = Engine::start(
            BackendKind::None,
            Box::new(move |_| {
                Ok(Box::new(GatedDevices {
                    script: backend,
                    entered: entered_tx,
                    release: release_rx,
                    fail_apps: Arc::new(AtomicBool::new(false)),
                    app_reads: Arc::new(AtomicUsize::new(0)),
                }))
            }),
        );
        engine.command_timeout = Duration::from_millis(50);
        let command = || Command::DeviceVolume("opaque UID / é".into(), DeviceKind::Output, 0.2);
        let sub = engine.subscribe();
        std::thread::scope(|scope| {
            let request = scope.spawn(|| engine.command(command()));
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("blocked setter");
            assert_eq!(
                request.join().expect("request").map_err(|e| e.kind),
                Err(ErrorKind::Timeout)
            );
        });
        assert_eq!(
            engine.list_devices().map_err(|e| e.kind),
            Err(ErrorKind::Timeout)
        );
        assert_eq!(sub.try_recv(), Ok(Changes::DEVICES));
        let started = std::time::Instant::now();
        for _ in 0..1000 {
            assert_eq!(
                engine.command(command()).map_err(|e| e.kind),
                Err(ErrorKind::Timeout)
            );
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        release_tx.send(()).expect("release setter");
        wait_until(|| !is_wedged(&engine));
        assert_eq!(engine.list_devices().expect("recovered")[0].volume, 0.2);
        assert_eq!(sub.try_recv(), Ok(Changes::DEVICES));
        assert_eq!(
            lock(&script.calls).len(),
            1,
            "fast failures never queued setters"
        );
        // A second stalled setter also cannot hold close indefinitely. Its
        // detached backend remains owned by the worker until it is released.
        std::thread::scope(|scope| {
            let request = scope.spawn(|| engine.command(command()));
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("second blocked setter");
            assert_eq!(
                request.join().expect("request").map_err(|e| e.kind),
                Err(ErrorKind::Timeout)
            );
        });
        let started = std::time::Instant::now();
        drop(engine);
        assert!(started.elapsed() < CLOSE_TIMEOUT + Duration::from_secs(1));
        assert!(!script.dropped.load(Ordering::Acquire));
        release_tx.send(()).expect("release detached setter");
        wait_until(|| script.dropped.load(Ordering::Acquire));
    }

    /// Gates the OS device read of one refresh and every app volume setter.
    struct TwoGates {
        script: DeviceScript,
        gate_devices: Arc<AtomicBool>,
        devices_entered: Sender<()>,
        devices_release: Receiver<()>,
        setter_entered: Sender<()>,
        setter_release: Receiver<()>,
        app_setters: Arc<AtomicUsize>,
    }

    impl Backend for TwoGates {
        fn status(&self) -> AppStatus {
            Tunable::default().status()
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_secs(60)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            Ok(vec![app("a")])
        }
        fn devices(&mut self) -> Result<Vec<Device>, Error> {
            if self.gate_devices.swap(false, Ordering::AcqRel) {
                self.devices_entered.send(()).expect("refresh entered");
                self.devices_release.recv().expect("release refresh");
            }
            self.script.devices()
        }
        fn set_volume(&mut self, _: &str, _: f32) -> Result<(), Error> {
            self.app_setters.fetch_add(1, Ordering::AcqRel);
            self.setter_entered.send(()).expect("setter entered");
            self.setter_release.recv().expect("release setter");
            Ok(())
        }
        fn set_mute(&mut self, _: &str, _: bool) -> Result<(), Error> {
            unreachable!("can_mute is false")
        }
        fn set_device_volume(
            &mut self,
            id: &str,
            kind: DeviceKind,
            volume: f32,
        ) -> Result<(), Error> {
            self.script.set_device_volume(id, kind, volume)
        }
    }

    /// Collects notifications until `want` has been seen (or 2 s passed).
    fn changes_until(sub: &Subscription, want: Changes) -> Changes {
        let mut seen = Changes::default();
        let deadline = std::time::Instant::now() + WAIT;
        while seen | want != seen {
            match sub.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(changes) => seen = seen | changes,
                Err(_) => break,
            }
        }
        seen
    }

    #[test]
    fn an_earlier_refresh_cannot_recover_a_later_timed_out_request() {
        let script = device_script();
        let backend = script.clone();
        let gate_devices = Arc::new(AtomicBool::new(false));
        let app_setters = Arc::new(AtomicUsize::new(0));
        let (devices_entered_tx, devices_entered) = mpsc::channel();
        let (devices_release, devices_release_rx) = mpsc::channel();
        let (setter_entered_tx, setter_entered) = mpsc::channel();
        let (setter_release, setter_release_rx) = mpsc::channel();
        let (notifier_tx, notifier_rx) = mpsc::channel();
        let (backend_gate, backend_setters) = (Arc::clone(&gate_devices), Arc::clone(&app_setters));
        let mut engine = Engine::start(
            BackendKind::None,
            Box::new(move |notifier| {
                notifier_tx.send(notifier).expect("notifier");
                Ok(Box::new(TwoGates {
                    script: backend,
                    gate_devices: backend_gate,
                    devices_entered: devices_entered_tx,
                    devices_release: devices_release_rx,
                    setter_entered: setter_entered_tx,
                    setter_release: setter_release_rx,
                    app_setters: backend_setters,
                }))
            }),
        );
        engine.command_timeout = Duration::from_millis(50);
        let sub = engine.subscribe();
        let set_app = || Command::AppVolume("a".into(), 0.5);
        let set_device = || Command::DeviceVolume("opaque UID / é".into(), DeviceKind::Output, 0.2);
        assert_eq!(engine.list_devices().expect("devices").len(), 2);

        // An earlier dirty refresh blocks inside its OS device read.
        gate_devices.store(true, Ordering::Release);
        notifier_rx.recv().expect("notifier").notify();
        devices_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("earlier refresh blocked");
        // B queues behind it and times out.
        assert_eq!(
            engine.command(set_app()).map_err(|e| e.kind),
            Err(ErrorKind::Timeout)
        );
        assert_eq!(
            engine.list_devices().map_err(|e| e.kind),
            Err(ErrorKind::Timeout)
        );
        assert_eq!(sub.try_recv(), Ok(Changes::DEVICES));

        // The earlier refresh completes; the serial worker then blocks in B.
        devices_release.send(()).expect("release refresh");
        setter_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("B reached its setter after timing out");
        assert!(is_wedged(&engine), "the earlier refresh cleared B's wedge");
        assert_eq!(
            engine.list_devices().map_err(|e| e.kind),
            Err(ErrorKind::Timeout),
            "the earlier refresh restored devices while B is blocked"
        );
        assert_eq!(
            sub.try_recv(),
            Ok(Changes::default()),
            "stale availability notification"
        );
        let started = std::time::Instant::now();
        assert_eq!(
            engine.command(set_device()).map_err(|e| e.kind),
            Err(ErrorKind::Timeout)
        );
        assert_eq!(
            engine.command(set_app()).map_err(|e| e.kind),
            Err(ErrorKind::Timeout)
        );
        assert!(started.elapsed() < Duration::from_millis(40));
        assert_eq!(app_setters.load(Ordering::Acquire), 1);
        assert!(lock(&script.calls).is_empty());

        // Servicing B and its own refresh recovers authoritative devices.
        setter_release.send(()).expect("release B");
        wait_until(|| !is_wedged(&engine));
        assert_eq!(
            engine.list_devices().expect("recovered"),
            *lock(&script.devices)
        );
        // Devices recovered; the app setter publishes its authoritative state.
        assert_eq!(
            changes_until(&sub, Changes::DEVICES | Changes::APPS),
            Changes::DEVICES | Changes::APPS
        );
        setter_release.send(()).expect("allow next setter");
        assert_eq!(engine.command(set_app()), Ok(()));
        assert_eq!(app_setters.load(Ordering::Acquire), 2);
    }

    #[test]
    fn timeouts_are_ordered_against_the_refresh_of_the_same_request() {
        let shared = Shared::new(BackendKind::None);
        let devices = || Ok(device_script().devices().expect("devices"));
        let issue = |shared: &Shared| {
            let mut requests = lock(&shared.requests);
            requests.issued += 1;
            requests.issued
        };
        let wedged = |shared: &Shared| lock(&shared.requests).wedged_by.is_some();
        let sub = shared.hub.subscribe();
        shared.publish_devices(None, devices(), false);
        assert_eq!(sub.try_recv(), Ok(Changes::DEVICES));

        // A late timeout after the request's own refresh changes nothing.
        let first = issue(&shared);
        shared.publish_devices(Some(first), devices(), false);
        shared.time_out(first);
        assert!(!wedged(&shared));
        assert!(lock(&shared.devices).is_ok());
        assert_eq!(sub.try_recv(), Ok(Changes::default()));

        // Two queued requests; only the later one times out. Neither an
        // unrelated refresh nor servicing the earlier request recovers it.
        let (earlier, later) = (issue(&shared), issue(&shared));
        shared.time_out(later);
        assert_eq!(sub.try_recv(), Ok(Changes::DEVICES));
        shared.publish_devices(None, devices(), true);
        shared.publish_devices(Some(earlier), devices(), true);
        assert!(wedged(&shared));
        assert_eq!(
            lock(&shared.devices).as_ref().map_err(|e| e.kind),
            Err(ErrorKind::Timeout)
        );
        assert_eq!(sub.try_recv(), Ok(Changes::default()));

        // A further timeout while invalid neither emits again nor lets the
        // earlier wedge owner recover in its place.
        let last = issue(&shared);
        shared.time_out(later);
        shared.time_out(last);
        assert_eq!(sub.try_recv(), Ok(Changes::default()));
        shared.publish_devices(Some(later), devices(), false);
        assert!(wedged(&shared));
        shared.publish_devices(Some(last), devices(), false);
        assert!(!wedged(&shared));
        assert!(lock(&shared.devices).is_ok());
        assert_eq!(sub.try_recv(), Ok(Changes::DEVICES));
    }

    #[test]
    fn a_panicking_worker_reports_unavailable_instead_of_going_silent() {
        let tunable = Tunable::default();
        let engine = start_tunable(&tunable);
        assert!(engine.status().available);
        let sub = engine.subscribe();
        tunable.panic_on_snapshot.store(true, Ordering::Release);
        // The reply comes first; the refresh after it panics.
        assert_eq!(set_a(&engine), Ok(()));
        assert!(eventually(|| !engine.status().available));
        assert!(engine
            .status()
            .message
            .contains("the audio thread panicked: backend exploded"));
        assert_eq!(
            set_a(&engine).map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );

        assert!(engine.list_devices().is_err());
        assert_eq!(changes_until(&sub, Changes::ALL), Changes::ALL);

        // A panic while building the backend is reported without waiting
        // for the open timeout.
        let started = std::time::Instant::now();
        let engine = Engine::start(BackendKind::None, Box::new(|_| panic!("cannot build")));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!engine.status().available);
        assert!(engine.status().message.contains("cannot build"));
    }

    #[test]
    fn unavailable_backend_reports_status_and_rejects_list() {
        let engine = Engine::start(
            BackendKind::WindowsCoreAudio,
            Box::new(|_| {
                Err(AppStatus::unavailable(
                    BackendKind::WindowsCoreAudio,
                    "no endpoint",
                ))
            }),
        );
        let status = engine.status();
        assert!(!status.available);
        assert_eq!(status.message, "no endpoint");
        assert_eq!(
            engine.list().map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );
        assert_eq!(
            engine
                .command(Command::AppVolume("a".into(), 0.5))
                .map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );
    }
}
