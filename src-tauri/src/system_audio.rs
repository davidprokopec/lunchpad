//! The system's audio as the macro engine sees it: a small port
//! ([`AudioControl`]) that a platform backend implements, the snapshot the
//! LEDs and faders read, and [`SystemAudio`], the handle actions and faders use.
//!
//! Every write goes through one writer thread in the order it was asked for,
//! so a press's "mute" always reaches the system before its release's
//! "restore". Each write resolves its target against a fresh snapshot (the
//! default input may have changed since the last event).

use crate::live::SharedLive;
use crate::macros::SetAudioMode;
use crate::profile::{AppRef, AudioDeviceKind, AudioTarget, DeviceRef, Profile};
use crossbeam_channel::{unbounded, Receiver, Sender};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, watch};

/// How long a release waits for its press to have taken the mute state.
const HOLD_SLOT_WAIT: Duration = Duration::from_secs(1);

/// One endpoint in one direction, as the system reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    /// Opaque system id (Core Audio UID, Windows endpoint id)
    pub id: String,
    pub name: String,
    pub kind: AudioDeviceKind,
    /// 0..1
    pub volume: f32,
    pub muted: bool,
    pub is_default: bool,
    pub can_set_volume: bool,
    pub can_mute: bool,
    /// The device has no mute of its own; muting parks the volume at 0 and remembers the level.
    pub emulated: bool,
    /// False for a device a profile names that the system does not list right now.
    pub present: bool,
}

/// One application with audio, as the system reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioApp {
    pub id: String,
    pub name: String,
    /// 0..1
    pub volume: f32,
    pub muted: bool,
    /// Producing sound right now
    pub active: bool,
    /// False for an app a profile names that is not running.
    pub present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioPermission {
    #[default]
    Unknown,
    Granted,
    Denied,
    /// Asked for, but the system has not confirmed it works yet ("play audio, then retry")
    Unconfirmed,
}

/// What the backend can do right now.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStatus {
    /// Devices can be listed and controlled.
    pub available: bool,
    /// Apps can be listed (and, with the permission, controlled).
    pub apps_available: bool,
    pub permission: AudioPermission,
    /// Why something is unavailable.
    pub message: Option<String>,
}

/// The port a platform backend implements. Calls may block briefly; Lunchpad
/// makes them from its own threads, never from the async runtime. Volumes are 0..1.
pub trait AudioControl: Send + Sync + 'static {
    fn status(&self) -> AudioStatus;
    /// Every device in both directions; `present` is true for all of them.
    fn devices(&self) -> Result<Vec<AudioDevice>, String>;
    /// Every app with audio; `present` is true for all of them.
    fn apps(&self) -> Result<Vec<AudioApp>, String>;
    fn set_device_volume(&self, id: &str, kind: AudioDeviceKind, volume: f32)
        -> Result<(), String>;
    /// On a device with `emulated` set this parks the volume at 0 (and brings it back).
    fn set_device_muted(&self, id: &str, kind: AudioDeviceKind, muted: bool) -> Result<(), String>;
    fn set_app_volume(&self, id: &str, volume: f32) -> Result<(), String>;
    fn set_app_muted(&self, id: &str, muted: bool) -> Result<(), String>;
    /// One message per change of devices, apps or status (bursts are fine; the
    /// bridge coalesces them). The channel closing ends the subscription.
    fn subscribe(&self) -> Receiver<()>;
}

/// The port when no backend is wired in: nothing is available.
pub struct NoAudio;

