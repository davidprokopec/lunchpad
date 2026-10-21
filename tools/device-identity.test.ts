import { expect, test } from "bun:test";
import fixtures from "./fixtures/device-identity.json";
import type { AudioDevice, AudioDeviceKind, AudioStatus, FineTuneState, StateLink } from "../src/lib/api";
import { resolveDevice } from "../src/lib/deviceIdentity";
import { fineTuneDevice, fineTuneReason } from "../src/lib/fineTune";
import { isLinkActive } from "../src/lib/stateLink";
import { audioState } from "../src/lib/systemAudio";

const APPLE_OUT = "AppleUSBAudioEngine:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1";
const APPLE_IN = "AppleUSBAudioEngine:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:2";
const LOGI_OUT = "LogiGamingAudio:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1";
const LOGI_IN = "LogiGamingAudio:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:2";

test("UI resolver agrees with the Rust shared fixtures", () => {
  expect(fixtures.length).toBeGreaterThanOrEqual(8);
  for (const fixture of fixtures) {
    const live = fixture.live.map((d) => ({ ...d, kind: d.kind as AudioDeviceKind }));
    const got = resolveDevice(fixture.saved.id, fixture.saved.label, fixture.saved.kind as AudioDeviceKind, live);
    expect(got && { id: got.device.id, match: got.match }, fixture.name).toEqual(fixture.expected);
  }
});

function swapped(): FineTuneState {
  const device = (uid: string, direction: "input" | "output") => ({ uid, name: "PRO X 2", direction, isDefault: true, volume: null, muted: null, volumeSource: null, capabilities: { volumeReadable: false, muteReadable: false, selectable: true } });
  return { status: "ready", connected: true, error: null, appVersion: "1.0", sessionId: "s", revision: 1, generation: 1, serverRevision: 1,
    snapshot: { apps: [], devices: [device(LOGI_OUT, "output"), device(LOGI_IN, "input")], defaultOutput: LOGI_OUT, defaultInput: LOGI_IN,
      inputPolicy: { lockEnabled: false, preferredDeviceUID: LOGI_IN, lockedDeviceUID: null }, permission: { audioCapture: "authorized", source: "preflight" } } };
}

test("FineTune targets saved under another driver are matched, lit and never rewritten", () => {
  const live = swapped();
  const output = { type: "output" as const, deviceUid: APPLE_OUT };
  expect(fineTuneReason(output, live)).toBeNull();
  expect(fineTuneDevice(output, live)).toMatchObject({ device: { uid: LOGI_OUT }, match: "driver" });
  expect(fineTuneDevice({ type: "output", deviceUid: "gone", label: "pro x 2" }, live)?.match).toBe("label");
  expect(fineTuneReason({ type: "output", deviceUid: APPLE_IN }, live)).toBe("missingDevice");
  const link = (kind: StateLink["kind"], target: { type: "output" | "input"; deviceUid: string }): StateLink => ({ provider: "fineTune", kind, scene: "", source: "", target: { type: "fineTune", target } });
  const links = [link("fineTuneDefaultOutput", output), link("fineTunePreferredInput", { type: "input", deviceUid: APPLE_IN }), link("fineTuneDefaultInput", { type: "input", deviceUid: APPLE_IN })];
  for (const l of links) expect(isLinkActive(l, null, null, null, null, live)).toBe(true);
  expect(output.deviceUid).toBe(APPLE_OUT);
  live.snapshot!.devices.push({ ...live.snapshot!.devices[0], uid: "OtherDriver:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1", isDefault: false });
  expect(fineTuneReason(output, live)).toBe("missingDevice");
  expect(isLinkActive(links[0], null, null, null, null, live)).toBe(false);
});

test("native named devices resolve across a driver swap; the saved phantom stays listed", () => {
  const device = (id: string, kind: AudioDeviceKind, present = true): AudioDevice => ({ id, name: "PRO X 2", kind, volume: .4, muted: false, isDefault: true, canSetVolume: true, canMute: true, emulated: false, present });
  const status: AudioStatus = { available: true, appsAvailable: false, canSetAppVolume: false, canMuteApps: false, permission: "granted", message: null, devices: [device(LOGI_OUT, "output"), device(LOGI_IN, "input"), device(APPLE_OUT, "output", false)], apps: [] };
  expect(audioState({ type: "device", device: { type: "named", id: APPLE_OUT, kind: "output", label: "" } }, status)).toMatchObject({ id: LOGI_OUT });
  expect(audioState({ type: "device", device: { type: "named", id: APPLE_IN, kind: "input", label: "" } }, status)).toMatchObject({ id: LOGI_IN });
  expect(audioState({ type: "device", device: { type: "named", id: APPLE_IN, kind: "output", label: "" } }, status)).toBeNull();
  // Empty id keeps the existing name lookup.
  expect(audioState({ type: "device", device: { type: "named", id: "", kind: "input", label: "pro x 2" } }, status)).toMatchObject({ id: LOGI_IN });
});
