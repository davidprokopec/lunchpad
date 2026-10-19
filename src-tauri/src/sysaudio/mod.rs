//! System audio: live input/output devices and per-app volume, with native or
//! persistently emulated mute.
//!
//! A [`SystemAudio`] handle fronts one worker thread that owns the platform
//! backend (Core Audio on macOS, WASAPI/COM on Windows). OS notifications
//! (device list, default devices, volume/mute changes, app sessions) only ask
//! the worker to refresh; the worker keeps authoritative snapshots and tells
//! subscribers what changed ([`SystemAudio::subscribe`]). Reads come from those
//! snapshots and never block on the OS. Setters run on the worker, wait at
//! most 3 s including authoritative target readback, and publish that state
//! before returning success. They block the calling
//! thread, so async callers should use `spawn_blocking`.
//!
//! Writes are serialised FIFO: the single worker executes commands in the
//! order they were accepted (ids are issued under the same lock as the send),
//! so for any target a write accepted earlier is applied before one accepted
//! later. A write is never reordered or retried; one that cannot be served in
//! time fails with `Timeout` and later ones fail fast until the worker
//! recovers.
//!
//! Ported from LaunchDeck's `native/audio` (the same author's MIT code), minus
//! its C ABI. Per-app volume on macOS needs process taps, which are off unless
//! the `audio.macosAppVolume` setting enables them.

mod emulation;
mod engine;
mod hub;
mod model;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_taps;
#[cfg(windows)]
mod windows;

pub use engine::{BackendKind, Error, ErrorKind, Permission};
pub use hub::{Changes, Subscription};
pub use model::{App, Device, DeviceKind};

use engine::{AppStatus, Command, Engine, MakeBackend};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Weak};

/// Tauri event carrying an [`AudioState`] whenever devices, apps or the status change.
pub const EVENT_AUDIO_STATE: &str = "audio:state";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Experimental macOS process taps, off by default. Needs System Audio
    /// Recording permission and user-present revocation/capture/output-switch
    /// verification: the conditional watchdog is not a fail-open guarantee.
    pub macos_taps: bool,
}

/// Which device a control means. Defaults are resolved when the control is
/// used, so they follow the system's choice.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DeviceRef {
    DefaultOutput,
    DefaultInput,
    Named { id: String, kind: DeviceKind },
}

impl DeviceRef {
    pub fn kind(&self) -> DeviceKind {
        match self {
            DeviceRef::DefaultOutput => DeviceKind::Output,
            DeviceRef::DefaultInput => DeviceKind::Input,
            DeviceRef::Named { kind, .. } => *kind,
        }
    }

    /// The device this reference means in `devices`, if any.
    pub fn find<'a>(&self, devices: &'a [Device]) -> Option<&'a Device> {
        match self {
            DeviceRef::DefaultOutput | DeviceRef::DefaultInput => devices
                .iter()
                .find(|d| d.kind == self.kind() && d.is_default),
            DeviceRef::Named { id, kind } => {
                devices.iter().find(|d| d.id == *id && d.kind == *kind)
            }
        }
    }
}

/// What works right now. Serialises to the frozen UI shape
/// `{available, appsAvailable, permission, message?}` plus capability details.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub backend: BackendKind,
    /// Devices can be listed and controlled. False while the audio thread is
    /// busy with a request that timed out, or when the backend is unavailable.
    pub available: bool,
    /// Apps can be listed.
    pub apps_available: bool,
    /// App volume can be changed (Windows; macOS with taps on).
    pub can_set_app_volume: bool,
    /// Apps can be muted.
    pub can_mute_apps: bool,
    /// Whether the OS lets Lunchpad change app volume.
    pub permission: Permission,
    /// Why something is unavailable or limited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Status {
    fn compose(apps: AppStatus, devices: &Result<Vec<Device>, Error>) -> Status {
        let mut reasons: Vec<String> = Vec::new();
        if let Err(e) = devices {
            reasons.push(e.message.clone());
        }
        if !apps.message.is_empty() && !reasons.contains(&apps.message) {
            reasons.push(apps.message);
        }
        Status {
            backend: apps.backend,
            available: devices.is_ok(),
            apps_available: apps.available,
            can_set_app_volume: apps.available && apps.can_set_volume,
            can_mute_apps: apps.available && apps.can_mute,
            permission: apps.permission,
            message: (!reasons.is_empty()).then(|| reasons.join("; ")),
        }
    }
}