impl AudioControl for NoAudio {
    fn status(&self) -> AudioStatus {
        AudioStatus {
            message: Some("system audio is not available in this build".into()),
            ..AudioStatus::default()
        }
    }
    fn devices(&self) -> Result<Vec<AudioDevice>, String> {
        Err("system audio is not available".into())
    }
    fn apps(&self) -> Result<Vec<AudioApp>, String> {
        Err("system audio is not available".into())
    }
    fn set_device_volume(&self, _: &str, _: AudioDeviceKind, _: f32) -> Result<(), String> {
        Err("system audio is not available".into())
    }
    fn set_device_muted(&self, _: &str, _: AudioDeviceKind, _: bool) -> Result<(), String> {
        Err("system audio is not available".into())
    }
    fn set_app_volume(&self, _: &str, _: f32) -> Result<(), String> {
        Err("system audio is not available".into())
    }
    fn set_app_muted(&self, _: &str, _: bool) -> Result<(), String> {
        Err("system audio is not available".into())
    }
    fn subscribe(&self) -> Receiver<()> {
        // The sender is gone at once: the subscription ends after the first look.
        unbounded().1
    }
}

/// The system audio as last seen, kept in the live status.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemAudioState {
    pub available: bool,
    pub apps_available: bool,
    pub devices: Vec<AudioDevice>,
    pub apps: Vec<AudioApp>,
}

/// What a target addresses in a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    Device { id: String, kind: AudioDeviceKind },
    App { id: String },
}

/// A target found in a snapshot, with its state.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetState {
    pub key: Resolved,
    pub volume: f32,
    pub muted: bool,
    pub can_mute: bool,
    pub emulated: bool,
}

impl SystemAudioState {
    /// The target's state, or `None` while it is not there (or audio is unavailable).
    pub fn resolve(&self, target: &AudioTarget) -> Option<TargetState> {
        match target {
            AudioTarget::Device { device } => {
                if !self.available {
                    return None;
                }
                let kind = device.kind();
                let found = self
                    .devices
                    .iter()
                    .filter(|d| d.present && d.kind == kind)
                    .find(|d| match device {
                        DeviceRef::DefaultInput | DeviceRef::DefaultOutput => d.is_default,
                        DeviceRef::Named { id, label, .. } if id.is_empty() => {
                            d.name.eq_ignore_ascii_case(label.trim())
                        }
                        DeviceRef::Named { id, .. } => d.id == *id,
                    })?;
                Some(TargetState {
                    key: Resolved::Device {
                        id: found.id.clone(),
                        kind,
                    },
                    volume: found.volume,
                    muted: found.muted,
                    can_mute: found.can_mute || found.emulated,
                    emulated: found.emulated,
                })
            }
            AudioTarget::App { app } => {
                if !self.apps_available {
                    return None;
                }
                let found = self.apps.iter().find(|a| a.present && a.id == app.id)?;
                Some(TargetState {
                    key: Resolved::App {
                        id: found.id.clone(),
                    },
                    volume: found.volume,
                    muted: found.muted,
                    can_mute: true,
                    emulated: false,
                })
            }
        }
    }
}

/// A fresh look at the system.
pub fn read_state(port: &dyn AudioControl) -> SystemAudioState {
    let status = port.status();
    let devices = if status.available {
        port.devices().ok()
    } else {
        None
    };
    let apps = if status.apps_available {
        port.apps().ok()
    } else {
        None
    };
    SystemAudioState {
        available: devices.is_some(),
        apps_available: apps.is_some(),
        devices: devices.unwrap_or_default(),
        apps: apps.unwrap_or_default(),
    }
}

/// What `audio_status` and the `system-audio:changed` event carry.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioReport {
    pub available: bool,
    pub apps_available: bool,
    pub permission: AudioPermission,
    pub message: Option<String>,
    pub devices: Vec<AudioDevice>,
    pub apps: Vec<AudioApp>,
}

