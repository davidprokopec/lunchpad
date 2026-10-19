//! One adapter over the existing native service; no worker or emulation store
//! is started here. Setters are called by the engine's FIFO writer thread.

use crate::profile::AudioDeviceKind;
use crate::sysaudio::{self, DeviceKind, DeviceRef, Permission};
use crate::system_audio::{
    AudioApp, AudioControl, AudioDevice, AudioPermission, AudioStatus, AudioSubscription,
};

pub struct NativeAudio {
    service: sysaudio::SystemAudio,
}

impl NativeAudio {
    pub fn new(service: sysaudio::SystemAudio) -> Self {
        Self { service }
    }
}

impl From<AudioDeviceKind> for DeviceKind {
    fn from(kind: AudioDeviceKind) -> Self {
        match kind {
            AudioDeviceKind::Input => Self::Input,
            AudioDeviceKind::Output => Self::Output,
        }
    }
}

impl From<DeviceKind> for AudioDeviceKind {
    fn from(kind: DeviceKind) -> Self {
        match kind {
            DeviceKind::Input => Self::Input,
            DeviceKind::Output => Self::Output,
        }
    }
}

impl AudioControl for NativeAudio {
    fn legacy_volume_fallback(&self) -> bool {
        cfg!(not(any(target_os = "macos", windows)))
    }
    fn configure(&self, macos_app_volume: bool) -> Result<(), String> {
        if self.legacy_volume_fallback() && !macos_app_volume {
            return Ok(());
        }
        self.service
            .configure(sysaudio::Options {
                macos_taps: macos_app_volume,
            })
            .map_err(|e| e.to_string())
    }

    fn status(&self) -> AudioStatus {
        let status = self.service.status();
        AudioStatus {
            available: status.available,
            apps_available: status.apps_available,
            can_set_app_volume: status.can_set_app_volume,
            can_mute_apps: status.can_mute_apps,
            permission: match status.permission {
                Permission::Unknown => AudioPermission::Unknown,
                Permission::Granted => AudioPermission::Granted,
                Permission::Denied => AudioPermission::Denied,
                Permission::Unconfirmed => AudioPermission::Unconfirmed,
            },
            message: status.message,
        }
    }

