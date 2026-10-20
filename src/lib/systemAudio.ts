import type { AudioStatus, AudioTarget, Fader, WaveLinkState } from "./api";
import { waveLinkControl } from "./waveLink";

export const DEFAULT_INPUT: AudioTarget = { type: "device", device: { type: "defaultInput" } };
export const DEFAULT_OUTPUT: AudioTarget = { type: "device", device: { type: "defaultOutput" } };

export function audioTargetConfigured(target: AudioTarget): boolean {
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

/** Resolve stable references against the latest OS snapshot, never a requested level. */
export function audioState(target: AudioTarget | null | undefined, status: AudioStatus | null, waveLink: WaveLinkState | null = null) {
  if (!target) return null;
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

export function audioControllable(target: AudioTarget, status: AudioStatus | null, control: "volume" | "mute" = "volume", waveLink: WaveLinkState | null = null) {
  if (target.type === "waveLink") {
    const found = waveLink?.status === "ready" ? waveLinkControl(target.target, waveLink) : null;
    return !!found && (control === "volume" ? found.canSetLevel && found.level !== null : found.canMute && found.muted !== null);
  }
  const state = audioState(target, status);
  if (target.type === "app") return !!state && !!status && (control === "volume" ? status.canSetAppVolume : status.canMuteApps);
  return !!state && (!("canSetVolume" in state) || (control === "volume" ? state.canSetVolume : state.canMute || state.emulated));
}

export function audioFader(fader: Fader, status: AudioStatus | null, waveLink: WaveLinkState | null = null): Fader {
  if (!fader.bound) return fader;
  const state = audioControllable(fader.bound, status, "volume", waveLink) ? audioState(fader.bound, status, waveLink) : null;
  return { ...fader, value: state && !state.muted && state.volume !== null ? fader.min + (fader.max - fader.min) * state.volume : fader.min };
}
