import { expect, test } from "bun:test";
import { fineTuneAppCatalogue, type FineTuneState, type FineTuneManagedApp, type AudioTarget } from "../src/lib/api";
import { fineTuneApp, fineTunePlayback, fineTuneReason, fineTuneRoutingReason } from "../src/lib/fineTune";
import { audioControllable, audioFader, audioFaderReadable } from "../src/lib/systemAudio";
import { isLinkActive } from "../src/lib/stateLink";
import { newFader } from "../src/lib/fader";

const app: FineTuneManagedApp = { appId: "com.player", name: "Player", pids: [], active: false, running: false, manageable: true, volume: .6, boost: 1, muted: false, routing: { requested: { mode: "followDefault" }, appliedDeviceUIDs: null, status: "unavailable", error: null } };
function state(): FineTuneState {
  return { status: "ready", connected: true, capabilities: ["managedApps"], error: null, appVersion: "A2", sessionId: "session", revision: 1, generation: 1, serverRevision: 1,
    snapshot: { apps: [{ ...app, active: true, pids: [99] }], managedApps: [{ ...app }], devices: [], defaultOutput: null, defaultInput: null, inputPolicy: { lockEnabled: false, preferredDeviceUID: null, lockedDeviceUID: null }, permission: { audioCapture: "authorized", source: "preflight" } } };
}
const target: AudioTarget = { type: "fineTune", target: { type: "app", appId: app.appId } };

test("catalogue is capability-gated and never fabricates saved or fallback rows", () => {
  const live = state();
  expect(fineTuneAppCatalogue(live)[0].pids).toEqual([]);
  live.capabilities = [];
  expect(fineTuneAppCatalogue(live)[0].pids).toEqual([99]);
  live.capabilities = undefined;
  expect(fineTuneApp(app.appId, live)?.active).toBe(true);
  live.capabilities = ["managedApps"]; live.snapshot!.managedApps = [];
  expect(fineTuneApp(app.appId, live)).toBeNull();
  expect(fineTuneReason(target.target, live)).toBe("missingApp");
  live.snapshot!.managedApps = undefined;
  expect(fineTuneAppCatalogue(live)).toEqual([]);
});

test("silent and remembered gain/mute are eligible, readable and never routeable", () => {
  for (const running of [true, false]) {
    const live = state(); const row = live.snapshot!.managedApps![0]; row.running = running; row.pids = running ? [123] : [];
    expect(fineTunePlayback(app.appId, live)).toBe(running ? "notPlaying" : "notRunning");
    expect(fineTuneReason(target.target, live)).toBeNull();
    for (const control of ["volume", "mute"] as const) expect(audioControllable(target, null, control, null, live)).toBe(true);
    expect(fineTuneRoutingReason(app.appId, live)).toBe("routingInactive");
    const fader = { ...newFader(0, 0, null), min: 0, max: 100, bound: target };
    expect(audioFader(fader, null, null, live).value).toBe(60);
    for (const permission of ["denied", "unknown", "authorized"] as const) {
      live.snapshot!.permission.audioCapture = permission; row.manageable = false; row.muted = true;
      expect(fineTuneReason(target.target, live)).toBe(permission === "authorized" ? "ignoredApp" : "capturePermission");
      expect(audioFaderReadable(target, null, null, live)).toBe(true);
      expect(audioFader(fader, null, null, live).value).toBe(0);
      expect(isLinkActive({ provider: "fineTune", kind: "audioMuted", scene: "", source: "", target }, null, null, null, null, live)).toBe(true);
      for (const control of ["volume", "mute"] as const) expect(audioControllable(target, null, control, null, live)).toBe(false);
    }
  }
});

test("ambiguity and missing block reads and writes; routing requires active singleton and preserves multi guard", () => {
  const live = state(); live.snapshot!.managedApps!.push({ ...app, running: true, pids: [123] });
  expect(fineTuneReason(target.target, live)).toBe("ambiguous");
  expect(fineTuneApp(app.appId, live)).toBeNull();
  expect(audioFaderReadable(target, null, null, live)).toBe(false);
  expect(audioControllable(target, null, "volume", null, live)).toBe(false);
  expect(fineTuneRoutingReason(app.appId, live)).toBe("ambiguous");
  expect(fineTuneRoutingReason("missing", live)).toBe("missingApp");
  live.snapshot!.managedApps = [{ ...app, active: true, running: true, pids: [123] }];
  expect(fineTuneRoutingReason(app.appId, live)).toBeNull();
  live.snapshot!.managedApps[0].pids = [];
  expect(fineTuneRoutingReason(app.appId, live)).toBe("routingInactive");
  live.snapshot!.managedApps[0].pids = [123];
  live.snapshot!.managedApps[0].routing = { ...app.routing, requested: { mode: "multi", deviceUIDs: ["a", "b"] } };
  expect(fineTuneRoutingReason(app.appId, live)).toBe("multiReadOnly");
  live.status = "disconnected";
  expect(fineTuneApp(app.appId, live)).toBeNull();
  expect(fineTuneReason(target.target, live)).toBe("notReady");
});
