import type { Action, AudioStatus, AudioTarget, Fader, WaveLinkState, FineTuneState } from "./api";
import { waveLinkControl } from "./waveLink";
import { fineTuneApp, fineTuneReason } from "./fineTune";

export const DEFAULT_INPUT: AudioTarget = { type: "device", device: { type: "defaultInput" } };
export const DEFAULT_OUTPUT: AudioTarget = { type: "device", device: { type: "defaultOutput" } };

export function audioTargetConfigured(target: AudioTarget): boolean {
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
export function audioState(target: AudioTarget | null | undefined, status: AudioStatus | null, waveLink: WaveLinkState | null = null, fineTune: FineTuneState | null = null) {
  if (!target) return null;
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
  return status.devices.find((d) => d.present && (ref.type === "named"
    ? d.kind === ref.kind && (ref.id ? d.id === ref.id : d.name.toLowerCase() === ref.label.trim().toLowerCase())
    : d.isDefault && d.kind === (ref.type === "defaultInput" ? "input" : "output"))) ?? null;
}

export function audioControllable(target: AudioTarget, status: AudioStatus | null, control: "volume" | "mute" = "volume", waveLink: WaveLinkState | null = null, fineTune: FineTuneState | null = null) {
  if (target.type === "fineTune") return target.target.type === "app" && !fineTuneReason(target.target, fineTune);
  if (target.type === "waveLink") {
    const found = waveLink?.status === "ready" ? waveLinkControl(target.target, waveLink) : null;
    return !!found && (control === "volume" ? found.canSetLevel && found.level !== null : found.canMute && found.muted !== null);
  }
  const state = audioState(target, status);
  if (target.type === "app") return !!state && !!status && (control === "volume" ? status.canSetAppVolume : status.canMuteApps);
  return !!state && (!("canSetVolume" in state) || (control === "volume" ? state.canSetVolume : state.canMute || state.emulated));
}

/** FineTune snapshots remain readable even when capture permission prevents writes. */
export function audioFaderReadable(target: AudioTarget, status: AudioStatus | null, waveLink: WaveLinkState | null = null, fineTune: FineTuneState | null = null) {
  return target.type === "fineTune" ? !!audioState(target, status, waveLink, fineTune) : audioControllable(target, status, "volume", waveLink);
}

export function audioFader(fader: Fader, status: AudioStatus | null, waveLink: WaveLinkState | null = null, fineTune: FineTuneState | null = null): Fader {
  if (!fader.bound) return fader;
  const state = audioFaderReadable(fader.bound, status, waveLink, fineTune) ? audioState(fader.bound, status, waveLink, fineTune) : null;
  return { ...fader, value: state && !state.muted && state.volume !== null ? fader.min + (fader.max - fader.min) * state.volume : fader.min };
}
