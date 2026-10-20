use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WaveLinkTarget {
    Channel {
        channel_id: String,
    },
    ChannelMix {
        channel_id: String,
        mix_id: String,
    },
    Mix {
        mix_id: String,
    },
    Output {
        output_device_id: String,
        output_id: String,
    },
    Input {
        input_device_id: String,
        input_id: String,
    },
    InputGain {
        input_device_id: String,
        input_id: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WaveLinkStatus {
    #[default]
    Disabled,
    Disconnected,
    Connecting,
    Synchronizing,
    Ready,
    Unsupported,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveLinkApplication {
    pub name: String,
    pub operating_system: String,
    #[serde(rename = "appID")]
    pub app_id: String,
    pub interface_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaveLinkGain {
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveLinkControl {
    pub target: WaveLinkTarget,
    pub name: String,
    pub level: Option<f64>,
    pub muted: Option<bool>,
    pub can_set_level: bool,
    pub can_mute: bool,
    pub reason: Option<String>,
    pub gain: Option<WaveLinkGain>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaveLinkApp {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaveLinkChannel {
    pub id: String,
    pub name: String,
    pub apps: Vec<WaveLinkApp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaveLinkMix {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveLinkOutput {
    pub output_device_id: String,
    pub output_id: String,
    pub name: String,
    pub mix_id: Option<String>,
    pub can_select_main: bool,
    pub can_set_mix: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveLinkMainOutput {
    pub output_device_id: String,
    pub output_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveLinkState {
    pub status: WaveLinkStatus,
    pub connected: bool,
    pub error: Option<String>,
    pub port: Option<u16>,
    pub application: Option<WaveLinkApplication>,
    pub version: Option<String>,
    pub generation: u64,
    pub revision: u64,
    pub controls: Vec<WaveLinkControl>,
    pub channels: Vec<WaveLinkChannel>,
    pub mixes: Vec<WaveLinkMix>,
    pub outputs: Vec<WaveLinkOutput>,
    pub main_output: Option<WaveLinkMainOutput>,
}

impl WaveLinkState {
    pub fn control(&self, target: &WaveLinkTarget) -> Option<&WaveLinkControl> {
        (self.status == WaveLinkStatus::Ready).then_some(())?;
        self.controls
            .iter()
            .find(|control| &control.target == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Settings, WaveLinkSettings};
    use serde_json::json;

    #[test]
    fn settings_are_opt_in_and_ports_are_checked() {
        let settings: Settings = serde_json::from_value(json!({})).unwrap();
        assert_eq!(settings.wave_link, WaveLinkSettings::default());
        assert!(!settings.wave_link.enabled);
        assert!(settings.wave_link.auto_connect);
        assert!(WaveLinkSettings {
            port: Some(0),
            ..Default::default()
        }
        .validate()
        .is_err());
        for value in [json!(-1), json!(65536), json!("1884")] {
            assert!(serde_json::from_value::<WaveLinkSettings>(json!({"port":value})).is_err());
        }
    }

    #[test]
    fn identities_are_qualified_and_json_is_frozen() {
        let targets = [
            json!({"type":"channel","channelId":"opaque"}),
            json!({"type":"channelMix","channelId":"opaque","mixId":"m"}),
            json!({"type":"mix","mixId":"m"}),
            json!({"type":"input","inputDeviceId":"a","inputId":"same"}),
            json!({"type":"input","inputDeviceId":"b","inputId":"same"}),
            json!({"type":"output","outputDeviceId":"a","outputId":"same"}),
            json!({"type":"inputGain","inputDeviceId":"a","inputId":"same"}),
        ];
        let mut identities = std::collections::HashSet::new();
        for value in targets {
            let target: WaveLinkTarget = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(&target).unwrap(), value);
            assert!(identities.insert(target));
        }
        let app: WaveLinkApplication = serde_json::from_value(json!({
            "name":"Wave Link","operatingSystem":"mac","appID":"EWL","interfaceRevision":2
        }))
        .unwrap();
        assert_eq!(serde_json::to_value(app).unwrap()["appID"], "EWL");
        let state = serde_json::to_value(WaveLinkState::default()).unwrap();
        assert_eq!(state["status"], "disabled");
        assert_eq!(state["version"], serde_json::Value::Null);
    }
}