/// The report: the snapshot plus every device and app the profile names that is
/// not there right now (`present: false`), so pickers keep showing them.
pub fn report(status: AudioStatus, state: &SystemAudioState, profile: &Profile) -> AudioReport {
    let mut devices = state.devices.clone();
    let mut apps = state.apps.clone();
    for target in saved_targets(profile) {
        match target {
            AudioTarget::Device {
                device: DeviceRef::Named { id, kind, label },
            } => {
                if !id.is_empty() && !devices.iter().any(|d| d.id == id && d.kind == kind) {
                    devices.push(AudioDevice {
                        id,
                        name: label,
                        kind,
                        volume: 0.0,
                        muted: false,
                        is_default: false,
                        can_set_volume: false,
                        can_mute: false,
                        emulated: false,
                        present: false,
                    });
                }
            }
            AudioTarget::Device { .. } => {}
            AudioTarget::App {
                app: AppRef { id, name },
            } => {
                if !apps.iter().any(|a| a.id == id) {
                    apps.push(AudioApp {
                        id,
                        name,
                        volume: 0.0,
                        muted: false,
                        active: false,
                        present: false,
                    });
                }
            }
        }
    }
    AudioReport {
        available: state.available,
        apps_available: state.apps_available,
        permission: status.permission,
        message: status.message,
        devices,
        apps,
    }
}

/// Every audio target the profile refers to: bound faders, System links, SetAudio actions.
pub fn saved_targets(profile: &Profile) -> Vec<AudioTarget> {
    use crate::macros::{Action, ActionKind};
    let mut out: Vec<AudioTarget> = Vec::new();
    let mut add = |t: &AudioTarget| {
        if !out.contains(t) {
            out.push(t.clone());
        }
    };
    let from_actions = |list: &[Action], add: &mut dyn FnMut(&AudioTarget)| {
        for a in list {
            if let ActionKind::SetAudio { target, .. } = &a.kind {
                add(target);
            }
        }
    };
    for page in &profile.pages {
        for b in &page.buttons {
            if let Some(t) = b.button.state_link.as_ref().and_then(|l| l.target.as_ref()) {
                add(t);
            }
            for list in [&b.button.down, &b.button.up, &b.button.hold] {
                from_actions(list, &mut add);
            }
        }
        for f in &page.faders {
            if let Some(t) = &f.bound {
                add(t);
            }
            for list in [&f.on_change, &f.on_touch, &f.on_release] {
                from_actions(list, &mut add);
            }
        }
    }
    out
}

fn set_muted(port: &dyn AudioControl, key: &Resolved, muted: bool) -> Result<(), String> {
    match key {
        Resolved::Device { id, kind } => port.set_device_muted(id, *kind, muted),
        Resolved::App { id } => port.set_app_muted(id, muted),
    }
}

fn set_volume(port: &dyn AudioControl, key: &Resolved, volume: f32) -> Result<(), String> {
    let volume = if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        0.0
    };
    match key {
        Resolved::Device { id, kind } => port.set_device_volume(id, *kind, volume),
        Resolved::App { id } => port.set_app_volume(id, volume),
    }
}

fn fresh(port: &dyn AudioControl, target: &AudioTarget) -> Result<TargetState, String> {
    read_state(port)
        .resolve(target)
        .ok_or_else(|| format!("{} is not available", describe(target)))
}

fn describe(target: &AudioTarget) -> String {
    match target {
        AudioTarget::Device {
            device: DeviceRef::DefaultInput,
        } => "the default input".into(),
        AudioTarget::Device {
            device: DeviceRef::DefaultOutput,
        } => "the default output".into(),
        AudioTarget::Device {
            device: DeviceRef::Named { id, label, .. },
        } => format!("device \"{}\"", if label.is_empty() { id } else { label }),
        AudioTarget::App { app } => format!(
            "app \"{}\"",
            if app.name.is_empty() {
                &app.id
            } else {
                &app.name
            }
        ),
    }
}

/// The pad a hold-to-mute press belongs to: (page, x, y).
pub type PadKey = (String, u8, u8);

/// What a hold-to-mute press saw before it muted.
struct HoldSlot {
    /// When the pad went down; the release must belong to the same press.
    down: Option<Instant>,
    /// Muted before the press; `None` when the target could not be read.
    captured: Option<bool>,
}

struct HoldMutes {
    slots: Mutex<HashMap<PadKey, HoldSlot>>,
    /// Bumped whenever a slot is stored, so a waiting release looks again.
    stored: watch::Sender<u64>,
}