/// Everything at once, for the window: the frozen
/// `{available, appsAvailable, permission, message?, devices, apps}` payload.
/// Lists are empty while their side is unavailable; `message` says why.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioState {
    #[serde(flatten)]
    pub status: Status,
    pub devices: Vec<Device>,
    pub apps: Vec<App>,
}

/// Handle to the system audio service. Cheap to clone; the worker stops when
/// [`SystemAudio::shutdown`] is called or the last handle is dropped.
#[derive(Clone)]
pub struct SystemAudio {
    engine: Arc<Engine>,
}

impl SystemAudio {
    /// Starts the platform backend. Waits for the first snapshot (normally
    /// milliseconds, at most 5 s). Never fails: an unavailable backend is
    /// reported through [`SystemAudio::status`] and errors.
    /// Mute restore levels are memory-only; use `start_in` in the application.
    pub fn start(options: Options) -> SystemAudio {
        let (kind, make) = platform(options);
        SystemAudio::with_backend(kind, make)
    }

    /// Starts with persistent mute restore levels in the app config folder.
    /// Use this entry point in the application; `start` uses memory only.
    pub fn start_in(options: Options, config_dir: &Path) -> SystemAudio {
        let (kind, make) = platform(options);
        SystemAudio {
            engine: Arc::new(Engine::start(
                kind,
                emulation::wrap(make, Some(config_dir.to_path_buf())),
            )),
        }
    }

    fn with_backend(kind: BackendKind, make: MakeBackend) -> SystemAudio {
        SystemAudio {
            engine: Arc::new(Engine::start(kind, emulation::wrap(make, None))),
        }
    }

    /// What works right now (devices, apps, permission) and why not.
    pub fn status(&self) -> Status {
        Status::compose(self.engine.status(), &self.devices())
    }

    /// Every input and output device, keyed by `(id, kind)`, sorted by id then kind.
    pub fn devices(&self) -> Result<Vec<Device>, Error> {
        self.engine.list_devices()
    }

    /// The device `target` means right now.
    pub fn device(&self, target: &DeviceRef) -> Result<Device, Error> {
        validate_ref(target)?;
        target
            .find(&self.devices()?)
            .cloned()
            .ok_or_else(|| not_found(target))
    }

    /// Apps producing (or able to produce) audio, grouped per app.
    pub fn apps(&self) -> Result<Vec<App>, Error> {
        self.engine.list()
    }

    /// The app with this id, if it is listed now.
    pub fn app(&self, id: &str) -> Result<App, Error> {
        validate_app(id)?;
        self.apps()?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no audio app with id {id}")))
    }

    pub fn state(&self) -> AudioState {
        let devices = self.devices();
        let status = Status::compose(self.engine.status(), &devices);
        AudioState {
            status,
            devices: devices.unwrap_or_default(),
            apps: self.apps().unwrap_or_default(),
        }
    }

    /// Sets a device's volume (0..1). Blocks for at most 3 s.
    pub fn set_volume(&self, target: &DeviceRef, volume: f32) -> Result<(), Error> {
        let volume = valid_volume(volume)?;
        let device = self.device(target)?;
        self.engine
            .command(Command::DeviceVolume(device.id, device.kind, volume))
    }

    /// Uses the real mute control when available. Otherwise emulates mute
    /// with volume zero and a persisted restore level (`emulated: true`).
    /// Blocks for at most 3 s.
    pub fn set_muted(&self, target: &DeviceRef, muted: bool) -> Result<(), Error> {
        let device = self.device(target)?;
        self.engine
            .command(Command::DeviceMute(device.id, device.kind, muted))
    }

    /// Sets an app's volume (0..1). Blocks for at most 3 s.
    pub fn set_app_volume(&self, app: &str, volume: f32) -> Result<(), Error> {
        let volume = valid_volume(volume)?;
        validate_app(app)?;
        self.engine
            .command(Command::AppVolume(app.to_string(), volume))
    }

    /// Mutes or unmutes an app. Blocks for at most 3 s.
    pub fn set_app_muted(&self, app: &str, muted: bool) -> Result<(), Error> {
        validate_app(app)?;
        self.engine
            .command(Command::AppMute(app.to_string(), muted))
    }