    fn devices(&self) -> Result<Vec<AudioDevice>, String> {
        self.service
            .devices()
            .map(|devices| {
                devices
                    .into_iter()
                    .map(|d| AudioDevice {
                        id: d.id,
                        name: d.name,
                        kind: d.kind.into(),
                        volume: d.volume,
                        muted: d.muted,
                        is_default: d.is_default,
                        can_set_volume: d.can_set_volume,
                        can_mute: d.can_mute,
                        emulated: d.emulated,
                        present: true,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }

    fn apps(&self) -> Result<Vec<AudioApp>, String> {
        self.service
            .apps()
            .map(|apps| {
                apps.into_iter()
                    .map(|a| AudioApp {
                        id: a.id,
                        name: a.name,
                        volume: a.volume,
                        muted: a.muted,
                        active: a.active,
                        present: true,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }

    fn set_device_volume(
        &self,
        id: &str,
        kind: AudioDeviceKind,
        volume: f32,
    ) -> Result<(), String> {
        self.service
            .set_volume(
                &DeviceRef::Named {
                    id: id.into(),
                    kind: kind.into(),
                },
                volume,
            )
            .map_err(|e| e.to_string())
    }

    fn set_device_muted(&self, id: &str, kind: AudioDeviceKind, muted: bool) -> Result<(), String> {
        self.service
            .set_muted(
                &DeviceRef::Named {
                    id: id.into(),
                    kind: kind.into(),
                },
                muted,
            )
            .map_err(|e| e.to_string())
    }

    fn set_app_volume(&self, id: &str, volume: f32) -> Result<(), String> {
        self.service
            .set_app_volume(id, volume)
            .map_err(|e| e.to_string())
    }

    fn set_app_muted(&self, id: &str, muted: bool) -> Result<(), String> {
        self.service
            .set_app_muted(id, muted)
            .map_err(|e| e.to_string())
    }

    fn subscribe(&self) -> AudioSubscription {
        AudioSubscription::Native(self.service.subscribe())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sysaudio::{App, Backend, BackendKind, Device, Error, Options, TestAppStatus};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::time::Duration;

    pub(crate) struct MockState {
        pub devices: Vec<Device>,
        pub apps: Vec<App>,
        pub volume: bool,
        pub mute: bool,
        pub permission: Permission,
        pub configurations: Vec<bool>,
        pub writes: usize,
    }

    struct BackendMock(Arc<Mutex<MockState>>);

    impl Backend for BackendMock {
        fn configure(&mut self, options: Options) -> Result<(), Error> {
            let mut st = self.0.lock();
            st.configurations.push(options.macos_taps);
            st.volume = options.macos_taps;
            st.mute = options.macos_taps;
            if !options.macos_taps {
                for app in &mut st.apps {
                    app.volume = 1.0;
                    app.muted = false;
                }
            }
            Ok(())
        }
        fn status(&self) -> TestAppStatus {
            let st = self.0.lock();
            TestAppStatus {
                backend: BackendKind::None,
                available: true,
                can_set_volume: st.volume,
                can_mute: st.mute,
                permission: st.permission,
                message: "mock permission".into(),
            }
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_secs(3600)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            Ok(self.0.lock().apps.clone())
        }
        fn devices(&mut self) -> Result<Vec<Device>, Error> {
            Ok(self.0.lock().devices.clone())
        }
        fn set_volume(&mut self, _: &str, volume: f32) -> Result<(), Error> {
            let mut st = self.0.lock();
            st.apps[0].volume = volume;
            st.writes += 1;
            Ok(())
        }
        fn set_mute(&mut self, _: &str, muted: bool) -> Result<(), Error> {
            let mut st = self.0.lock();
            st.apps[0].muted = muted;
            st.writes += 1;
            Ok(())
        }
        fn set_device_volume(
            &mut self,
            id: &str,
            kind: DeviceKind,
            volume: f32,
        ) -> Result<(), Error> {
            let mut st = self.0.lock();
            let d = st
                .devices
                .iter_mut()
                .find(|d| d.id == id && d.kind == kind)
                .ok_or_else(|| Error::new(sysaudio::ErrorKind::NotFound, "mock device"))?;
            // Real devices can quantize requested values; return only actual readback.
            d.volume = (volume * 10.0).round() / 10.0;
            st.writes += 1;
            Ok(())
        }
        fn set_device_mute(
            &mut self,
            id: &str,
            kind: DeviceKind,
            muted: bool,
        ) -> Result<(), Error> {
            let mut st = self.0.lock();
            let d = st
                .devices
                .iter_mut()
                .find(|d| d.id == id && d.kind == kind)
                .ok_or_else(|| Error::new(sysaudio::ErrorKind::NotFound, "mock device"))?;
            d.muted = muted;
            st.writes += 1;
            Ok(())
        }
    }

    pub(crate) fn fixture(
        config_dir: Option<&std::path::Path>,
    ) -> (sysaudio::SystemAudio, Arc<Mutex<MockState>>) {
        fixture_capabilities(config_dir, false, false, Permission::Unknown)
    }

    fn fixture_capabilities(
        config_dir: Option<&std::path::Path>,
        volume: bool,
        mute: bool,
        permission: Permission,
    ) -> (sysaudio::SystemAudio, Arc<Mutex<MockState>>) {
        let device = |kind| Device {
            id: "duplex".into(),
            name: "Duplex".into(),
            kind,
            volume: 0.6,
            muted: false,
            is_default: true,
            can_set_volume: true,
            can_mute: kind == DeviceKind::Output,
            emulated: false,
        };
        let state = Arc::new(Mutex::new(MockState {
            devices: vec![device(DeviceKind::Input), device(DeviceKind::Output)],
            apps: vec![App {
                id: "music".into(),
                name: "Music".into(),
                volume: 1.0,
                muted: false,
                active: true,
                process_count: 1,
            }],
            volume,
            mute,
            permission,
            configurations: vec![],
            writes: 0,
        }));
        let backend = state.clone();
        (
            sysaudio::SystemAudio::with_test_backend(
                move || Box::new(BackendMock(backend)),
                config_dir,
            ),
            state,
        )
    }

    #[test]
    fn adapter_preserves_duplex_identity_emulated_restore_and_read_after_write() {
        let dir = std::env::temp_dir().join(format!("lunchpad-adapter-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (service, _) = fixture(Some(&dir));
        let adapter = NativeAudio::new(service.clone());
        let input = AudioDeviceKind::Input;
        let output = AudioDeviceKind::Output;
        adapter.set_device_volume("duplex", output, 0.27).unwrap();
        let device = |kind| {
            adapter
                .devices()
                .unwrap()
                .into_iter()
                .find(|d| d.kind == kind)
                .unwrap()
        };
        assert_eq!(device(output).volume, 0.3);
        assert_eq!(device(input).volume, 0.6);
        adapter.set_device_muted("duplex", input, true).unwrap();
        let muted = device(input);
        assert!(muted.emulated && muted.can_mute && muted.present && muted.muted);
        assert_eq!(muted.volume, 0.6, "restore level, never raw parked zero");
        assert!(!device(output).muted);
        assert!(dir.join("sysaudio-mutes.json").exists());
        adapter.set_device_muted("duplex", input, false).unwrap();
        assert_eq!(device(input).volume, 0.6);
        assert!(!device(input).muted);
        service.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn listing_capabilities_and_permission_are_independent_and_configure_same_service() {
        let (service, state) = fixture(None);
        let adapter = NativeAudio::new(service.clone());
        let status = adapter.status();
        assert!(status.available && status.apps_available);
        assert!(!status.can_set_app_volume && !status.can_mute_apps);
        assert_eq!(status.permission, AudioPermission::Unknown);
        assert_eq!(adapter.apps().unwrap().len(), 1);
        assert!(adapter.set_app_volume("music", 0.5).is_err());
        adapter.configure(true).unwrap();
        assert_eq!(state.lock().configurations, vec![true]);
        assert_eq!(
            adapter.status().permission,
            AudioPermission::Unknown,
            "opt-in permits a probe, not a claim of permission"
        );
        adapter.set_app_volume("music", 0.5).unwrap();
        assert_eq!(adapter.apps().unwrap()[0].volume, 0.5);
        adapter.set_app_muted("music", true).unwrap();
        assert!(adapter.apps().unwrap()[0].muted);
        state.lock().permission = Permission::Unconfirmed;
        adapter.configure(true).unwrap();
        assert_eq!(adapter.status().permission, AudioPermission::Unconfirmed);
        adapter.set_app_muted("music", false).unwrap();
        adapter.configure(false).unwrap();
        assert_eq!(adapter.apps().unwrap()[0].volume, 1.0);
        assert!(!adapter.status().can_set_app_volume);
        service.shutdown();
    }

    #[test]
    fn subscriptions_do_not_retain_service_and_shutdown_invalidates_adapter() {
        let (service, _) = fixture(None);
        let adapter = NativeAudio::new(service.clone());
        let subscription = adapter.subscribe();
        service.shutdown();
        subscription.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            subscription.recv_timeout(Duration::from_secs(1)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(!adapter.status().available && !adapter.status().apps_available);
        assert!(!adapter.status().can_set_app_volume && !adapter.status().can_mute_apps);
        #[cfg(any(target_os = "macos", windows))]
        assert!(
            !adapter.legacy_volume_fallback(),
            "a stopped native service must not fall through to OS one-shot writes"
        );
        assert!(adapter.devices().is_err() && adapter.apps().is_err());
        assert!(adapter.set_app_muted("music", true).is_err());
        drop(subscription);
        let (service, _) = fixture(None);
        let adapter = NativeAudio::new(service.clone());
        let subscription = adapter.subscribe();
        drop(adapter);
        drop(service);
        subscription.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            subscription.recv_timeout(Duration::from_secs(1)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn app_listing_is_not_volume_or_mute_admission_but_opted_in_probes_work() {
        use crate::macros::SetAudioMode;
        use crate::profile::{AppRef, AudioTarget};
        use crate::system_audio::{read_state, SystemAudio};
        let target = AudioTarget::App {
            app: AppRef {
                id: "music".into(),
                name: "Music".into(),
            },
        };
        for (volume, mute, permission) in [
            (false, false, Permission::Unknown),
            (true, false, Permission::Unconfirmed),
            (false, true, Permission::Granted),
            (true, true, Permission::Unknown),
        ] {
            let (service, st) = fixture_capabilities(None, volume, mute, permission);
            let port = Arc::new(NativeAudio::new(service.clone()));
            let state = read_state(port.as_ref());
            let resolved = state.resolve(&target).unwrap();
            assert_eq!(resolved.can_set_volume, volume);
            assert_eq!(resolved.can_mute, mute);
            assert_eq!(state.apps.len(), 1, "picker listing is independent");
            let audio = SystemAudio::new(port.clone(), Default::default());
            assert_eq!(
                audio
                    .apply(target.clone(), SetAudioMode::SetVolume, 50.0)
                    .await
                    .is_ok(),
                volume
            );
            assert_eq!(
                audio
                    .apply(target.clone(), SetAudioMode::Mute, 0.0)
                    .await
                    .is_ok(),
                mute
            );
            assert_eq!(st.lock().writes, usize::from(volume) + usize::from(mute));
            service.shutdown();
        }
    }

    #[tokio::test]
    async fn native_bridge_drives_bound_faders_without_feedback_and_drops_its_subscription() {
        use crate::live::{LiveHandle, SharedLive};
        use crate::macros::{sink::NullSink, Action, ActionKind, MacroEngine, Services, VarScope};
        use crate::profile::{AudioTarget, Fader, ProfileStore};
        use crate::system_audio::SystemAudio;
        use crate::system_audio_bridge::Bridge;
        use std::collections::HashSet;
        let (service, state) = fixture(None);
        let port = Arc::new(NativeAudio::new(service.clone()));
        let live: SharedLive = Default::default();
        let audio = SystemAudio::new(port.clone(), live.clone());
        let dir =
            std::env::temp_dir().join(format!("lunchpad-adapter-fader-{}", uuid::Uuid::new_v4()));
        let profile = Arc::new(Mutex::new(ProfileStore::load(&dir)));
        profile.lock().profile.pages[0].set_fader(Fader {
            id: "native".into(),
            value: 100.0,
            bound: Some(AudioTarget::Device {
                device: crate::profile::DeviceRef::DefaultOutput,
            }),
            on_change: vec![Action::new(ActionKind::SetVariable {
                name: "unexpected".into(),
                value: "writeback".into(),
                scope: VarScope::Global,
            })],
            ..Default::default()
        });
        let engine = MacroEngine::new(
            profile.clone(),
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(NullSink),
            Services {
                system_audio: Some(audio.clone()),
                ..Default::default()
            },
            tokio::runtime::Handle::current(),
        );
        let bridge = Bridge {
            audio,
            live: LiveHandle::new(live.clone(), Arc::new(|| {})),
            engine: engine.clone(),
            profile: profile.clone(),
            app: None,
        };
        bridge.refresh();
        assert!((profile.lock().profile.pages[0].faders[0].value - 60.0).abs() < 1e-4);
        assert_eq!(state.lock().writes, 0);
        assert!(!engine.globals().contains_key("unexpected"));
        {
            let mut store = profile.lock();
            let fader = &mut store.profile.pages[0].faders[0];
            fader.bound = Some(AudioTarget::App {
                app: crate::profile::AppRef {
                    id: "music".into(),
                    name: "Music".into(),
                },
            });
            fader.value = 40.0;
        }
        bridge.refresh();
        assert!(profile.lock().profile.pages[0].faders[0]
            .render_value(&live.lock())
            .is_none());
        engine.on_button(&crate::midi::types::ButtonEvent {
            x: 0,
            y: 3,
            pressed: true,
            note: 0,
            cc: false,
            value: 127,
        });
        bridge.audio.run(|_| Ok(())).await.unwrap();
        assert_eq!(
            profile.lock().profile.pages[0].faders[0].value,
            40.0,
            "listing-only app cannot accept a fader commit"
        );
        assert_eq!(state.lock().writes, 0);
        assert!(!engine.globals().contains_key("unexpected"));
        port.configure(true).unwrap();
        bridge.refresh();
        assert_eq!(
            profile.lock().profile.pages[0].faders[0].value,
            100.0,
            "opted-in target adopts its authoritative app level"
        );
        assert_eq!(
            state.lock().writes,
            0,
            "Os reconciliation never writes back"
        );
        let token = Arc::new(());
        let weak = Arc::downgrade(&token);
        let watched = LiveHandle::new(
            live.clone(),
            Arc::new(move || {
                let _ = &token;
            }),
        );
        let terminal = bridge.clone();
        let guard = Bridge {
            live: watched,
            ..bridge
        }
        .spawn();
        // Drop joins the worker and releases its subscription even if the service stays alive.
        drop(guard);
        assert!(weak.upgrade().is_none());
        assert!(service.status().available);
        let terminal_guard = terminal.spawn();
        service.shutdown();
        drop(terminal_guard);
        let stopped = live.lock().system.clone();
        assert!(!stopped.available && !stopped.apps_available);
        assert!(!port.status().available);
        std::fs::remove_dir_all(dir).unwrap_or_default();
    }
}
