import { expect, test } from "bun:test";
import type { AudioGroupReport, AudioStatus, Fader } from "../src/lib/api";
import { DEFAULT_MUTED_COLOR, faderMutedColor, faderMutedLevel, faderPadRgb, newFader } from "../src/lib/fader";
import { aggregateGroup, audioFader, audioFaderView } from "../src/lib/systemAudio";

const device = { type: "device" as const, device: { type: "defaultInput" as const } };
const status = (volume: number, muted: boolean, canSetVolume = true): AudioStatus => ({
  available: true, appsAvailable: false, canSetAppVolume: false, canMuteApps: false, permission: "granted", message: null, apps: [],
  devices: [{ id: "mic", name: "Mic", kind: "input", isDefault: true, present: true, volume, muted, canSetVolume, canMute: true, emulated: false }],
});
const fader = (patch: Partial<Fader> = {}): Fader => ({ ...newFader(0, 0, null), length: 5, min: -60, max: 20, value: 0, bound: device, ...patch });

test("old faders without the new fields normalize to off and red; newFader writes the defaults", () => {
  const old = fader();
  delete old.mutedLevel; delete old.mutedColor;
  expect(faderMutedLevel(old)).toBe(false);
  expect(faderMutedColor(old)).toEqual([255, 0, 0]);
  const created = newFader(0, 0, null);
  expect(created.mutedLevel).toBe(false);
  expect(created.mutedColor).toEqual([255, 0, 0]);
  created.mutedColor![0] = 1;
  expect(DEFAULT_MUTED_COLOR).toEqual([255, 0, 0]);
  // Missing fields keep the existing dim-to-zero look exactly.
  const view = audioFaderView(old, status(0.25, true));
  expect(view.mutedColor).toBeNull();
  expect(view.fader.value).toBe(-60);
});

test("faderPadRgb colour override: solid up to level, existing dim percentage above, gradient otherwise unchanged", () => {
  const f = fader({ value: 0, dim: 12 }); // -60..20, 5 pads: level step round(0.75*4)=3
  expect([0, 1, 2, 3, 4].map((step) => faderPadRgb(f, step, [200, 40, 100]))).toEqual([[200, 40, 100], [200, 40, 100], [200, 40, 100], [200, 40, 100], [24, 5, 12]]);
  expect(faderPadRgb(f, 4, null)).toEqual(faderPadRgb(f, 4));
  expect(faderPadRgb({ ...f, dim: 250 }, 4, [10, 20, 30])).toEqual([10, 20, 30]);
});

test("single target: keep-level mode retains stored value only while confirmed muted and readable", () => {
  const keep = fader({ mutedLevel: true, mutedColor: [120, 40, 80] });
  // Muted with a different reported level: stored value and the mute colour.
  expect(audioFaderView(keep, status(0.9, true))).toEqual({ fader: keep, mutedColor: [120, 40, 80] });
  expect(audioFader(keep, status(0.9, true)).value).toBe(0);
  // Mode off: existing dim to the minimum.
  expect(audioFaderView({ ...keep, mutedLevel: false }, status(0.9, true))).toEqual({ fader: { ...keep, mutedLevel: false, value: -60 }, mutedColor: null });
  // Unmuted: follows the observed level, normal gradient.
  expect(audioFaderView(keep, status(0.25, false))).toEqual({ fader: { ...keep, value: -40 }, mutedColor: null });
  // Unavailable / not readable: unchanged (minimum, no mute look).
  expect(audioFaderView(keep, status(0.25, true, false)).mutedColor).toBeNull();
  expect(audioFaderView(keep, null)).toEqual({ fader: { ...keep, value: -60 }, mutedColor: null });
  // Unbound: unchanged.
  const free = { ...keep, bound: null };
  expect(audioFaderView(free, status(0.25, true))).toEqual({ fader: free, mutedColor: null });
});

test("groups: only all-known-muted with a readable level uses the retained look; partial/unknown/unreadable unchanged", () => {
  const bound = { type: "group" as const, groupId: "comms" };
  const keep = fader({ bound, mutedLevel: true });
  const report = (members: { volume: number | null; muted: boolean | null }[]): AudioGroupReport[] => {
    const full = members.map((m, i) => ({ ...m, target: { type: "app" as const, app: { id: String(i), name: "" } }, canSetVolume: true, canMute: true, reason: null }));
    return [{ groupId: "comms", revision: 1, members: full, summary: aggregateGroup(full), restoreOwned: false, busy: false, operation: null }];
  };
  expect(audioFaderView(keep, null, null, null, report([{ volume: 0.5, muted: true }, { volume: 0.2, muted: true }]))).toEqual({ fader: keep, mutedColor: [255, 0, 0] });
  // Default mode still shows the group zero override.
  expect(audioFaderView({ ...keep, mutedLevel: false }, null, null, null, report([{ volume: 0.5, muted: true }, { volume: 0.2, muted: true }])).fader.value).toBe(-60);
  // Partial mute: ordinary observed mean.
  expect(audioFaderView(keep, null, null, null, report([{ volume: 0.5, muted: true }, { volume: 1, muted: false }]))).toEqual({ fader: { ...keep, value: 0 }, mutedColor: null });
  // Unknown mute member: not "all".
  expect(audioFaderView(keep, null, null, null, report([{ volume: 0.5, muted: true }, { volume: 0.5, muted: null }])).mutedColor).toBeNull();
  // All muted but nothing readable: unavailable, no look.
  expect(audioFaderView(keep, null, null, null, report([{ volume: null, muted: true }])).mutedColor).toBeNull();
  // No report at all.
  expect(audioFaderView(keep, null, null, null, []).mutedColor).toBeNull();
});
