//! Live state of the streaming integrations that button colours can follow.
//! A button with a [`StateLink`] shows its active colour while the linked
//! condition holds (its scene is live, its source is visible, the stream is
//! on, the microphone is muted, …). The integrations write here; the LED
//! renderer and the window read.

use crate::profile::{LinkKind, LinkProvider, StateLink};
use crate::system_audio::SystemAudioState;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderStatus {
    pub connected: bool,
    pub current_scene: Option<String>,
    /// `(scene, source)` pairs whose item is visible
    pub visible: HashSet<(String, String)>,
    pub muted: HashSet<String>,
    pub streaming: bool,
    pub recording: bool,
    pub replay_buffer: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveStatus {
    pub obs: ProviderStatus,
    pub slobs: ProviderStatus,
    /// Devices and apps as the system audio bridge last saw them.
    pub system: SystemAudioState,
    pub wave_link: crate::wavelink::WaveLinkState,
    pub fine_tune: crate::finetune::FineTuneState,
    pub audio_groups: Vec<crate::system_audio::AudioGroupReport>,
}

pub type SharedLive = Arc<Mutex<LiveStatus>>;

impl LiveStatus {
    pub fn resolve_audio(
        &self,
        target: &crate::profile::AudioTarget,
    ) -> Option<crate::system_audio::TargetState> {
        match target {
            crate::profile::AudioTarget::FineTune { target } => {
                crate::system_audio::fine_tune_state(&self.fine_tune, target)
            }
            crate::profile::AudioTarget::WaveLink { target } => {
                crate::system_audio::wave_link_state(&self.wave_link, target)
            }
            _ => self.system.resolve(target),
        }
    }
}

/// Writer handle for the integrations; `on_change` runs after a real change
/// (the app repaints the LEDs with it).
#[derive(Clone)]
pub struct LiveHandle {
    status: SharedLive,
    on_change: Arc<dyn Fn() + Send + Sync>,
}

impl LiveHandle {
    pub fn new(status: SharedLive, on_change: Arc<dyn Fn() + Send + Sync>) -> Self {
        LiveHandle { status, on_change }
    }

    pub fn update(&self, provider: LinkProvider, f: impl FnOnce(&mut ProviderStatus)) {
        let changed = {
            let mut st = self.status.lock();
            let slot = match provider {
                LinkProvider::Obs => &mut st.obs,
                LinkProvider::Slobs => &mut st.slobs,
                // The system audio has its own shape; see `update_system`.
                LinkProvider::System | LinkProvider::WaveLink | LinkProvider::FineTune => return,
            };
            let before = slot.clone();
            f(slot);
            *slot != before
        };
        if changed {
            (self.on_change)();
        }
    }

    pub fn update_system(&self, f: impl FnOnce(&mut SystemAudioState)) {
        let changed = {
            let mut st = self.status.lock();
            let before = st.system.clone();
            f(&mut st.system);
            st.system != before
        };
        if changed {
            (self.on_change)();
        }
    }

    pub fn update_wave_link(&self, next: crate::wavelink::WaveLinkState) {
        let changed = {
            let mut live = self.status.lock();
            if live.wave_link == next {
                false
            } else {
                live.wave_link = next;
                true
            }
        };
        if changed {
            (self.on_change)();
        }
    }

    pub fn update_fine_tune(&self, next: crate::finetune::FineTuneState) {
        let changed = {
            let mut live = self.status.lock();
            if live.fine_tune == next {
                false
            } else {
                live.fine_tune = next;
                true
            }
        };
        if changed {
            (self.on_change)();
        }
    }
}

impl StateLink {
    pub fn is_active(&self, live: &LiveStatus) -> bool {
        let p = match self.provider {
            LinkProvider::FineTune => {
                use crate::finetune::{FineTuneStatus, FineTuneTarget};
                let state = &live.fine_tune;
                if state.status != FineTuneStatus::Ready {
                    return false;
                }
                let Some(crate::profile::AudioTarget::FineTune { target }) = &self.target else {
                    return false;
                };
                if state.target_reason(target).is_some() {
                    return false;
                }
                let Some(snapshot) = state.snapshot.as_ref() else {
                    return false;
                };
                return match (&self.kind, target) {
                    (LinkKind::AudioMuted, FineTuneTarget::App { app_id }) => {
                        state.app(app_id).is_ok_and(|app| app.muted)
                    }
                    (LinkKind::FineTuneDefaultOutput, FineTuneTarget::Output { device_uid }) => {
                        snapshot.default_output.as_ref() == Some(device_uid)
                    }
                    (LinkKind::FineTunePreferredInput, FineTuneTarget::Input { device_uid }) => {
                        snapshot.input_policy.preferred_device_uid.as_ref() == Some(device_uid)
                    }
                    (LinkKind::FineTuneDefaultInput, FineTuneTarget::Input { device_uid }) => {
                        snapshot.default_input.as_ref() == Some(device_uid)
                    }
                    _ => false,
                };
            }
            LinkProvider::WaveLink => {
                let state = &live.wave_link;
                if state.status != crate::wavelink::WaveLinkStatus::Ready {
                    return false;
                }
                let Some(crate::profile::AudioTarget::WaveLink { target }) = &self.target else {
                    return false;
                };
                return match self.kind {
                    LinkKind::AudioMuted => state
                        .control(target)
                        .is_some_and(|control| control.can_mute && control.muted == Some(true)),
                    LinkKind::WaveLinkMainOutput => match target {
                        crate::wavelink::WaveLinkTarget::Output {
                            output_device_id,
                            output_id,
                        } => state.main_output.as_ref().is_some_and(|output| {
                            &output.output_device_id == output_device_id
                                && &output.output_id == output_id
                        }),
                        _ => false,
                    },
                    LinkKind::WaveLinkMonitoredMix => match target {
                        crate::wavelink::WaveLinkTarget::Output {
                            output_device_id,
                            output_id,
                        } => self
                            .wave_link_mix_id
                            .as_ref()
                            .filter(|id| !id.is_empty())
                            .is_some_and(|mix| {
                                state.outputs.iter().any(|output| {
                                    &output.output_device_id == output_device_id
                                        && &output.output_id == output_id
                                        && output.mix_id.as_ref() == Some(mix)
                                })
                            }),
                        _ => false,
                    },
                    _ => false,
                };
            }
            LinkProvider::Obs => &live.obs,
            LinkProvider::Slobs => &live.slobs,
            // Muted and there; a missing device or app (or no audio at all) is not lit.
            LinkProvider::System => {
                if let Some(crate::profile::AudioTarget::Group { group_id }) = &self.target {
                    return self.kind == LinkKind::AudioMuted && live.audio_groups.iter().any(|g| &g.group_id == group_id && g.summary.mute == crate::system_audio::AudioGroupMute::All);
                }
                return self.kind == LinkKind::AudioMuted
                    && self
                        .target
                        .as_ref()
                        .and_then(|t| live.system.resolve(t))
                        .is_some_and(|s| s.can_mute && s.muted);
            }
        };
        if !p.connected {
            return false;
        }
        match self.kind {
            LinkKind::Scene => p.current_scene.as_deref() == Some(self.scene.as_str()),
            LinkKind::SourceVisible => {
                let scene = if self.scene.is_empty() {
                    p.current_scene.clone()
                } else {
                    Some(self.scene.clone())
                };
                scene
                    .map(|s| p.visible.contains(&(s, self.source.clone())))
                    .unwrap_or(false)
            }
            LinkKind::SourceMuted => p.muted.contains(&self.source),
            LinkKind::Streaming => p.streaming,
            LinkKind::Recording => p.recording,
            LinkKind::ReplayBuffer => p.replay_buffer,
            LinkKind::AudioMuted
            | LinkKind::WaveLinkMainOutput
            | LinkKind::WaveLinkMonitoredMix => false,
            LinkKind::FineTuneDefaultOutput
            | LinkKind::FineTunePreferredInput
            | LinkKind::FineTuneDefaultInput => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn fine_tune_links_follow_only_ready_snapshot_visible_targets() {
        use crate::finetune::*;
        use crate::profile::AudioTarget;
        let mut snapshot: FineTuneSnapshot =
            serde_json::from_value(crate::finetune::tests::snapshot()).unwrap();
        snapshot.apps[0].muted = true;
        let mut live = LiveStatus {
            fine_tune: FineTuneState {
                status: FineTuneStatus::Ready,
                snapshot: Some(snapshot),
                ..Default::default()
            },
            ..Default::default()
        };
        for (kind, target) in [
            (
                LinkKind::AudioMuted,
                FineTuneTarget::App {
                    app_id: "app".into(),
                },
            ),
            (
                LinkKind::FineTuneDefaultOutput,
                FineTuneTarget::Output {
                    device_uid: "out".into(),
                },
            ),
            (
                LinkKind::FineTunePreferredInput,
                FineTuneTarget::Input {
                    device_uid: "in".into(),
                },
            ),
            (
                LinkKind::FineTuneDefaultInput,
                FineTuneTarget::Input {
                    device_uid: "in".into(),
                },
            ),
        ] {
            let link = StateLink {
                provider: LinkProvider::FineTune,
                kind,
                target: Some(AudioTarget::FineTune { target }),
                scene: String::new(),
                source: String::new(),
                wave_link_mix_id: None,
            };
            assert!(link.is_active(&live));
            live.fine_tune.status = FineTuneStatus::Disconnected;
            assert!(!link.is_active(&live));
            live.fine_tune.status = FineTuneStatus::Ready;
        }
        let target = AudioTarget::FineTune {
            target: FineTuneTarget::App {
                app_id: "app".into(),
            },
        };
        assert!(live.resolve_audio(&target).unwrap().muted);
        let snapshot = live.fine_tune.snapshot.as_mut().unwrap();
        snapshot.apps.push(snapshot.apps[0].clone());
        assert!(
            live.resolve_audio(&target).is_none(),
            "ambiguous apps cannot drive a bound fader"
        );
        let snapshot = live.fine_tune.snapshot.as_mut().unwrap();
        snapshot.devices.clear();
        let link = StateLink {
            provider: LinkProvider::FineTune,
            kind: LinkKind::FineTuneDefaultOutput,
            target: Some(AudioTarget::FineTune {
                target: FineTuneTarget::Output {
                    device_uid: "out".into(),
                },
            }),
            scene: String::new(),
            source: String::new(),
            wave_link_mix_id: None,
        };
        assert!(
            !link.is_active(&live),
            "a UID must still be visible to FineTune"
        );
    }

    #[test]
    fn wave_link_links_and_faders_require_ready_qualified_state() {
        use crate::wavelink::*;
        let target = WaveLinkTarget::Output {
            output_device_id: "device-a".into(),
            output_id: "same".into(),
        };
        let audio = crate::profile::AudioTarget::WaveLink {
            target: target.clone(),
        };
        let link = StateLink {
            provider: LinkProvider::WaveLink,
            kind: LinkKind::AudioMuted,
            target: Some(audio.clone()),
            scene: String::new(),
            source: String::new(),
            wave_link_mix_id: Some("mix".into()),
        };
        let mut live = LiveStatus::default();
        live.wave_link.status = WaveLinkStatus::Ready;
        live.wave_link.controls.push(WaveLinkControl {
            target,
            name: "Output".into(),
            level: Some(0.7),
            muted: Some(true),
            can_set_level: true,
            can_mute: true,
            reason: None,
            gain: None,
        });
        live.wave_link.outputs.push(WaveLinkOutput {
            output_device_id: "device-a".into(),
            output_id: "same".into(),
            name: "Output".into(),
            mix_id: Some("mix".into()),
            can_select_main: true,
            can_set_mix: true,
        });
        live.wave_link.main_output = Some(WaveLinkMainOutput {
            output_device_id: "device-a".into(),
            output_id: "same".into(),
        });
        assert!(link.is_active(&live));
        assert!(StateLink {
            kind: LinkKind::WaveLinkMainOutput,
            ..link.clone()
        }
        .is_active(&live));
        assert!(StateLink {
            kind: LinkKind::WaveLinkMonitoredMix,
            ..link.clone()
        }
        .is_active(&live));
        assert!(!StateLink {
            kind: LinkKind::WaveLinkMonitoredMix,
            wave_link_mix_id: Some(String::new()),
            ..link.clone()
        }
        .is_active(&live));
        let mut fader = crate::profile::Fader {
            bound: Some(audio),
            min: 0.0,
            max: 100.0,
            value: 70.0,
            ..Default::default()
        };
        assert_eq!(fader.render_value(&live), Some(0.0));
        live.wave_link.controls[0].muted = Some(false);
        assert_eq!(fader.render_value(&live), Some(70.0));
        fader.bound = Some(crate::profile::AudioTarget::WaveLink {
            target: WaveLinkTarget::Output {
                output_device_id: "device-b".into(),
                output_id: "same".into(),
            },
        });
        assert_eq!(fader.render_value(&live), None);
        live.wave_link.status = WaveLinkStatus::Disconnected;
        assert!(!link.is_active(&live));
        assert_eq!(live.resolve_audio(link.target.as_ref().unwrap()), None);
    }

    fn link(provider: LinkProvider, kind: LinkKind, scene: &str, source: &str) -> StateLink {
        StateLink {
            provider,
            kind,
            scene: scene.into(),
            source: source.into(),
            target: None,
            wave_link_mix_id: None,
        }
    }

    #[test]
    fn links_follow_the_status() {
        let mut live = LiveStatus::default();
        live.obs.connected = true;
        live.obs.current_scene = Some("Game".into());
        live.obs.visible.insert(("Game".into(), "Cam".into()));
        live.obs.muted.insert("Mic".into());
        live.obs.streaming = true;

        assert!(link(LinkProvider::Obs, LinkKind::Scene, "Game", "").is_active(&live));
        assert!(!link(LinkProvider::Obs, LinkKind::Scene, "Chat", "").is_active(&live));
        assert!(link(LinkProvider::Obs, LinkKind::SourceVisible, "Game", "Cam").is_active(&live));
        // Empty scene = the current scene.
        assert!(link(LinkProvider::Obs, LinkKind::SourceVisible, "", "Cam").is_active(&live));
        assert!(!link(LinkProvider::Obs, LinkKind::SourceVisible, "Chat", "Cam").is_active(&live));
        assert!(link(LinkProvider::Obs, LinkKind::SourceMuted, "", "Mic").is_active(&live));
        assert!(link(LinkProvider::Obs, LinkKind::Streaming, "", "").is_active(&live));
        assert!(!link(LinkProvider::Obs, LinkKind::Recording, "", "").is_active(&live));
        // Streamlabs is not connected: nothing lights up.
        assert!(!link(LinkProvider::Slobs, LinkKind::Streaming, "", "").is_active(&live));
    }

    #[test]
    fn a_system_link_lights_while_its_target_is_muted() {
        use crate::profile::{AudioDeviceKind, AudioTarget, DeviceRef};
        use crate::system_audio::FakeAudio;
        let mic = StateLink {
            provider: LinkProvider::System,
            kind: LinkKind::AudioMuted,
            scene: String::new(),
            source: String::new(),
            target: Some(AudioTarget::Device {
                device: DeviceRef::DefaultInput,
            }),
            wave_link_mix_id: None,
        };
        let mut live = LiveStatus::default();
        // No system audio at all: not lit.
        assert!(!mic.is_active(&live));
        let mut device = FakeAudio::device("m", AudioDeviceKind::Input, true, 0.5);
        device.muted = true;
        live.system = SystemAudioState {
            available: true,
            devices: vec![device],
            ..Default::default()
        };
        assert!(mic.is_active(&live));
        // The muted device is not the default any more: the default input is absent, not lit.
        live.system.devices[0].is_default = false;
        assert!(!mic.is_active(&live));
        live.system.devices[0].is_default = true;
        live.system.available = false;
        assert!(!mic.is_active(&live), "unavailable is not lit");
        live.system.available = true;
        // A link without a target, or with an OBS kind, never lights.
        assert!(!StateLink {
            target: None,
            ..mic.clone()
        }
        .is_active(&live));
        assert!(!StateLink {
            kind: LinkKind::Streaming,
            ..mic.clone()
        }
        .is_active(&live));
        // An OBS link with the System kind does not either.
        live.obs.connected = true;
        assert!(!StateLink {
            provider: LinkProvider::Obs,
            ..mic.clone()
        }
        .is_active(&live));
        // A change repaints, the same snapshot again does not.
        let repaints = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = repaints.clone();
        let handle = LiveHandle::new(
            Arc::new(Mutex::new(LiveStatus::default())),
            Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        );
        let snapshot = live.system.clone();
        handle.update_system(|s| *s = snapshot.clone());
        handle.update_system(|s| *s = snapshot.clone());
        assert_eq!(repaints.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn disconnected_provider_is_inactive() {
        let mut live = LiveStatus::default();
        live.obs.streaming = true;
        assert!(!link(LinkProvider::Obs, LinkKind::Streaming, "", "").is_active(&live));
        let handle = LiveHandle::new(Arc::new(Mutex::new(live)), Arc::new(|| {}));
        handle.update(LinkProvider::Obs, |p| p.connected = true);
        assert!(
            link(LinkProvider::Obs, LinkKind::Streaming, "", "").is_active(&handle.status.lock())
        );
    }
}
