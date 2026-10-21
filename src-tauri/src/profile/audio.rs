//! What a fader, a state link or an audio action points at in the system's
//! audio: a device (the default input or output, or one by its id) or an
//! application by its stable id. Only the reference is saved; volume and mute
//! always come from the system.

use serde::{Deserialize, Serialize};

/// Which way a device carries sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioDeviceKind {
    /// Microphones
    Input,
    /// Speakers and headphones
    Output,
}

/// A device: whichever one the system uses by default, or a specific one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DeviceRef {
    /// The system's default microphone, whichever that is right now
    DefaultInput,
    /// The system's default speakers or headphones
    DefaultOutput,
    /// One device by the id the system gives it; `label` is only shown, or matched when the id is empty
    Named {
        id: String,
        kind: AudioDeviceKind,
        #[serde(default)]
        label: String,
    },
}

impl DeviceRef {
    pub fn kind(&self) -> AudioDeviceKind {
        match self {
            DeviceRef::DefaultInput => AudioDeviceKind::Input,
            DeviceRef::DefaultOutput => AudioDeviceKind::Output,
            DeviceRef::Named { kind, .. } => *kind,
        }
    }
}

/// An application by its stable id (macOS bundle id or `proc:<exe>`, Windows lower-case exe).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRef {
    pub id: String,
    /// Shown while the app is not running
    #[serde(default)]
    pub name: String,
}

/// A device, application or Wave Link control whose volume and mute Lunchpad follows or sets.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AudioTarget {
    Device {
        device: DeviceRef,
    },
    App {
        app: AppRef,
    },
    WaveLink {
        target: crate::wavelink::WaveLinkTarget,
    },
    FineTune {
        target: crate::finetune::FineTuneTarget,
    },
    Group {
        #[serde(rename = "groupId")]
        group_id: String,
    },
}

/// Group members cannot refer to another group.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AudioMemberTarget {
    Device { device: DeviceRef },
    App { app: AppRef },
    WaveLink { target: crate::wavelink::WaveLinkTarget },
    FineTune { target: crate::finetune::FineTuneTarget },
}

impl AudioMemberTarget {
    pub fn target(&self) -> AudioTarget {
        match self {
            Self::Device { device } => AudioTarget::Device { device: device.clone() },
            Self::App { app } => AudioTarget::App { app: app.clone() },
            Self::WaveLink { target } => AudioTarget::WaveLink { target: target.clone() },
            Self::FineTune { target } => AudioTarget::FineTune { target: target.clone() },
        }
    }

    pub fn identity(&self) -> Self {
        let mut member = self.clone();
        match &mut member {
            Self::Device { device: DeviceRef::Named { label, .. } } => label.clear(),
            Self::App { app } => app.name.clear(),
            _ => {}
        }
        member
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioGroup {
    pub id: String,
    pub name: String,
    pub members: Vec<AudioMemberTarget>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn targets_json_shape() {
        let mic = AudioTarget::Device {
            device: DeviceRef::DefaultInput,
        };
        assert_eq!(
            serde_json::to_value(&mic).unwrap(),
            json!({ "type": "device", "device": { "type": "defaultInput" } })
        );
        let named = AudioTarget::Device {
            device: DeviceRef::Named {
                id: "BuiltInMic".into(),
                kind: AudioDeviceKind::Input,
                label: "MacBook Mic".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&named).unwrap(),
            json!({ "type": "device", "device": { "type": "named", "id": "BuiltInMic", "kind": "input", "label": "MacBook Mic" } })
        );
        let app = AudioTarget::App {
            app: AppRef {
                id: "com.spotify.client".into(),
                name: "Spotify".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&app).unwrap(),
            json!({ "type": "app", "app": { "id": "com.spotify.client", "name": "Spotify" } })
        );
        for t in [mic, named, app] {
            let back: AudioTarget =
                serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
            assert_eq!(back, t);
        }
        // The label and the app name are optional.
        let bare: AudioTarget = serde_json::from_value(
            json!({ "type": "device", "device": { "type": "named", "id": "x", "kind": "output" } }),
        )
        .unwrap();
        assert_eq!(
            bare,
            AudioTarget::Device {
                device: DeviceRef::Named {
                    id: "x".into(),
                    kind: AudioDeviceKind::Output,
                    label: String::new()
                }
            }
        );
        assert_eq!(DeviceRef::DefaultOutput.kind(), AudioDeviceKind::Output);
    }
}
