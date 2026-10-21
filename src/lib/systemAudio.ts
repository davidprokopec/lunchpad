import type { Action, AudioMemberTarget, AudioStatus, AudioTarget, Fader, WaveLinkState, FineTuneState, AudioGroupMemberReport, AudioGroupSummary, AudioGroupReport } from "./api";
import { waveLinkControl } from "./waveLink";
import { fineTuneApp, fineTuneDevice, fineTuneReason } from "./fineTune";
import { faderMutedColor, faderMutedLevel } from "./fader";
import { resolveDevice, type DeviceMatch } from "./deviceIdentity";

export const DEFAULT_INPUT: AudioTarget = { type: "device", device: { type: "defaultInput" } };
export const DEFAULT_OUTPUT: AudioTarget = { type: "device", device: { type: "defaultOutput" } };

export function audioTargetConfigured(target: AudioTarget): boolean {
  if (target.type === "group") return !!target.groupId.trim();
  if (target.type === "fineTune") return target.target.type === "app" ? !!target.target.appId : !!target.target.deviceUid;
  if (target.type === "app") return !!target.app.id;
  if (target.type !== "waveLink") return true;
  const ref = target.target;
  switch (ref.type) {
    case "channel": return !!ref.channelId;
    case "channelMix": return !!ref.channelId && !!ref.mixId;
    case "mix": return !!ref.mixId;
    case "output": return !!ref.outputDeviceId && !!ref.outputId;
    case "input": case "inputGain": return !!ref.inputDeviceId && !!ref.inputId;
  }
}

/** Structural references only: saved offline targets must remain editable. */
export function audioActionConfigured(action: Action): boolean {
  switch (action.type) {
    case "setAudio": return audioTargetConfigured(action.target) && (action.target.type !== "fineTune" || action.target.target.type === "app");
    case "fineTuneSetAppRouting": return !!action.appId && (action.routing.mode === "followDefault" || !!action.routing.deviceUID);
    case "fineTuneSetDefaultOutput": case "fineTuneSetPreferredInput": return !!action.deviceUid;
    case "waveLinkSetMainOutput": return !!action.outputDeviceId && !!action.outputId;
    case "waveLinkSetMonitoredMix": return !!action.outputDeviceId && !!action.outputId && !!action.mixId;
    default: return true;
  }
}

/** Resolve stable references against the latest OS snapshot, never a requested level. */
export function audioState(target: AudioTarget | null | undefined, status: AudioStatus | null, waveLink: WaveLinkState | null = null, fineTune: FineTuneState | null = null, groups: AudioGroupReport[] = []) {
  if (!target) return null;
  if (target.type === "group") {
    const report = groups.find((group) => group.groupId === target.groupId);
    if (!report) return null;
    const summary = aggregateGroup(report.members);
    return { volume: summary.volume, muted: summary.mute === "all" };
  }
  if (target.type === "fineTune") return target.target.type === "app" ? fineTuneApp(target.target.appId, fineTune) : null;
  if (target.type === "waveLink") {
    if (waveLink?.status !== "ready") return null;
    const control = waveLinkControl(target.target, waveLink);
    return control ? { ...control, volume: control.level } : null;
  }
  if (!status) return null;
  if (target.type === "app") {
    if (!status.appsAvailable) return null;
    return status.apps.find((a) => a.id === target.app.id && a.present) ?? null;
  }
  if (!status.available) return null;
  const ref = target.device;
  // A saved id may come back under another driver prefix or only by its label (see deviceIdentity.ts).
  if (ref.type === "named" && ref.id) return resolveDevice(ref.id, ref.label, ref.kind, status.devices.filter((d) => d.present))?.device ?? null;
  return status.devices.find((d) => d.present && (ref.type === "named"
    ? d.kind === ref.kind && d.name.toLowerCase() === ref.label.trim().toLowerCase()
    : d.isDefault && d.kind === (ref.type === "defaultInput" ? "input" : "output"))) ?? null;
}

export function audioControllable(target: AudioTarget, status: AudioStatus | null, control: "volume" | "mute" = "volume", waveLink: WaveLinkState | null = null, fineTune: FineTuneState | null = null, groups: AudioGroupReport[] = []) {
  if (target.type === "group") {
    const report = groups.find((group) => group.groupId === target.groupId);
    return control === "volume" && !!report && !report.busy && report.members.some((member) => member.canSetVolume);
  }
  if (target.type === "fineTune") return target.target.type === "app" && !fineTuneReason(target.target, fineTune);
  if (target.type === "waveLink") {
    const found = waveLink?.status === "ready" ? waveLinkControl(target.target, waveLink) : null;
    return !!found && (control === "volume" ? found.canSetLevel && found.level !== null : found.canMute && found.muted !== null);
  }
  const state = audioState(target, status);
  if (target.type === "app") return !!state && !!status && (control === "volume" ? status.canSetAppVolume : status.canMuteApps);
  return !!state && (!("canSetVolume" in state) || (control === "volume" ? state.canSetVolume : state.canMute || state.emulated));
}