    /// Notifications of what changed; read the new state from this handle.
    /// Coalescing and bounded: a slow subscriber never holds up the worker.
    /// Shutdown invalidates all state and closes existing/new subscriptions.
    pub fn subscribe(&self) -> Subscription {
        self.engine.subscribe()
    }

    /// Stops the worker, tearing down taps, listeners and COM objects on it.
    /// Waits at most 2 s. Every later call reports the service as stopped.
    pub fn shutdown(&self) {
        self.engine.shutdown();
    }

    fn downgrade(&self) -> Weak<Engine> {
        Arc::downgrade(&self.engine)
    }
}

/// Emits [`EVENT_AUDIO_STATE`] to the window on every change, from a thread of
/// its own. The thread does not keep the service alive.
pub fn forward_to_window(audio: &SystemAudio, app: tauri::AppHandle) {
    use tauri::Emitter;
    let subscription = audio.subscribe();
    let engine = audio.downgrade();
    let spawned = std::thread::Builder::new()
        .name("lunchpad-sysaudio-events".into())
        .spawn(move || {
            while subscription.recv().is_some() {
                let Some(engine) = engine.upgrade() else {
                    break;
                };
                let state = SystemAudio { engine }.state();
                if let Err(e) = app.emit(EVENT_AUDIO_STATE, state) {
                    tracing::debug!(error = %e, "audio state not delivered to the window");
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "audio state events could not be started");
    }
}

fn platform(options: Options) -> (BackendKind, MakeBackend) {
    #[cfg(target_os = "macos")]
    {
        (BackendKind::MacosTap, macos::make(options))
    }
    #[cfg(windows)]
    {
        let _ = options;
        (BackendKind::WindowsCoreAudio, windows::make())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = options;
        (
            BackendKind::None,
            Box::new(|_| {
                Err(AppStatus::unavailable(
                    BackendKind::None,
                    "system audio control is not supported on this platform",
                ))
            }),
        )
    }
}

fn valid_volume(volume: f32) -> Result<f32, Error> {
    if (0.0..=1.0).contains(&volume) {
        Ok(volume)
    } else {
        Err(Error::new(
            ErrorKind::InvalidArgument,
            "volume must be within 0..1",
        ))
    }
}

fn validate_ref(target: &DeviceRef) -> Result<(), Error> {
    match target {
        DeviceRef::Named { id, .. } if !model::valid_device_id(id) => {
            Err(Error::new(ErrorKind::InvalidArgument, "invalid device id"))
        }
        _ => Ok(()),
    }
}

fn validate_app(app: &str) -> Result<(), Error> {
    if model::is_valid_id(app) {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::InvalidArgument, "invalid app id"))
    }
}

fn not_found(target: &DeviceRef) -> Error {
    let what = match target {
        DeviceRef::DefaultOutput => "there is no default output device",
        DeviceRef::DefaultInput => "there is no default input device",
        DeviceRef::Named { .. } => "no audio device with this id and kind",
    };
    Error::new(ErrorKind::NotFound, what)
}

#[cfg(test)]
mod tests {
    use super::engine::Backend;
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    type Calls = Arc<Mutex<Vec<String>>>;

    /// Two outputs, one input; app "a" with volume control but no mute.
    struct Mock {
        devices: Arc<Mutex<Vec<Device>>>,
        calls: Calls,
    }