type Job = Box<dyn FnOnce(&dyn AudioControl) + Send>;

/// The engine's handle to the system audio. Clone freely.
#[derive(Clone)]
pub struct SystemAudio {
    port: Arc<dyn AudioControl>,
    live: SharedLive,
    jobs: Sender<Job>,
    holds: Arc<HoldMutes>,
}

impl SystemAudio {
    /// Start the writer thread for `port`; `live` is where the bridge keeps the snapshot.
    pub fn new(port: Arc<dyn AudioControl>, live: SharedLive) -> SystemAudio {
        let (jobs, rx) = unbounded::<Job>();
        let worker = port.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("lunchpad-audio-write".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job(worker.as_ref());
                }
            })
        {
            tracing::warn!(error = %e, "the system audio writer could not start");
        }
        SystemAudio {
            port,
            live,
            jobs,
            holds: Arc::new(HoldMutes {
                slots: Mutex::new(HashMap::new()),
                stored: watch::channel(0).0,
            }),
        }
    }

    pub fn port(&self) -> &Arc<dyn AudioControl> {
        &self.port
    }

    /// Devices can be controlled right now.
    pub fn available(&self) -> bool {
        self.port.status().available
    }

    /// The target as the last change event left it (what the LEDs show).
    pub fn cached(&self, target: &AudioTarget) -> Option<TargetState> {
        self.live.lock().system.resolve(target)
    }

    /// Run `f` on the writer thread, after every write asked for before it.
    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn AudioControl) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Box::new(move |port| {
                let _ = tx.send(f(port));
            }))
            .map_err(|_| "the system audio writer has stopped".to_string())?;
        rx.await
            .map_err(|_| "the system audio writer has stopped".to_string())?
    }

    /// A fader's level (0..1) from a press: unmutes a muted target first, then sets the volume.
    pub fn set_level(&self, target: AudioTarget, volume: f32) {
        let job: Job = Box::new(move |port| {
            let result = fresh(port, &target).and_then(|state| {
                if state.muted {
                    set_muted(port, &state.key, false)?;
                }
                set_volume(port, &state.key, volume)
            });
            if let Err(e) = result {
                tracing::warn!(error = %e, "the fader could not set the system volume");
            }
        });
        if self.jobs.send(job).is_err() {
            tracing::warn!("the system audio writer has stopped");
        }
    }

    /// Every mode but `HoldMute`; `amount` is percent (the level, or the step for adjust).
    pub async fn apply(
        &self,
        target: AudioTarget,
        mode: SetAudioMode,
        amount: f32,
    ) -> Result<(), String> {
        self.run(move |port| {
            let state = fresh(port, &target)?;
            match mode {
                SetAudioMode::SetVolume => set_volume(port, &state.key, amount / 100.0),
                SetAudioMode::AdjustVolume => {
                    set_volume(port, &state.key, state.volume + amount / 100.0)
                }
                SetAudioMode::Mute => set_muted(port, &state.key, true),
                SetAudioMode::Unmute => set_muted(port, &state.key, false),
                SetAudioMode::ToggleMute | SetAudioMode::HoldMute => {
                    set_muted(port, &state.key, !state.muted)
                }
            }
        })
        .await
    }

    /// The press of a hold-to-mute pad: remember whether the target was muted, then mute it.
    pub async fn hold_press(
        &self,
        key: PadKey,
        down: Option<Instant>,
        target: AudioTarget,
    ) -> Result<(), String> {
        let holds = self.holds.clone();
        self.run(move |port| {
            let state = fresh(port, &target);
            let captured = state.as_ref().ok().map(|s| s.muted);
            holds.slots.lock().insert(key, HoldSlot { down, captured });
            holds.stored.send_modify(|n| *n = n.wrapping_add(1));
            let state = state?;
            if state.muted {
                Ok(())
            } else {
                set_muted(port, &state.key, true)
            }
        })
        .await
    }

    /// The release of a hold-to-mute pad: a tap leaves the target the other way round
    /// from before the press, a hold brings back what it was (whatever changed meanwhile).
    pub async fn hold_release(
        &self,
        key: PadKey,
        down: Option<Instant>,
        tapped: bool,
        target: AudioTarget,
    ) -> Result<(), String> {
        let mut stored = self.holds.stored.subscribe();
        let deadline = tokio::time::Instant::now() + HOLD_SLOT_WAIT;
        let captured = loop {
            {
                let mut slots = self.holds.slots.lock();
                if slots.get(&key).is_some_and(|s| s.down == down) {
                    break slots.remove(&key).and_then(|s| s.captured);
                }
            }
            match tokio::time::timeout_at(deadline, stored.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => {
                    tracing::warn!(page = %key.0, x = key.1, y = key.2, "hold-to-mute release without its press, nothing to restore");
                    return Ok(());
                }
            }
        };
        let Some(was_muted) = captured else {
            // The press could not read the target and has said so.
            return Ok(());
        };
        let wanted = if tapped { !was_muted } else { was_muted };
        self.run(move |port| {
            let state = fresh(port, &target)?;
            if state.muted == wanted {
                Ok(())
            } else {
                set_muted(port, &state.key, wanted)
            }
        })
        .await
    }
}

