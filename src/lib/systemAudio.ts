import type { AudioStatus, AudioTarget, Fader } from "./api";

export const DEFAULT_INPUT: AudioTarget = { type: "device", device: { type: "defaultInput" } };
export const DEFAULT_OUTPUT: AudioTarget = { type: "device", device: { type: "defaultOutput" } };

/** Resolve stable references against the latest OS snapshot, never a requested level. */
export function audioState(target: AudioTarget | null | undefined, status: AudioStatus | null) {
  if (!target || !status) return null;
  if (target.type === "app") {
    if (!status.appsAvailable || status.permission === "denied" || status.permission === "unconfirmed") return null;
    return status.apps.find((a) => a.id === target.app.id && a.present) ?? null;
  }
  if (!status.available) return null;
  const ref = target.device;
  return status.devices.find((d) => d.present && (ref.type === "named"
    ? d.kind === ref.kind && (ref.id ? d.id === ref.id : d.name.toLowerCase() === ref.label.trim().toLowerCase())
    : d.isDefault && d.kind === (ref.type === "defaultInput" ? "input" : "output"))) ?? null;
}

export function audioControllable(target: AudioTarget, status: AudioStatus | null, control: "volume" | "mute" = "volume") {
  const state = audioState(target, status);
  return !!state && (!("canSetVolume" in state) || (control === "volume" ? state.canSetVolume : state.canMute || state.emulated));
}

export function audioFader(fader: Fader, status: AudioStatus | null): Fader {
  if (!fader.bound) return fader;
  const state = audioState(fader.bound, status);
  return { ...fader, value: state && !state.muted ? fader.min + (fader.max - fader.min) * state.volume : fader.min };
}
