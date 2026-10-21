use super::*;
use crate::profile::{AudioGroup, AudioMemberTarget};
use serde::Deserialize;
use std::collections::HashSet;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioGroupMemberReport {
    pub target: AudioMemberTarget,
    pub volume: Option<f64>,
    pub muted: Option<bool>,
    pub can_set_volume: bool,
    pub can_mute: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioGroupOutcome {
    Pending,
    Confirmed,
    Failed,
    Unconfirmed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioGroupMemberOperation {
    pub target: AudioMemberTarget,
    pub outcome: AudioGroupOutcome,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioGroupOperation {
    pub id: u64,
    pub members: Vec<AudioGroupMemberOperation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioGroupSummary {
    pub volume: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub mixed: bool,
    pub coverage: AudioGroupCoverage,
    pub mute: AudioGroupMute,
    pub readable: usize,
    pub muted: usize,
    pub unknown_mute: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioGroupCoverage {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioGroupMute {
    All,
    None,
    Partial,
    Unknown,
}

pub fn aggregate_group(members: &[AudioGroupMemberReport]) -> AudioGroupSummary {
    let volumes: Vec<_> = members
        .iter()
        .filter_map(|m| m.volume.filter(|v| v.is_finite()))
        .collect();
    let min = volumes.iter().copied().reduce(f64::min);
    let max = volumes.iter().copied().reduce(f64::max);
    let muted = members.iter().filter(|m| m.muted == Some(true)).count();
    let unmuted = members.iter().filter(|m| m.muted == Some(false)).count();
    let unknown_mute = members.len() - muted - unmuted;
    let all_muted = !members.is_empty() && muted == members.len();
    AudioGroupSummary {
        volume: if volumes.is_empty() {
            None
        } else if all_muted {
            Some(0.0)
        } else {
            Some(volumes.iter().sum::<f64>() / volumes.len() as f64)
        },
        min,
        max,
        // Comparing to min + tolerance preserves the inclusive decimal boundary.
        mixed: min.zip(max).is_some_and(|(min, max)| max > min + 0.01),
        coverage: if volumes.is_empty() {
            AudioGroupCoverage::Unavailable
        } else if volumes.len() == members.len() {
            AudioGroupCoverage::Complete
        } else {
            AudioGroupCoverage::Partial
        },
        mute: if all_muted {
            AudioGroupMute::All
        } else if !members.is_empty() && unmuted == members.len() {
            AudioGroupMute::None
        } else if muted > 0 && unmuted > 0 {
            AudioGroupMute::Partial
        } else {
            AudioGroupMute::Unknown
        },
        readable: volumes.len(),
        muted,
        unknown_mute,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioGroupReport {
    pub group_id: String,
    pub revision: u64,
    pub members: Vec<AudioGroupMemberReport>,
    pub summary: AudioGroupSummary,
    pub restore_owned: bool,
    pub busy: bool,
    pub operation: Option<AudioGroupOperation>,
}

pub(super) type GroupNotifier = Arc<dyn Fn(Vec<AudioGroupReport>) + Send + Sync>;

#[derive(Default)]
pub(super) struct GroupRuntime {
    definitions: Vec<AudioGroup>,
    operations: HashMap<String, AudioGroupOperation>,
    busy: HashSet<String>,
    revision: u64,
    next_operation: u64,
}

struct Observation {
    report: AudioGroupMemberReport,
    state: Option<TargetState>,
    legacy_pids: Option<Vec<u32>>,
}

fn group_native(port: &dyn AudioControl) -> SystemAudioState {
    let mut native = read_state(port);
    if port.status().permission != AudioPermission::Granted {
        native.can_set_app_volume = false;
        native.can_mute_apps = false;
    }
    native
}

fn observe(
    member: &AudioMemberTarget,
    native: &SystemAudioState,
    wave: Option<&crate::wavelink::WaveLinkState>,
    fine: Option<&crate::finetune::FineTuneState>,
) -> Observation {
    let mut report = AudioGroupMemberReport {
        target: member.clone(),
        volume: None,
        muted: None,
        can_set_volume: false,
        can_mute: false,
        reason: None,
    };
    let mut state = None;
    let mut legacy_pids = None;
    match member {
        AudioMemberTarget::Device { .. } | AudioMemberTarget::App { .. } => {
            state = native.resolve(&member.target());
            if let Some(found) = &state {
                report.volume = Some(f64::from(found.volume));
                report.muted = Some(found.muted);
                report.can_set_volume = found.can_set_volume;
                report.can_mute = found.can_mute;
            } else {
                report.reason = Some("native endpoint is unavailable".into());
            }
        }
        AudioMemberTarget::WaveLink { target } => {
            if let Some(control) = wave.and_then(|s| s.control(target)) {
                report.volume = control.level;
                report.muted = control.muted;
                report.can_set_volume = control.can_set_level && control.level.is_some();
                report.can_mute = control.can_mute && control.muted.is_some();
                report.reason = control.reason.clone();
                state = wave.and_then(|s| wave_link_state(s, target));
            } else {
                report.reason = Some("Wave Link target is unavailable or not ready".into());
            }
        }
        AudioMemberTarget::FineTune { target } => {
            if let Some(fine) = fine {
                match target {
                    crate::finetune::FineTuneTarget::App { app_id } => match fine.app(app_id) {
                        Ok(app) => {
                            report.volume = Some(app.volume);
                            report.muted = Some(app.muted);
                            report.reason = fine.writable_app(app_id).err();
                            report.can_set_volume = report.reason.is_none();
                            report.can_mute = report.reason.is_none();
                            state = fine_tune_state(fine, target);
                            if !fine.supports_managed_apps() {
                                let mut pids = app.pids.clone();
                                pids.sort_unstable();
                                pids.dedup();
                                legacy_pids = Some(pids);
                            }
                        }
                        Err(error) => report.reason = Some(error),
                    },
                    crate::finetune::FineTuneTarget::Input { .. }
                    | crate::finetune::FineTuneTarget::Output { .. } => {
                        if let Some(reason) = fine.target_reason(target) {
                            report.reason = Some(reason);
                        } else {
                            if let Some((device, _)) = fine.device(target) {
                                report.volume = device
                                    .capabilities
                                    .volume_readable
                                    .then_some(device.volume)
                                    .flatten();
                                report.muted = device
                                    .capabilities
                                    .mute_readable
                                    .then_some(device.muted)
                                    .flatten();
                            }
                            report.reason = Some("FineTune device volume/mute is read-only".into());
                        }
                    }
                }
            } else {
                report.reason = Some("FineTune is unavailable".into());
            }
        }
    }
    if !report.can_set_volume && report.reason.is_none() {
        report.reason = Some("volume control is unsupported or unreadable".into());
    }
    Observation {
        report,
        state,
        legacy_pids,
    }
}

struct AdmittedMember {
    index: usize,
    target: AudioMemberTarget,
    key: Resolved,
    legacy_pids: Option<Vec<u32>>,
}

impl SystemAudio {
    pub fn group_report(&self, id: &str) -> Option<AudioGroupReport> {
        self.live
            .lock()
            .audio_groups
            .iter()
            .find(|g| g.group_id == id)
            .cloned()
    }
    pub fn set_group_notifier(&self, notify: GroupNotifier) {
        *self.group_notifier.lock() = Some(notify);
    }

    pub fn configure_groups(&self, definitions: &[AudioGroup]) {
        let _serial = self.group_refresh.lock();
        let mut groups = self.groups.lock();
        if groups.definitions != definitions {
            let changed: Vec<_> = groups
                .definitions
                .iter()
                .filter(|old| !definitions.iter().any(|new| new == *old))
                .map(|g| g.id.clone())
                .collect();
            for id in changed {
                groups.operations.remove(&id);
                groups.busy.remove(&id);
            }
            groups.definitions = definitions.to_vec();
        }
    }

    fn group_observations(&self, members: &[AudioMemberTarget]) -> Vec<Observation> {
        let native = if members.iter().any(|m| {
            matches!(
                m,
                AudioMemberTarget::Device { .. } | AudioMemberTarget::App { .. }
            )
        }) {
            group_native(self.port.as_ref())
        } else {
            SystemAudioState::default()
        };
        let wave = self.wave_link.as_ref().map(|w| w.state());
        let fine = self.fine_tune.as_ref().map(|f| f.state());
        let permission = self.port.status().permission;
        members
            .iter()
            .map(|m| {
                let mut observation = observe(m, &native, wave.as_ref(), fine.as_ref());
                if matches!(m, AudioMemberTarget::App { .. })
                    && permission != AudioPermission::Granted
                {
                    observation.report.can_set_volume = false;
                    observation.report.can_mute = false;
                    observation.report.reason =
                        Some(format!("native app audio permission is {permission:?}"));
                }
                observation
            })
            .collect()
    }

    pub fn refresh_groups(&self) -> Vec<AudioGroupReport> {
        let serial = self.group_refresh.lock();
        let definitions = self.groups.lock().definitions.clone();
        let observed: Vec<_> = definitions
            .iter()
            .map(|g| {
                (
                    g.id.clone(),
                    self.group_observations(&g.members)
                        .into_iter()
                        .map(|o| o.report)
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let mut groups = self.groups.lock();
        groups.revision += 1;
        let reports: Vec<_> = observed
            .into_iter()
            .filter(|(id, _)| groups.definitions.iter().any(|g| &g.id == id))
            .map(|(group_id, members)| AudioGroupReport {
                revision: groups.revision,
                summary: aggregate_group(&members),
                members,
                restore_owned: false,
                busy: groups.busy.contains(&group_id),
                operation: groups.operations.get(&group_id).cloned(),
                group_id,
            })
            .collect();
        drop(groups);
        self.live.lock().audio_groups = reports.clone();
        let notifier = self.group_notifier.lock().clone();
        if let Some(notify) = notifier {
            notify(reports.clone());
        }
        drop(serial);
        reports
    }

    pub(super) fn start_group_volume(
        &self,
        id: &str,
        volume: f32,
        unmute: bool,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        if !volume.is_finite() {
            return Err("audio group volume must be finite".into());
        }
        let volume = volume.clamp(0.0, 1.0);
        let definition = self
            .groups
            .lock()
            .definitions
            .iter()
            .find(|g| g.id == id)
            .cloned()
            .ok_or_else(|| format!("no audio group {id}"))?;
        let observations = self.group_observations(&definition.members);
        let mut admitted = Vec::new();
        let mut seen = HashSet::new();
        let mut outcomes = Vec::new();
        for (index, observation) in observations.into_iter().enumerate() {
            let mut reason = observation.report.reason.clone();
            if observation.report.can_set_volume {
                if let Some(state) = observation.state {
                    if seen.insert(state.key.clone()) {
                        let target = match &state.key {
                            Resolved::Device { id, kind } => AudioMemberTarget::Device {
                                device: DeviceRef::Named {
                                    id: id.clone(),
                                    kind: *kind,
                                    label: String::new(),
                                },
                            },
                            _ => observation.report.target.clone(),
                        };
                        admitted.push(AdmittedMember {
                            index,
                            target,
                            key: state.key,
                            legacy_pids: observation.legacy_pids,
                        });
                        reason = None;
                    } else {
                        reason = Some("duplicate resolved endpoint".into());
                    }
                }
            }
            outcomes.push(AudioGroupMemberOperation {
                target: observation.report.target,
                outcome: if reason.is_none() {
                    AudioGroupOutcome::Pending
                } else {
                    AudioGroupOutcome::Skipped
                },
                message: reason,
            });
        }
        let operation_id = {
            let mut groups = self.groups.lock();
            if groups.busy.contains(id) {
                return Err("audio group has an operation in progress".into());
            }
            if !groups.definitions.contains(&definition) {
                return Err("audio group changed during admission".into());
            }
            groups.next_operation += 1;
            let operation_id = groups.next_operation;
            groups.operations.insert(
                id.to_string(),
                AudioGroupOperation {
                    id: operation_id,
                    members: outcomes,
                },
            );
            if !admitted.is_empty() {
                groups.busy.insert(id.to_string());
            }
            operation_id
        };
        self.refresh_groups();
        let (reply, response) = oneshot::channel();
        if admitted.is_empty() {
            let _ = reply.send(Err("no eligible audio group members".into()));
            return Ok(response);
        }
        let audio = self.clone();
        let id = id.to_string();
        let job = Box::new(move |writer: &Writer| {
            let mut errors = Vec::new();
            let mut attempted = false;
            for member in admitted {
                let operation_valid = || {
                    audio
                        .groups
                        .lock()
                        .operations
                        .get(&id)
                        .is_some_and(|op| op.id == operation_id)
                };
                let (outcome, message) = if !operation_valid() {
                    (
                        AudioGroupOutcome::Skipped,
                        Some("audio group changed before execution".into()),
                    )
                } else {
                    execute_member(writer, &member, volume, unmute, &operation_valid)
                };
                attempted |= outcome != AudioGroupOutcome::Skipped;
                if matches!(
                    outcome,
                    AudioGroupOutcome::Failed | AudioGroupOutcome::Unconfirmed
                ) {
                    errors.push(
                        message
                            .clone()
                            .unwrap_or_else(|| "member write unconfirmed".into()),
                    );
                }
                {
                    let mut groups = audio.groups.lock();
                    if let Some(op) = groups
                        .operations
                        .get_mut(&id)
                        .filter(|op| op.id == operation_id)
                    {
                        op.members[member.index].outcome = outcome;
                        op.members[member.index].message = message;
                    }
                }
                audio.refresh_groups();
            }
            {
                let mut groups = audio.groups.lock();
                if groups
                    .operations
                    .get(&id)
                    .is_some_and(|op| op.id == operation_id)
                {
                    groups.busy.remove(&id);
                }
            }
            audio.refresh_groups();
            if !attempted {
                errors.push("no audio group members remained eligible at execution".into());
            }
            let _ = reply.send(if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            });
        });
        if self.jobs.send(job).is_err() {
            let mut groups = self.groups.lock();
            if let Some(operation) = groups
                .operations
                .get_mut(&definition.id)
                .filter(|op| op.id == operation_id)
            {
                for member in &mut operation.members {
                    if member.outcome == AudioGroupOutcome::Pending {
                        member.outcome = AudioGroupOutcome::Skipped;
                        member.message = Some("the system audio writer has stopped".into());
                    }
                }
                groups.busy.remove(&definition.id);
            }
            drop(groups);
            self.refresh_groups();
            return Err("the system audio writer has stopped".into());
        }
        Ok(response)
    }
}

fn execute_member(
    writer: &Writer,
    member: &AdmittedMember,
    volume: f32,
    unmute: bool,
    operation_valid: &impl Fn() -> bool,
) -> (AudioGroupOutcome, Option<String>) {
    if matches!(member.target, AudioMemberTarget::App { .. })
        && writer.port.status().permission != AudioPermission::Granted
    {
        return (
            AudioGroupOutcome::Skipped,
            Some(format!(
                "native app audio permission is {:?}",
                writer.port.status().permission
            )),
        );
    }
    let native = match member.key {
        Resolved::Device { .. } | Resolved::App { .. } => group_native(writer.port.as_ref()),
        _ => SystemAudioState::default(),
    };
    let wave = writer.wave_link.as_ref().map(|w| w.state());
    let fine = writer.fine_tune.as_ref().map(|f| f.state());
    let observed = observe(&member.target, &native, wave.as_ref(), fine.as_ref());
    if !observed.report.can_set_volume
        || observed.state.as_ref().is_none_or(|s| s.key != member.key)
        || observed.legacy_pids != member.legacy_pids
    {
        return (
            AudioGroupOutcome::Skipped,
            Some(
                observed
                    .report
                    .reason
                    .unwrap_or_else(|| "captured endpoint/session ended or changed".into()),
            ),
        );
    }
    if unmute && observed.report.muted == Some(true) && observed.report.can_mute {
        if !operation_valid() {
            return (
                AudioGroupOutcome::Skipped,
                Some("audio group operation invalidated; unmute and volume not written".into()),
            );
        }
        if let Err(error) = writer.set_muted(&member.key, false) {
            return (
                AudioGroupOutcome::Failed,
                Some(format!(
                    "required unmute failed; volume not written: {error}"
                )),
            );
        }
        let native = match member.key {
            Resolved::Device { .. } | Resolved::App { .. } => group_native(writer.port.as_ref()),
            _ => SystemAudioState::default(),
        };
        let wave = writer.wave_link.as_ref().map(|w| w.state());
        let fine = writer.fine_tune.as_ref().map(|f| f.state());
        let unmuted = observe(&member.target, &native, wave.as_ref(), fine.as_ref());
        if unmuted.report.muted != Some(false)
            || !unmuted.report.can_set_volume
            || unmuted
                .state
                .as_ref()
                .is_none_or(|state| state.key != member.key)
            || unmuted.legacy_pids != member.legacy_pids
        {
            return (
                AudioGroupOutcome::Unconfirmed,
                Some(
                    "required unmute or endpoint eligibility was not confirmed; volume not written"
                        .into(),
                ),
            );
        }
    }
    if !operation_valid() {
        return (if unmute && observed.report.muted == Some(true) && observed.report.can_mute { AudioGroupOutcome::Unconfirmed } else { AudioGroupOutcome::Skipped }, Some("audio group operation invalidated; volume not written (any already-dispatched unmute is not rolled back)".into()));
    }
    if let Err(error) = writer.set_volume(&member.key, volume) {
        return (AudioGroupOutcome::Unconfirmed, Some(error));
    }
    let native = match member.key {
        Resolved::Device { .. } | Resolved::App { .. } => group_native(writer.port.as_ref()),
        _ => SystemAudioState::default(),
    };
    let wave = writer.wave_link.as_ref().map(|w| w.state());
    let fine = writer.fine_tune.as_ref().map(|f| f.state());
    let readback = observe(&member.target, &native, wave.as_ref(), fine.as_ref());
    if readback.state.as_ref().is_some_and(|s| s.key == member.key)
        && readback.legacy_pids == member.legacy_pids
        && readback
            .report
            .volume
            .is_some_and(|v| (v - f64::from(volume)).abs() <= 0.000_001)
        && (!unmute
            || observed.report.muted != Some(true)
            || !observed.report.can_mute
            || readback.report.muted == Some(false))
    {
        (AudioGroupOutcome::Confirmed, None)
    } else {
        (
            AudioGroupOutcome::Unconfirmed,
            Some(
                "write dispatched, but requested state was not confirmed by backend readback"
                    .into(),
            ),
        )
    }
}
