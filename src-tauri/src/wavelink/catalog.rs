use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Domain {
    Channels,
    Mixes,
    Inputs,
    Outputs,
}

impl Domain {
    pub const ALL: [Self; 4] = [Self::Channels, Self::Mixes, Self::Inputs, Self::Outputs];
    pub fn method(self) -> &'static str {
        match self {
            Self::Channels => "getChannels",
            Self::Mixes => "getMixes",
            Self::Inputs => "getInputDevices",
            Self::Outputs => "getOutputDevices",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Channels => "channels",
            Self::Mixes => "mixes",
            Self::Inputs => "inputDevices",
            Self::Outputs => "outputDevices",
        }
    }
    pub fn notification(method: &str) -> Option<(Self, bool)> {
        Some(match method {
            "channelChanged" => (Self::Channels, false),
            "channelsChanged" => (Self::Channels, true),
            "mixChanged" => (Self::Mixes, false),
            "mixesChanged" => (Self::Mixes, true),
            "inputDeviceChanged" => (Self::Inputs, false),
            "inputDevicesChanged" => (Self::Inputs, true),
            "outputDeviceChanged" => (Self::Outputs, false),
            "outputDevicesChanged" => (Self::Outputs, true),
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    level: Option<f64>,
    #[serde(default)]
    is_muted: Option<bool>,
    #[serde(default)]
    apps: Option<Vec<WaveLinkApp>>,
    #[serde(default)]
    mixes: Option<Vec<Raw>>,
    #[serde(default)]
    inputs: Option<Vec<Raw>>,
    #[serde(default)]
    outputs: Option<Vec<Raw>>,
    #[serde(default)]
    mix_id: Option<String>,
    #[serde(default)]
    gain: Option<RawGain>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawGain {
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    unit: Option<String>,
}

#[derive(Default)]
pub(super) struct Catalog {
    domains: std::collections::HashMap<Domain, Vec<Raw>>,
    main: Option<WaveLinkMainOutput>,
}

fn validate_ids(list: &[Raw]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for entry in list {
        if !seen.insert(&entry.id) {
            return Err("duplicate Wave Link id in one catalog/parent".into());
        }
        for nested in [&entry.mixes, &entry.inputs, &entry.outputs]
            .into_iter()
            .flatten()
        {
            validate_ids(nested)?;
        }
    }
    Ok(())
}

// Only the documented nested endpoint collections are patches. App membership
// arrays replace membership; unknown fields disappear when decoded into Raw.
fn merge(existing: &mut Value, patch: &Value) -> Result<(), String> {
    let object = patch
        .as_object()
        .ok_or("Wave Link patch must be an object")?;
    let destination = existing
        .as_object_mut()
        .ok_or("Wave Link patch destination must be an object")?;
    for (key, value) in object {
        if matches!(key.as_str(), "mixes" | "inputs" | "outputs") && value.is_array() {
            let mut list = destination
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut seen = std::collections::HashSet::new();
            for child in value.as_array().ok_or("invalid nested patch")? {
                let id = child
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("nested Wave Link patch missing string id")?;
                if !seen.insert(id) {
                    return Err("duplicate nested Wave Link patch id".into());
                }
                match list
                    .iter_mut()
                    .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
                {
                    Some(entry) => merge(entry, child)?,
                    None => list.push(child.clone()),
                }
            }
            destination.insert(key.clone(), Value::Array(list));
        } else if key == "gain" && value.is_object() {
            let entry = destination
                .entry(key.clone())
                .or_insert_with(|| serde_json::json!({}));
            if !entry.is_object() {
                *entry = serde_json::json!({});
            }
            merge(entry, value)?;
        } else {
            destination.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

impl Catalog {
    pub fn replace(&mut self, domain: Domain, value: Value) -> Result<(), String> {
        let list: Vec<Raw> = serde_json::from_value(
            value
                .get(domain.key())
                .cloned()
                .ok_or("missing Wave Link catalog list")?,
        )
        .map_err(|error| error.to_string())?;
        validate_ids(&list)?;
        let main = if domain == Domain::Outputs {
            serde_json::from_value(value.get("mainOutput").cloned().unwrap_or(Value::Null))
                .map_err(|error| error.to_string())?
        } else {
            self.main.clone()
        };
        self.domains.insert(domain, list);
        self.main = main;
        Ok(())
    }

    pub fn patch(&mut self, domain: Domain, value: Value) -> Result<(), String> {
        // Deserialize first so invalid known fields cannot become state.
        let patch: Raw =
            serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
        let list = self
            .domains
            .get(&domain)
            .ok_or("patch before catalog synchronization")?;
        let mut next = list.clone();
        match next.iter_mut().find(|entry| entry.id == patch.id) {
            Some(entry) => {
                let mut joined =
                    serde_json::to_value(&*entry).map_err(|error| error.to_string())?;
                merge(&mut joined, &value)?;
                *entry = serde_json::from_value(joined).map_err(|error| error.to_string())?;
            }
            None => next.push(patch),
        }
        validate_ids(&next)?;
        self.domains.insert(domain, next);
        Ok(())
    }

    pub fn invalidate(&mut self, domain: Domain) {
        self.domains.remove(&domain);
        if domain == Domain::Outputs {
            self.main = None;
        }
    }
    pub fn complete(&self) -> bool {
        Domain::ALL
            .iter()
            .all(|domain| self.domains.contains_key(domain))
    }

    pub fn apply(&self, state: &mut WaveLinkState) {
        state.controls.clear();
        state.channels.clear();
        state.mixes.clear();
        state.outputs.clear();
        for (domain, list) in &self.domains {
            for raw in list {
                let name = raw.name.as_deref().unwrap_or(&raw.id).to_string();
                match domain {
                    Domain::Channels => {
                        state.channels.push(WaveLinkChannel {
                            id: raw.id.clone(),
                            name: name.clone(),
                            apps: raw.apps.clone().unwrap_or_default(),
                        });
                        state.controls.push(control(
                            raw,
                            name.clone(),
                            WaveLinkTarget::Channel {
                                channel_id: raw.id.clone(),
                            },
                        ));
                        for mix in raw.mixes.iter().flatten() {
                            state.controls.push(control(
                                mix,
                                format!("{name} / {}", mix.name.as_deref().unwrap_or(&mix.id)),
                                WaveLinkTarget::ChannelMix {
                                    channel_id: raw.id.clone(),
                                    mix_id: mix.id.clone(),
                                },
                            ));
                        }
                    }
                    Domain::Mixes => {
                        state.mixes.push(WaveLinkMix {
                            id: raw.id.clone(),
                            name: name.clone(),
                        });
                        state.controls.push(control(
                            raw,
                            name,
                            WaveLinkTarget::Mix {
                                mix_id: raw.id.clone(),
                            },
                        ));
                    }
                    Domain::Inputs => {
                        for input in raw.inputs.iter().flatten() {
                            let input_name =
                                format!("{name} / {}", input.name.as_deref().unwrap_or(&input.id));
                            let mut mute = control(
                                input,
                                input_name.clone(),
                                WaveLinkTarget::Input {
                                    input_device_id: raw.id.clone(),
                                    input_id: input.id.clone(),
                                },
                            );
                            mute.level = None;
                            mute.can_set_level = false;
                            state.controls.push(mute);
                            if let Some(metadata) = input.gain.as_ref().and_then(|gain| {
                                Some(WaveLinkGain {
                                    value: gain.value?,
                                    min: gain.min?,
                                    max: gain.max?,
                                    unit: gain.unit.clone()?,
                                })
                            }) {
                                state.controls.push(WaveLinkControl { target: WaveLinkTarget::InputGain { input_device_id: raw.id.clone(), input_id: input.id.clone() }, name: input_name, level: None, muted: None, can_set_level: false, can_mute: false, reason: Some("Input gain is read-only: its value-to-range mapping is unvalidated".into()), gain: Some(metadata) });
                            }
                        }
                    }
                    Domain::Outputs => {
                        for output in raw.outputs.iter().flatten() {
                            let output_name = format!(
                                "{name} / {}",
                                output.name.as_deref().unwrap_or(&output.id)
                            );
                            state.controls.push(control(
                                output,
                                output_name.clone(),
                                WaveLinkTarget::Output {
                                    output_device_id: raw.id.clone(),
                                    output_id: output.id.clone(),
                                },
                            ));
                            state.outputs.push(WaveLinkOutput {
                                output_device_id: raw.id.clone(),
                                output_id: output.id.clone(),
                                name: output_name,
                                mix_id: output.mix_id.clone(),
                                can_select_main: true,
                                can_set_mix: output.mix_id.is_some(),
                            });
                        }
                    }
                }
            }
        }
        state
            .controls
            .sort_by_key(|control| format!("{:?}", control.target));
        state.channels.sort_by(|a, b| a.id.cmp(&b.id));
        state.mixes.sort_by(|a, b| a.id.cmp(&b.id));
        state.outputs.sort_by(|a, b| {
            (&a.output_device_id, &a.output_id).cmp(&(&b.output_device_id, &b.output_id))
        });
        state.main_output = self.main.clone().filter(|main| {
            state.outputs.iter().any(|output| {
                output.output_device_id == main.output_device_id
                    && output.output_id == main.output_id
            })
        });
        if state.status != WaveLinkStatus::Ready {
            for control in &mut state.controls {
                control.can_set_level = false;
                control.can_mute = false;
            }
            for output in &mut state.outputs {
                output.can_select_main = false;
                output.can_set_mix = false;
            }
        }
    }
}

fn control(raw: &Raw, name: String, target: WaveLinkTarget) -> WaveLinkControl {
    let level = raw
        .level
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value));
    WaveLinkControl {
        target,
        name,
        level,
        muted: raw.is_muted,
        can_set_level: level.is_some(),
        can_mute: raw.is_muted.is_some(),
        reason: if level.is_none() || raw.is_muted.is_none() {
            Some("Control fields are absent or unsupported in the Wave Link catalog".into())
        } else {
            None
        },
        gain: None,
    }
}