/// An in-memory backend: tests, and a stand-in while no platform backend is wired.
/// Devices with `emulated` mute by parking the volume at 0, the way the real
/// backend handles devices without a mute control.
#[derive(Default)]
pub struct FakeAudio {
    state: Mutex<FakeState>,
    subscribers: Mutex<Vec<Sender<()>>>,
}

#[derive(Default)]
struct FakeState {
    status: AudioStatus,
    devices: Vec<AudioDevice>,
    apps: Vec<AudioApp>,
    /// Levels parked by an emulated mute, by device id
    parked: HashMap<String, f32>,
    writes: Vec<FakeWrite>,
    reads: usize,
}

/// A write the fake received.
#[derive(Debug, Clone, PartialEq)]
pub enum FakeWrite {
    Volume(String, f32),
    Muted(String, bool),
}

impl FakeAudio {
    pub fn new(devices: Vec<AudioDevice>, apps: Vec<AudioApp>) -> FakeAudio {
        let fake = FakeAudio::default();
        {
            let mut st = fake.state.lock();
            st.status = AudioStatus {
                available: true,
                apps_available: true,
                permission: AudioPermission::Granted,
                message: None,
            };
            st.devices = devices;
            st.apps = apps;
        }
        fake
    }

    /// A present device with a volume and a mute control.
    pub fn device(id: &str, kind: AudioDeviceKind, is_default: bool, volume: f32) -> AudioDevice {
        AudioDevice {
            id: id.into(),
            name: id.into(),
            kind,
            volume,
            muted: false,
            is_default,
            can_set_volume: true,
            can_mute: true,
            emulated: false,
            present: true,
        }
    }

    pub fn writes(&self) -> Vec<FakeWrite> {
        self.state.lock().writes.clone()
    }

    /// How often the device list was read.
    pub fn reads(&self) -> usize {
        self.state.lock().reads
    }

    /// Change the system from "outside" (another app, the OS's own controls).
    pub fn change(&self, f: impl FnOnce(&mut Vec<AudioDevice>, &mut Vec<AudioApp>)) {
        {
            let mut st = self.state.lock();
            let st = &mut *st;
            f(&mut st.devices, &mut st.apps);
        }
        self.notify();
    }

    pub fn set_status(&self, status: AudioStatus) {
        self.state.lock().status = status;
        self.notify();
    }

    fn notify(&self) {
        self.subscribers.lock().retain(|tx| tx.send(()).is_ok());
    }
}

