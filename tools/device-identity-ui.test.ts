import { expect, test } from "bun:test";
import type { AudioDevice, AudioDeviceKind, AudioStatus, AudioTarget, FineTuneState } from "../src/lib/api";
import { savedDeviceMatch } from "../src/lib/systemAudio";

const APPLE_OUT = "AppleUSBAudioEngine:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1";
const LOGI_OUT = "LogiGamingAudio:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1";
const LOGI_IN = "LogiGamingAudio:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:2";

const device = (id: string, name: string, kind: AudioDeviceKind, present = true): AudioDevice => ({ id, name, kind, volume: .4, muted: false, isDefault: false, canSetVolume: true, canMute: true, emulated: false, present });
const status = (devices: AudioDevice[]): AudioStatus => ({ available: true, appsAvailable: false, canSetAppVolume: false, canMuteApps: false, permission: "granted", message: null, devices, apps: [] });
const named = (id: string, label: string, kind: AudioDeviceKind = "output"): AudioTarget => ({ type: "device", device: { type: "named", id, kind, label } });

test("native named devices: exact, driver, label and unresolved, with an update to the live id and name", () => {
  const live = status([device(LOGI_OUT, "PRO X 2 LIGHTSPEED", "output"), device(LOGI_IN, "PRO X 2 LIGHTSPEED", "input"), device("new-usb", "USB Mic", "input"), device(APPLE_OUT, "PRO X 2", "output", false)]);
  expect(savedDeviceMatch(named(LOGI_OUT, "PRO X 2 LIGHTSPEED"), live)?.match).toBe("exact");
  expect(savedDeviceMatch(named(APPLE_OUT, "PRO X 2"), live)).toEqual({ match: "driver", name: "PRO X 2 LIGHTSPEED", id: LOGI_OUT, update: named(LOGI_OUT, "PRO X 2 LIGHTSPEED") });
  // The not-connected phantom for the saved id is never a match candidate.
  expect(savedDeviceMatch(named("old-usb", " usb mic ", "input"), live)).toEqual({ match: "label", name: "USB Mic", id: "new-usb", update: named("new-usb", "USB Mic", "input") });
  expect(savedDeviceMatch(named("gone", "Other", "input"), live)).toBeNull();
  expect(savedDeviceMatch(named(APPLE_OUT, "PRO X 2"), { ...live, available: false })).toBeNull();
  expect(savedDeviceMatch(named(APPLE_OUT, "PRO X 2"), null)).toBeNull();
  // Name-addressed refs (empty id), defaults, apps and groups have no saved id to update.
  expect(savedDeviceMatch(named("", "USB Mic", "input"), live)).toBeNull();
  expect(savedDeviceMatch({ type: "device", device: { type: "defaultOutput" } }, live)).toBeNull();
  expect(savedDeviceMatch({ type: "group", groupId: "g" }, live)).toBeNull();
});

test("FineTune output/input targets: driver and label matches update uid and label", () => {
  const fine = (uid: string, name: string, direction: AudioDeviceKind) => ({ uid, name, direction, isDefault: false, volume: null, muted: null, volumeSource: null, capabilities: { volumeReadable: false, muteReadable: false, selectable: true } });
  const state: FineTuneState = { status: "ready", connected: true, error: null, appVersion: "1.0", sessionId: "s", revision: 1, generation: 1, serverRevision: 1,
    snapshot: { apps: [], devices: [fine(LOGI_OUT, "PRO X 2 LIGHTSPEED", "output"), fine("mic-2", "Desk Mic", "input")], defaultOutput: LOGI_OUT, defaultInput: null,
      inputPolicy: { lockEnabled: false, preferredDeviceUID: null, lockedDeviceUID: null }, permission: { audioCapture: "authorized", source: "preflight" } } };
  const target = (type: "output" | "input", deviceUid: string, label?: string): AudioTarget => ({ type: "fineTune", target: { type, deviceUid, ...(label ? { label } : {}) } });
  expect(savedDeviceMatch(target("output", APPLE_OUT), null, state)).toEqual({ match: "driver", name: "PRO X 2 LIGHTSPEED", id: LOGI_OUT, update: target("output", LOGI_OUT, "PRO X 2 LIGHTSPEED") });
  expect(savedDeviceMatch(target("input", "mic-1", "desk mic"), null, state)).toEqual({ match: "label", name: "Desk Mic", id: "mic-2", update: target("input", "mic-2", "Desk Mic") });
  expect(savedDeviceMatch(target("input", "mic-1"), null, state)).toBeNull();
  expect(savedDeviceMatch(target("output", APPLE_OUT), null, { ...state, status: "disconnected" } as FineTuneState)).toBeNull();
  expect(savedDeviceMatch({ type: "fineTune", target: { type: "app", appId: "x" } }, null, state)).toBeNull();
});
