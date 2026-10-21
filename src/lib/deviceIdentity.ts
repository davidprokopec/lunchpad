import type { AudioDeviceKind } from "./api";

/**
 * Which live device a saved device id means. Mirrors `resolve_device` in
 * src-tauri/src/profile/device_identity.rs (shared fixtures:
 * tools/fixtures/device-identity.json). The saved id is never rewritten.
 *
 * - "exact": the saved id itself
 * - "driver": same CoreAudio UID under another driver prefix
 *   (`AppleUSBAudioEngine:…:1` ↔ `LogiGamingAudio:…:1`)
 * - "label": the only device in that direction with the saved name
 */
export type DeviceMatch = "exact" | "driver" | "label";

/** Rust `eq_ignore_ascii_case`: only ASCII letters fold. */
const fold = (value: string) => value.trim().replace(/[A-Z]/g, (c) => c.toLowerCase());

export interface LiveDeviceIdentity { id: string; name: string; kind: AudioDeviceKind }

/** `driver:vendor:product:serial:index` (≥4 segments) only; never Windows `{…}.{guid}` ids. */
function driverless(id: string): string | null {
  if (id.startsWith("{")) return null;
  const parts = id.split(":");
  if (parts.length < 4) return null;
  const rest = parts.slice(1).join(":");
  return parts[0].trim() && rest.trim() ? rest : null;
}

export function resolveDevice<T extends LiveDeviceIdentity>(savedId: string, label: string | null | undefined, kind: AudioDeviceKind, live: readonly T[]): { device: T; match: DeviceMatch } | null {
  const candidates = live.filter((d) => d.kind === kind);
  const exact = candidates.find((d) => d.id === savedId);
  if (exact) return { device: exact, match: "exact" };
  const rest = driverless(savedId);
  if (rest !== null) {
    const same = candidates.filter((d) => driverless(d.id) === rest);
    if (same.length) return same.length === 1 ? { device: same[0], match: "driver" } : null;
  }
  const name = label ? fold(label) : "";
  if (!name) return null;
  const named = candidates.filter((d) => fold(d.name) === name);
  return named.length === 1 ? { device: named[0], match: "label" } : null;
}