    fn device(id: &str, kind: DeviceKind, is_default: bool) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            kind,
            volume: 0.5,
            muted: false,
            is_default,
            can_set_volume: true,
            can_mute: true,
            emulated: false,
        }
    }

    impl Backend for Mock {
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
            Duration::from_secs(60)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            Ok(vec![App {
                id: "a".into(),
                name: "A".into(),
                volume: 1.0,
                muted: false,
                active: true,
                process_count: 1,
            }])
        }
        fn set_volume(&mut self, app: &str, volume: f32) -> Result<(), Error> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("app {app} {volume}"));
            Ok(())
        }
        fn set_mute(&mut self, _: &str, _: bool) -> Result<(), Error> {
            unreachable!("can_mute is false")
        }
        fn devices(&mut self) -> Result<Vec<Device>, Error> {
            Ok(self.devices.lock().unwrap().clone())
        }
        fn set_device_volume(
            &mut self,
            id: &str,
            kind: DeviceKind,
            volume: f32,
        ) -> Result<(), Error> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("volume {id} {kind:?} {volume}"));
            for device in self
                .devices
                .lock()
                .unwrap()
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
            self.calls
                .lock()
                .unwrap()
                .push(format!("mute {id} {kind:?} {muted}"));
            let mut devices = self.devices.lock().unwrap();
            for d in devices.iter_mut().filter(|d| d.id == id && d.kind == kind) {
                d.muted = muted;
            }
            Ok(())
        }
    }

    fn wait_until(check: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !check() {
            assert!(
                std::time::Instant::now() < deadline,
                "the worker did not refresh"
            );
            std::thread::yield_now();
        }
    }

    fn mock() -> (SystemAudio, Arc<Mutex<Vec<Device>>>, Calls) {
        let devices = Arc::new(Mutex::new(vec![
            device("speakers", DeviceKind::Output, true),
            device("headset", DeviceKind::Output, false),
            device("headset", DeviceKind::Input, true),
        ]));
        let calls: Calls = Arc::default();
        let backend = Mock {
            devices: Arc::clone(&devices),
            calls: Arc::clone(&calls),
        };
        (
            SystemAudio::with_backend(BackendKind::None, Box::new(move |_| Ok(Box::new(backend)))),
            devices,
            calls,
        )
    }

    #[test]
    fn references_resolve_defaults_when_used_and_named_devices_by_id_and_kind() {
        let (audio, devices, calls) = mock();
        audio.set_volume(&DeviceRef::DefaultOutput, 0.3).unwrap();
        audio.set_muted(&DeviceRef::DefaultInput, true).unwrap();
        audio
            .set_muted(
                &DeviceRef::Named {
                    id: "headset".into(),
                    kind: DeviceKind::Output,
                },
                true,
            )
            .unwrap();
        // The user switches the default output; the next use follows it.
        for d in devices
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|d| d.kind == DeviceKind::Output)
        {
            d.is_default = d.id == "headset";
        }
        // The switch is picked up by the refresh after the next command (or an OS notification).
        audio.set_app_volume("a", 0.25).unwrap();
        wait_until(|| {
            audio
                .device(&DeviceRef::DefaultOutput)
                .is_ok_and(|d| d.id == "headset")
        });
        audio.set_volume(&DeviceRef::DefaultOutput, 0.7).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "volume speakers Output 0.3",
                "mute headset Input true",
                "mute headset Output true",
                "app a 0.25",
                "volume headset Output 0.7"
            ]
        );
        // Setters publish the authoritative state afterwards.
        assert!(audio.device(&DeviceRef::DefaultInput).unwrap().muted);
    }

    #[test]
    fn invalid_arguments_are_rejected_before_the_worker() {
        let (audio, _, calls) = mock();
        let kind = |r: Result<(), Error>| r.map_err(|e| e.kind);
        assert_eq!(
            kind(audio.set_volume(&DeviceRef::DefaultOutput, f32::NAN)),
            Err(ErrorKind::InvalidArgument)
        );
        assert_eq!(
            kind(audio.set_volume(&DeviceRef::DefaultOutput, 1.5)),
            Err(ErrorKind::InvalidArgument)
        );
        assert_eq!(
            kind(audio.set_muted(
                &DeviceRef::Named {
                    id: "bad\nid".into(),
                    kind: DeviceKind::Input
                },
                true
            )),
            Err(ErrorKind::InvalidArgument)
        );
        assert_eq!(
            kind(audio.set_muted(
                &DeviceRef::Named {
                    id: "speakers".into(),
                    kind: DeviceKind::Input
                },
                true
            )),
            Err(ErrorKind::NotFound)
        );
        assert_eq!(
            kind(audio.set_app_volume("not an id", 0.5)),
            Err(ErrorKind::InvalidArgument)
        );
        assert_eq!(
            kind(audio.set_app_volume("missing", 0.5)),
            Err(ErrorKind::NotFound)
        );
        assert_eq!(
            kind(audio.set_app_muted("a", true)),
            Err(ErrorKind::Unsupported)
        );
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_default_that_does_not_exist_is_not_found() {
        let (audio, devices, calls) = mock();
        devices
            .lock()
            .unwrap()
            .retain(|d| d.kind == DeviceKind::Output);
        audio.set_app_volume("a", 0.5).unwrap();
        wait_until(|| {
            audio
                .devices()
                .is_ok_and(|d| d.iter().all(|d| d.kind == DeviceKind::Output))
        });
        assert_eq!(
            audio
                .set_muted(&DeviceRef::DefaultInput, true)
                .map_err(|e| e.kind),
            Err(ErrorKind::NotFound)
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn status_reports_devices_and_apps_independently() {
        let apps_ok = AppStatus {
            backend: BackendKind::None,
            available: true,
            can_set_volume: true,
            can_mute: true,
            permission: Permission::Granted,
            message: String::new(),
        };
        let ok = Status::compose(apps_ok.clone(), &Ok(vec![]));
        assert!(ok.available && ok.apps_available && ok.can_mute_apps && ok.message.is_none());

        let busy = Error::new(ErrorKind::Timeout, "the audio thread is busy");
        let busy = Status::compose(apps_ok, &Err(busy));
        assert!(!busy.available && busy.apps_available);
        assert_eq!(busy.message.as_deref(), Some("the audio thread is busy"));

        let old_macos = AppStatus::unavailable(BackendKind::MacosTap, "needs macOS 14.2");
        let no_apps = Status::compose(old_macos, &Ok(vec![]));
        assert!(no_apps.available && !no_apps.apps_available && !no_apps.can_set_app_volume);
        assert_eq!(no_apps.permission, Permission::Unknown);
        assert_eq!(no_apps.message.as_deref(), Some("needs macOS 14.2"));

        let stopped = AppStatus::unavailable(BackendKind::None, "stopped");
        let both = Status::compose(stopped, &Err(Error::new(ErrorKind::Unsupported, "stopped")));
        assert_eq!(
            both.message.as_deref(),
            Some("stopped"),
            "a shared reason is not repeated"
        );

        let values = [
            Permission::Unknown,
            Permission::Granted,
            Permission::Denied,
            Permission::Unconfirmed,
        ]
        .map(|p| serde_json::to_value(p).unwrap());
        assert_eq!(values, ["unknown", "granted", "denied", "unconfirmed"]);
    }

    #[test]
    fn single_apps_are_read_from_the_snapshot() {
        let (audio, _, _) = mock();
        assert_eq!(audio.app("a").unwrap().name, "A");
        assert_eq!(
            audio.app("missing").map_err(|e| e.kind),
            Err(ErrorKind::NotFound)
        );
        assert_eq!(
            audio.app("not an id").map_err(|e| e.kind),
            Err(ErrorKind::InvalidArgument)
        );
    }

    #[test]
    fn rapid_taps_read_the_completed_mute_before_the_next_tap() {
        for real_mute in [false, true] {
            let raw = Arc::new(Mutex::new(vec![Device {
                can_mute: real_mute,
                ..device("mic", DeviceKind::Input, true)
            }]));
            let backend = Mock {
                devices: Arc::clone(&raw),
                calls: Arc::default(),
            };
            let audio = SystemAudio::with_backend(
                BackendKind::None,
                Box::new(move |_| Ok(Box::new(backend))),
            );
            for _ in 0..50 {
                let before = audio.device(&DeviceRef::DefaultInput).unwrap();
                audio
                    .set_muted(&DeviceRef::DefaultInput, !before.muted)
                    .unwrap();
                let after = audio.device(&DeviceRef::DefaultInput).unwrap();
                assert_eq!(
                    after.muted, !before.muted,
                    "setters must complete readback before returning"
                );
                assert_eq!(after.emulated, !real_mute);
                assert_eq!(after.volume, 0.5);
                assert_eq!(
                    raw.lock().unwrap()[0].volume,
                    if after.muted && !real_mute { 0.0 } else { 0.5 }
                );
            }
        }
    }

    #[test]
    fn a_persisted_mute_survives_service_restart_and_keeps_the_new_restore_volume() {
        let dir = std::env::temp_dir().join(format!("lunchpad-service-{}", uuid::Uuid::new_v4()));
        let raw = Arc::new(Mutex::new(vec![Device {
            can_mute: false,
            ..device("mic", DeviceKind::Input, true)
        }]));
        let start = || {
            let backend = Mock {
                devices: Arc::clone(&raw),
                calls: Arc::default(),
            };
            let make: MakeBackend = Box::new(move |_| Ok(Box::new(backend)));
            SystemAudio {
                engine: Arc::new(Engine::start(
                    BackendKind::None,
                    emulation::wrap(make, Some(dir.clone())),
                )),
            }
        };
        let audio = start();
        audio.set_muted(&DeviceRef::DefaultInput, true).unwrap();
        audio.set_volume(&DeviceRef::DefaultInput, 0.7).unwrap();
        assert_eq!(
            raw.lock().unwrap()[0].volume,
            0.0,
            "a volume set while muted only changes the restore level"
        );
        assert_eq!(audio.device(&DeviceRef::DefaultInput).unwrap().volume, 0.7);
        drop(audio);
        let restarted = start();
        assert!(restarted.device(&DeviceRef::DefaultInput).unwrap().muted);
        restarted
            .set_muted(&DeviceRef::DefaultInput, false)
            .unwrap();
        assert_eq!(raw.lock().unwrap()[0].volume, 0.7);
        assert!(!restarted.device(&DeviceRef::DefaultInput).unwrap().muted);
        drop(restarted);
        assert!(!start().device(&DeviceRef::DefaultInput).unwrap().muted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn state_and_references_serialise_for_the_window() {
        let (audio, _, _) = mock();
        let state = serde_json::to_value(audio.state()).unwrap();
        // The frozen payload: {available, appsAvailable, permission, message?, devices, apps}.
        assert_eq!(state["available"], true);
        assert_eq!(state["appsAvailable"], true);
        assert_eq!(state["permission"], "granted");
        assert!(
            state.get("message").is_none(),
            "message only when something is limited"
        );
        assert_eq!(state["backend"], "none");
        assert_eq!(state["canSetAppVolume"], true);
        assert_eq!(state["canMuteApps"], false);
        assert_eq!(state["devices"].as_array().unwrap().len(), 3);
        assert_eq!(state["devices"][0]["emulated"], false);
        assert_eq!(state["apps"][0]["id"], "a");
        assert_eq!(
            serde_json::to_value(DeviceRef::DefaultInput).unwrap(),
            serde_json::json!({"type": "defaultInput"})
        );
        let named: DeviceRef = serde_json::from_value(
            serde_json::json!({"type": "named", "id": "x", "kind": "input"}),
        )
        .unwrap();
        assert_eq!(
            named,
            DeviceRef::Named {
                id: "x".into(),
                kind: DeviceKind::Input
            }
        );
        let error = serde_json::to_value(Error::new(ErrorKind::Timeout, "busy")).unwrap();
        assert_eq!(
            error,
            serde_json::json!({"kind": "timeout", "message": "busy"})
        );
    }

    #[test]
    fn a_stopped_service_reports_errors_instead_of_stale_state() {
        let (audio, _, _) = mock();
        let watcher = audio.clone();
        let sub = watcher.subscribe();
        audio.shutdown();
        assert_eq!(sub.recv(), Some(Changes::ALL));
        assert_eq!(sub.recv(), None);
        let state = watcher.state();
        assert!(state.devices.is_empty());
        assert!(!state.status.available);
        assert!(state.apps.is_empty());
        assert!(
            !state.status.apps_available
                && !state.status.can_set_app_volume
                && !state.status.can_mute_apps
        );
        assert!(state.status.message.is_some());
        assert_eq!(
            watcher
                .set_muted(&DeviceRef::DefaultInput, true)
                .map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );
    }

    /// The real platform backend, read-only: status and lists, never a setter.
    #[test]
    fn the_platform_backend_starts_and_lists_without_taps() {
        let audio = SystemAudio::start(Options::default());
        let status = audio.status();
        assert!(
            !status.can_set_app_volume || !cfg!(target_os = "macos"),
            "taps stay off by default"
        );
        if let Ok(devices) = audio.devices() {
            let mut keys = std::collections::HashSet::new();
            for d in &devices {
                assert!(model::valid_device_id(&d.id));
                assert!(keys.insert((d.id.clone(), d.kind)), "(id, kind) is unique");
                assert!((0.0..=1.0).contains(&d.volume));
            }
            for kind in [DeviceKind::Output, DeviceKind::Input] {
                assert!(
                    devices
                        .iter()
                        .filter(|d| d.kind == kind && d.is_default)
                        .count()
                        <= 1
                );
            }
        }
        if let Ok(apps) = audio.apps() {
            assert!(apps.iter().all(|a| model::is_valid_id(&a.id)));
        }
        let started = std::time::Instant::now();
        audio.shutdown();
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