impl AudioControl for FakeAudio {
    fn status(&self) -> AudioStatus {
        self.state.lock().status.clone()
    }
    fn devices(&self) -> Result<Vec<AudioDevice>, String> {
        let mut st = self.state.lock();
        st.reads += 1;
        Ok(st.devices.clone())
    }
    fn apps(&self) -> Result<Vec<AudioApp>, String> {
        Ok(self.state.lock().apps.clone())
    }
    fn set_device_volume(
        &self,
        id: &str,
        kind: AudioDeviceKind,
        volume: f32,
    ) -> Result<(), String> {
        {
            let mut st = self.state.lock();
            let d = st
                .devices
                .iter_mut()
                .find(|d| d.id == id && d.kind == kind)
                .ok_or("no such device")?;
            d.volume = volume;
            st.writes.push(FakeWrite::Volume(id.into(), volume));
        }
        self.notify();
        Ok(())
    }
    fn set_device_muted(&self, id: &str, kind: AudioDeviceKind, muted: bool) -> Result<(), String> {
        {
            let mut st = self.state.lock();
            let st = &mut *st;
            let d = st
                .devices
                .iter_mut()
                .find(|d| d.id == id && d.kind == kind)
                .ok_or("no such device")?;
            if d.emulated {
                if muted && !d.muted {
                    st.parked.insert(id.into(), d.volume);
                    d.volume = 0.0;
                } else if !muted && d.muted {
                    d.volume = st.parked.remove(id).unwrap_or(0.75);
                }
            } else if !d.can_mute {
                return Err("this device cannot be muted".into());
            }
            d.muted = muted;
            st.writes.push(FakeWrite::Muted(id.into(), muted));
        }
        self.notify();
        Ok(())
    }
    fn set_app_volume(&self, id: &str, volume: f32) -> Result<(), String> {
        {
            let mut st = self.state.lock();
            let a = st
                .apps
                .iter_mut()
                .find(|a| a.id == id)
                .ok_or("no such app")?;
            a.volume = volume;
            st.writes.push(FakeWrite::Volume(id.into(), volume));
        }
        self.notify();
        Ok(())
    }
    fn set_app_muted(&self, id: &str, muted: bool) -> Result<(), String> {
        {
            let mut st = self.state.lock();
            let a = st
                .apps
                .iter_mut()
                .find(|a| a.id == id)
                .ok_or("no such app")?;
            a.muted = muted;
            st.writes.push(FakeWrite::Muted(id.into(), muted));
        }
        self.notify();
        Ok(())
    }
    fn subscribe(&self) -> Receiver<()> {
        let (tx, rx) = unbounded();
        self.subscribers.lock().push(tx);
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AudioDeviceKind::*;

    fn mic_target() -> AudioTarget {
        AudioTarget::Device {
            device: DeviceRef::DefaultInput,
        }
    }

    #[test]
    fn targets_resolve_against_the_snapshot() {
        let fake = FakeAudio::new(
            vec![
                FakeAudio::device("mic1", Input, false, 0.2),
                FakeAudio::device("mic2", Input, true, 0.6),
                FakeAudio::device("spk", Output, true, 0.4),
            ],
            vec![AudioApp {
                id: "com.spotify.client".into(),
                name: "Spotify".into(),
                volume: 0.9,
                muted: true,
                active: true,
                present: true,
            }],
        );
        let state = read_state(&fake);
        assert_eq!(
            state.resolve(&mic_target()).map(|s| s.key),
            Some(Resolved::Device {
                id: "mic2".into(),
                kind: Input
            })
        );
        let named = AudioTarget::Device {
            device: DeviceRef::Named {
                id: "mic1".into(),
                kind: Input,
                label: "whatever".into(),
            },
        };
        assert_eq!(state.resolve(&named).map(|s| s.volume), Some(0.2));
        // A named device of the other kind is not the same endpoint.
        let wrong_kind = AudioTarget::Device {
            device: DeviceRef::Named {
                id: "mic1".into(),
                kind: Output,
                label: String::new(),
            },
        };
        assert!(state.resolve(&wrong_kind).is_none());
        // Without an id, the label is what is matched.
        let by_label = AudioTarget::Device {
            device: DeviceRef::Named {
                id: String::new(),
                kind: Output,
                label: "SPK".into(),
            },
        };
        assert!(state.resolve(&by_label).is_some());
        let app = AudioTarget::App {
            app: AppRef {
                id: "com.spotify.client".into(),
                name: String::new(),
            },
        };
        assert_eq!(state.resolve(&app).map(|s| s.muted), Some(true));
        assert!(state
            .resolve(&AudioTarget::App {
                app: AppRef {
                    id: "gone".into(),
                    name: String::new()
                }
            })
            .is_none());
        // Nothing resolves while audio is unavailable.
        assert!(read_state(&NoAudio).resolve(&mic_target()).is_none());
    }

    #[test]
    fn the_report_lists_saved_targets_that_are_not_there() {
        use crate::profile::{Fader, DEFAULT_PAGE_ID};
        let fake = FakeAudio::new(vec![FakeAudio::device("mic", Input, true, 0.5)], vec![]);
        let mut profile = Profile::default();
        let page = profile.page_mut(DEFAULT_PAGE_ID).unwrap();
        page.set_fader(Fader {
            id: "a".into(),
            bound: Some(AudioTarget::App {
                app: AppRef {
                    id: "com.discord".into(),
                    name: "Discord".into(),
                },
            }),
            ..Fader::default()
        });
        page.set_fader(Fader {
            id: "b".into(),
            x: 1,
            bound: Some(AudioTarget::Device {
                device: DeviceRef::Named {
                    id: "usb".into(),
                    kind: Input,
                    label: "USB Mic".into(),
                },
            }),
            ..Fader::default()
        });
        let report = report(fake.status(), &read_state(&fake), &profile);
        assert!(report.available && report.apps_available);
        assert_eq!(report.apps.len(), 1);
        assert_eq!(
            (report.apps[0].name.as_str(), report.apps[0].present),
            ("Discord", false)
        );
        assert_eq!(report.devices.len(), 2);
        assert_eq!(
            (report.devices[1].name.as_str(), report.devices[1].present),
            ("USB Mic", false)
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["permission"], "granted");
        assert!(
            json["devices"][0].get("isDefault").is_some()
                && json["devices"][0].get("canMute").is_some()
        );
    }

    #[tokio::test]
    async fn writes_run_in_order_and_resolve_freshly() {
        let fake = Arc::new(FakeAudio::new(
            vec![
                FakeAudio::device("a", Input, true, 0.5),
                FakeAudio::device("b", Input, false, 0.5),
            ],
            vec![],
        ));
        let audio = SystemAudio::new(fake.clone(), SharedLive::default());
        audio
            .apply(mic_target(), SetAudioMode::SetVolume, 30.0)
            .await
            .unwrap();
        audio
            .apply(mic_target(), SetAudioMode::AdjustVolume, -10.0)
            .await
            .unwrap();
        // The default input changes: the next write goes to the new one.
        fake.change(|d, _| d.iter_mut().for_each(|d| d.is_default = d.id == "b"));
        audio
            .apply(mic_target(), SetAudioMode::ToggleMute, 0.0)
            .await
            .unwrap();
        let w = fake.writes();
        assert_eq!(w[0], FakeWrite::Volume("a".into(), 0.3));
        assert!(matches!(&w[1], FakeWrite::Volume(id, v) if id == "a" && (*v - 0.2).abs() < 1e-6));
        assert_eq!(w[2], FakeWrite::Muted("b".into(), true));
        // A target that is not there is an error, not a write.
        let gone = AudioTarget::App {
            app: AppRef {
                id: "x".into(),
                name: String::new(),
            },
        };
        assert!(audio.apply(gone, SetAudioMode::Mute, 0.0).await.is_err());
        assert_eq!(fake.writes().len(), 3);
    }
}