export function aggregateGroup(members: AudioGroupMemberReport[]): AudioGroupSummary {
  const volumes = members.flatMap((member) => member.volume !== null && Number.isFinite(member.volume) ? [member.volume] : []);
  const min = volumes.length ? Math.min(...volumes) : null;
  const max = volumes.length ? Math.max(...volumes) : null;
  const muted = members.filter((member) => member.muted === true).length;
  const unmuted = members.filter((member) => member.muted === false).length;
  const allMuted = members.length > 0 && muted === members.length;
  return {
    volume: !volumes.length ? null : allMuted ? 0 : volumes.reduce((sum, volume) => sum + volume, 0) / volumes.length,
    min, max,
    mixed: min !== null && max !== null && max > min + 0.01,
    coverage: !volumes.length ? "unavailable" : volumes.length === members.length ? "complete" : "partial",
    mute: allMuted ? "all" : members.length > 0 && unmuted === members.length ? "none" : muted > 0 && unmuted > 0 ? "partial" : "unknown",
    readable: volumes.length, muted, unknownMute: members.length - muted - unmuted,
  };
}

/** FineTune snapshots remain readable even when capture permission prevents writes. */
export function audioFaderReadable(target: AudioTarget, status: AudioStatus | null, waveLink: WaveLinkState | null = null, fineTune: FineTuneState | null = null, groups: AudioGroupReport[] = []) {
  if (target.type === "group") return audioState(target, status, waveLink, fineTune, groups)?.volume != null;
  return target.type === "fineTune" ? !!audioState(target, status, waveLink, fineTune) : audioControllable(target, status, "volume", waveLink);
}

/**
 * How a bound fader looks with the latest observed state. Muted normally shows
 * the minimum. With "keep level" (`mutedLevel`) a confirmed mute (a single
 * target, or a group whose every member is known muted and has a readable
 * level) keeps the stored level and is painted in `mutedColor`. Mirrors
 * `Fader::render_state` in Rust. Display only: nothing is written.
 */
export function audioFaderView(fader: Fader, status: AudioStatus | null, waveLink: WaveLinkState | null = null, fineTune: FineTuneState | null = null, groups: AudioGroupReport[] = []): { fader: Fader; mutedColor: [number, number, number] | null } {
  if (!fader.bound) return { fader, mutedColor: null };
  const state = audioFaderReadable(fader.bound, status, waveLink, fineTune, groups) ? audioState(fader.bound, status, waveLink, fineTune, groups) : null;
  if (state?.muted && faderMutedLevel(fader)) return { fader, mutedColor: faderMutedColor(fader) };
  return { fader: { ...fader, value: state && !state.muted && state.volume !== null ? fader.min + (fader.max - fader.min) * state.volume : fader.min }, mutedColor: null };
}

export function audioFader(fader: Fader, status: AudioStatus | null, waveLink: WaveLinkState | null = null, fineTune: FineTuneState | null = null, groups: AudioGroupReport[] = []): Fader {
  return audioFaderView(fader, status, waveLink, fineTune, groups).fader;
}

/**
 * Which live device a saved device target (native named device, or FineTune
 * output/input) currently means, and how it was matched. `update` is the same
 * target pointing at that live device (its id and name), for an explicit
 * "Update to current device" through the normal draft/save path. Display only:
 * the saved target is never rewritten here.
 */
export function savedDeviceMatch(target: AudioTarget | null | undefined, status: AudioStatus | null, fineTune: FineTuneState | null = null): { match: DeviceMatch; name: string; id: string; update: AudioMemberTarget } | null {
  if (target?.type === "device" && target.device.type === "named" && target.device.id) {
    const { id, label, kind } = target.device;
    const found = status?.available ? resolveDevice(id, label, kind, status.devices.filter((d) => d.present)) : null;
    return found && { match: found.match, name: found.device.name, id: found.device.id, update: { type: "device", device: { type: "named", id: found.device.id, kind, label: found.device.name } } };
  }
  if (target?.type === "fineTune" && target.target.type !== "app") {
    const type = target.target.type;
    const found = fineTuneDevice(target.target, fineTune);
    return found && { match: found.match, name: found.device.name, id: found.device.uid, update: { type: "fineTune", target: { type, deviceUid: found.device.uid, label: found.device.name } } };
  }
  return null;
}
