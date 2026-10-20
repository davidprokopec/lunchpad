use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FineTuneTarget {
    App { app_id: String },
    Output { device_uid: String },
    Input { device_uid: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FineTuneStatus {
    #[default]
    Disabled,
    Disconnected,
    Connecting,
    Synchronizing,
    Ready,
    Unsupported,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FineTuneRoutingStatus {
    Unavailable,
    Pending,
    Applied,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FineTuneVolumeSource {
    Hardware,
    Ddc,
    Software,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FineTuneCapturePermission {
    Unknown,
    Authorized,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FineTunePermissionSource {
    Preflight,
    Assumed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum FineTuneRouting {
    #[serde(rename = "followDefault")]
    FollowDefault,
    #[serde(rename = "single")]
    Single {
        #[serde(rename = "deviceUID")]
        device_uid: String,
    },
    #[serde(rename = "multi")]
    Multi {
        #[serde(rename = "deviceUIDs")]
        device_uids: Vec<String>,
    },
}

/// Only the two writable routing modes; multi is observable, not writable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum FineTuneRoutingRequest {
    #[serde(rename = "followDefault")]
    FollowDefault,
    #[serde(rename = "single")]
    Single {
        #[serde(rename = "deviceUID")]
        device_uid: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FineTuneRoutingState {
    pub requested: FineTuneRouting,
    #[serde(rename = "appliedDeviceUIDs")]
    #[serde(deserialize_with = "Option::deserialize")]
    pub applied_device_uids: Option<Vec<String>>,
    pub status: FineTuneRoutingStatus,
    #[serde(deserialize_with = "Option::deserialize")]
    pub error: Option<FineTuneError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FineTuneError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FineTuneApp {
    pub app_id: String,
    pub pids: Vec<u32>,
    pub name: String,
    pub active: bool,
    pub volume: f64,
    pub boost: f64,
    pub muted: bool,
    pub routing: FineTuneRoutingState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FineTuneManagedApp {
    #[serde(flatten)]
    pub app: FineTuneApp,
    pub running: bool,
    pub manageable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FineTuneDeviceCapabilities {
    pub volume_readable: bool,
    pub mute_readable: bool,
    pub selectable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FineTuneDevice {
    pub uid: String,
    pub name: String,
    pub direction: crate::profile::AudioDeviceKind,
    pub is_default: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    pub volume: Option<f64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub muted: Option<bool>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub volume_source: Option<FineTuneVolumeSource>,
    pub capabilities: FineTuneDeviceCapabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FineTuneInputPolicy {
    pub lock_enabled: bool,
    #[serde(rename = "preferredDeviceUID")]
    #[serde(deserialize_with = "Option::deserialize")]
    pub preferred_device_uid: Option<String>,
    #[serde(rename = "lockedDeviceUID")]
    #[serde(deserialize_with = "Option::deserialize")]
    pub locked_device_uid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FineTunePermission {
    pub audio_capture: FineTuneCapturePermission,
    pub source: FineTunePermissionSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FineTuneSnapshot {
    pub apps: Vec<FineTuneApp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_apps: Option<Vec<FineTuneManagedApp>>,
    pub devices: Vec<FineTuneDevice>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub default_output: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub default_input: Option<String>,
    pub input_policy: FineTuneInputPolicy,
    pub permission: FineTunePermission,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FineTuneState {
    pub status: FineTuneStatus,
    pub connected: bool,
    pub capabilities: Vec<String>,
    pub error: Option<String>,
    pub app_version: Option<String>,
    pub session_id: Option<String>,
    /// Local lifecycle revision, suitable for UI receipt fencing.
    pub revision: u64,
    /// Connection generation fences queued writes, including reconnect to the same listener.
    pub generation: u64,
    pub server_revision: Option<u64>,
    pub snapshot: Option<FineTuneSnapshot>,
}

impl FineTuneState {
    pub fn supports_managed_apps(&self) -> bool {
        self.capabilities.iter().any(|value| value == "managedApps")
    }

    pub fn app_catalogue(&self) -> impl Iterator<Item = &FineTuneApp> {
        let managed = self.supports_managed_apps();
        self.snapshot.iter().flat_map(move |snapshot| {
            snapshot.apps.iter().filter(move |_| !managed).chain(
                snapshot
                    .managed_apps
                    .iter()
                    .flatten()
                    .filter(move |_| managed)
                    .map(|row| &row.app),
            )
        })
    }

    pub fn app(&self, app_id: &str) -> Result<&FineTuneApp, String> {
        if self.status != FineTuneStatus::Ready {
            return Err("FineTune is not ready".into());
        }
        self.snapshot.as_ref().ok_or("FineTune state unavailable")?;
        let mut matches = self.app_catalogue().filter(|app| app.app_id == app_id);
        let app = matches.next().ok_or("FineTune app is missing")?;
        if matches.next().is_some() {
            return Err("FineTune app is ambiguous".into());
        }
        if !self.supports_managed_apps() && !app.active {
            return Err("FineTune app is inactive".into());
        }
        Ok(app)
    }

    pub fn writable_app(&self, app_id: &str) -> Result<&FineTuneApp, String> {
        let app = self.app(app_id)?;
        let snapshot = self.snapshot.as_ref().ok_or("FineTune state unavailable")?;
        if snapshot.permission.audio_capture != FineTuneCapturePermission::Authorized {
            return Err("FineTune audio capture permission is not authorized".into());
        }
        if self.supports_managed_apps()
            && !snapshot.managed_apps.as_ref().is_some_and(|rows| {
                rows.iter()
                    .any(|row| row.app.app_id == app_id && row.manageable)
            })
        {
            return Err("FineTune app is not manageable (ignored or unavailable)".into());
        }
        Ok(app)
    }

    pub fn target_reason(&self, target: &FineTuneTarget) -> Option<String> {
        match target {
            FineTuneTarget::App { app_id } => self.app(app_id).err(),
            _ if self.status != FineTuneStatus::Ready => Some("FineTune is not ready".into()),
            FineTuneTarget::Output { device_uid } | FineTuneTarget::Input { device_uid } => {
                let direction = if matches!(target, FineTuneTarget::Output { .. }) {
                    crate::profile::AudioDeviceKind::Output
                } else {
                    crate::profile::AudioDeviceKind::Input
                };
                (!self.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot
                        .devices
                        .iter()
                        .any(|d| d.uid == *device_uid && d.direction == direction)
                }))
                .then(|| "FineTune device is missing".into())
            }
        }
    }
}
