import { afterEach, expect, test } from "bun:test";
import { api, events, type AudioTarget, type FineTuneState, type StateLink } from "../src/lib/api";
import { useMediaStore } from "../src/store/media";
import { audioActionConfigured, audioControllable, audioFader, audioFaderReadable, audioState, audioTargetConfigured } from "../src/lib/systemAudio";
import { fineTuneApp, fineTuneReason } from "../src/lib/fineTune";
import { isLinkActive } from "../src/lib/stateLink";
import { muteButton } from "../src/lib/buttonPresets";
import { newFader } from "../src/lib/fader";

const originalApi = { ...api }; const originalEvents = { ...events };
afterEach(() => { Object.assign(api, originalApi); Object.assign(events, originalEvents); });
function state(revision = 1): FineTuneState {
  return { status: "ready", connected: true, error: null, appVersion: "1.0", sessionId: "session", revision, generation: 1, serverRevision: 1,
    snapshot: { apps: [{ appId: "com.player", pids: [123], name: "Player", active: true, volume: .75, boost: 2, muted: false, routing: { requested: { mode: "followDefault" }, appliedDeviceUIDs: ["speaker"], status: "applied", error: null } }],
      devices: ["output", "input"].map((direction) => ({ uid: "duplex", name: "Duplex", direction: direction as "input" | "output", isDefault: true, volume: null, muted: null, volumeSource: null, capabilities: { volumeReadable: false, muteReadable: false, selectable: true } })),
      defaultOutput: "duplex", defaultInput: "duplex", inputPolicy: { lockEnabled: false, preferredDeviceUID: "duplex", lockedDeviceUID: null }, permission: { audioCapture: "authorized", source: "preflight" } } };
}
const target: AudioTarget = { type: "fineTune", target: { type: "app", appId: "com.player" } };
const link: StateLink = { provider: "fineTune", kind: "audioMuted", scene: "", source: "", target };

test("FineTune app resolution rejects duplicates, missing/inactive apps and capture denial without PID persistence", () => {
  const live = state();
  expect(fineTuneApp("com.player", live)?.pids).toEqual([123]);
  expect(fineTuneReason({ type: "app", appId: "absent" }, live)).toBe("notRunning");
  live.snapshot!.apps.push({ ...live.snapshot!.apps[0], pids: [456] });
  expect(fineTuneApp("com.player", live)).toBeNull();
  expect(fineTuneReason(target.target, live)).toBe("ambiguous");
  expect(audioControllable(target, null, "mute", null, live)).toBe(false);
  live.snapshot!.apps.pop(); live.snapshot!.apps[0].active = false;
  expect(fineTuneReason(target.target, live)).toBe("notRunning");
  live.snapshot!.apps[0].active = true; live.snapshot!.permission.audioCapture = "denied";
  expect(fineTuneReason(target.target, live)).toBe("capturePermission");
  expect(isLinkActive(link, null, null, null, null, live)).toBe(false);
});

test("FineTune live feedback uses managed gain, not boost; muted/unavailable renders minimum", () => {
  const live = state(); const fader = { ...newFader(0, 0, null), min: 10, max: 110, bound: target };
  expect(audioFader(fader, null, null, live).value).toBe(85);
  live.snapshot!.apps[0].muted = true;
  expect(audioFader(fader, null, null, live).value).toBe(10);
  expect(isLinkActive(link, null, null, null, null, live)).toBe(true);
  live.status = "disconnected";
  expect(audioFader(fader, null, null, live).value).toBe(10);
  expect(isLinkActive(link, null, null, null, null, live)).toBe(false);
});

test("FineTune reported mute and fader state remain readable without capture write permission", () => {
  const live = state(); const fader = { ...newFader(0, 0, null), min: 10, max: 110, bound: target };
  for (const permission of ["denied", "unknown"] as const) {
    live.snapshot!.permission.audioCapture = permission;
    live.snapshot!.apps[0].muted = true;
    expect(isLinkActive(link, null, null, null, null, live)).toBe(true);
    expect(audioState(target, null, null, live)?.muted).toBe(true);
    expect(audioFaderReadable(target, null, null, live)).toBe(true);
    expect(audioFader(fader, null, null, live).value).toBe(10);
    for (const control of ["volume", "mute"] as const) expect(audioControllable(target, null, control, null, live)).toBe(false);
    live.snapshot!.apps[0].muted = false;
    expect(isLinkActive(link, null, null, null, null, live)).toBe(false);
    expect(audioFader(fader, null, null, live).value).toBe(85);
  }
  live.snapshot!.apps[0].muted = true;
  const app = live.snapshot!.apps[0];
  for (const apps of [[], [{ ...app, active: false }], [app, { ...app, pids: [456] }]]) {
    live.snapshot!.apps = apps;
    expect(isLinkActive(link, null, null, null, null, live)).toBe(false);
    expect(audioState(target, null, null, live)).toBeNull();
    expect(audioFaderReadable(target, null, null, live)).toBe(false);
    expect(audioControllable(target, null, "mute", null, live)).toBe(false);
  }
  live.snapshot!.apps = [app]; live.status = "disconnected";
  expect(isLinkActive(link, null, null, null, null, live)).toBe(false);
  expect(audioState(target, null, null, live)).toBeNull();
  expect(audioFaderReadable(target, null, null, live)).toBe(false);
  expect(audioControllable(target, null, "volume", null, live)).toBe(false);
});

test("FineTune device links use actual state and direction, independently of capture and device level capability", () => {
  const live = state(); live.snapshot!.permission.audioCapture = "denied";
  const deviceLink = (kind: StateLink["kind"], direction: "input" | "output"): StateLink => ({ ...link, kind, target: { type: "fineTune", target: { type: direction, deviceUid: "duplex" } } });
  for (const [kind, direction] of [["fineTuneDefaultOutput", "output"], ["fineTunePreferredInput", "input"], ["fineTuneDefaultInput", "input"]] as const) {
    expect(isLinkActive(deviceLink(kind, direction), null, null, null, null, live)).toBe(true);
    expect(audioControllable(deviceLink(kind, direction).target!, null, "volume", null, live)).toBe(false);
  }
  expect(isLinkActive(deviceLink("fineTuneDefaultOutput", "input"), null, null, null, null, live)).toBe(false);
  live.snapshot!.devices = live.snapshot!.devices.filter((d) => d.direction !== "output");
  expect(isLinkActive(deviceLink("fineTuneDefaultOutput", "output"), null, null, null, null, live)).toBe(false);
});

test("FineTune mute preset has exact canonical JSON; structural guards preserve offline nonempty references", () => {
  const button = muteButton({ target, caption: "PLAYER", color: { mode: "palette", index: 12 }, mutedColor: { mode: "palette", index: 5 } });
  expect(button.down).toHaveLength(1); expect(button.up).toHaveLength(1); expect(button.hold).toEqual([]);
  expect(button.runMode).toBe("parallel"); expect(button.loop).toBe(false); expect(button.holdWait).toBe(false); expect(button.holdMs).toBe(500);
  for (const action of [button.down[0], button.up[0]]) expect(action).toEqual({ id: action.id, wait: true, type: "setAudio", target, mode: "holdMute", volume: 50, volumeFrom: null });
  expect(button.down[0].id).not.toBe(button.up[0].id); expect(button.stateLink).toEqual(link);
  expect(audioTargetConfigured(target)).toBe(true);
  expect(audioTargetConfigured({ type: "fineTune", target: { type: "app", appId: "" } })).toBe(false);
  const route = { id: "route", wait: true, type: "fineTuneSetAppRouting" as const, appId: "com.player", routing: { mode: "followDefault" as const } };
  expect(audioActionConfigured(route)).toBe(true);
  expect(audioActionConfigured({ ...route, appId: "" })).toBe(false);
  expect(audioActionConfigured({ ...route, routing: { mode: "single", deviceUID: "" } })).toBe(false);
  expect(audioActionConfigured({ ...route, routing: { mode: "single", deviceUID: "offline-uid" } })).toBe(true);
  for (const type of ["fineTuneSetDefaultOutput", "fineTuneSetPreferredInput"] as const) {
    expect(audioActionConfigured({ id: "device", wait: true, type, deviceUid: "" })).toBe(false);
    expect(audioActionConfigured({ id: "device", wait: true, type, deviceUid: "offline-uid" })).toBe(true);
  }
  for (const type of ["input", "output"] as const) {
    expect(audioActionConfigured({ ...button.down[0], type: "setAudio", target: { type: "fineTune", target: { type, deviceUid: "duplex" } }, mode: "mute", volume: 50, volumeFrom: null })).toBe(false);
  }
});

test("FineTune subscribes before read and only admits newer local receipts, not server revisions", async () => {
  let listener: (value: FineTuneState) => void = () => {};
  let resolve!: (value: FineTuneState) => void;
  const initial = new Promise<FineTuneState>((yes) => { resolve = yes; }); const calls: string[] = [];
  events.onFineTuneState = async (cb) => { calls.push("subscribe"); listener = cb; return () => {}; };
  api.fineTuneState = () => { calls.push("read"); return initial; };
  events.onObsState = events.onSlobsState = events.onHomeAssistantState = events.onSystemAudio = events.onWaveLinkState = async () => () => {};
  api.obsState = async () => { throw new Error("fixture"); }; api.slobsState = api.obsState;
  api.homeAssistantState = async () => { throw new Error("fixture"); }; api.audioStatus = async () => { throw new Error("fixture"); }; api.waveLinkState = async () => { throw new Error("fixture"); };
  useMediaStore.setState({ fineTune: null }); const dispose = await useMediaStore.getState().init();
  expect(calls).toEqual(["subscribe", "read"]);
  listener(state(8)); resolve(state(2)); await initial; await Promise.resolve();
  expect(useMediaStore.getState().fineTune?.revision).toBe(8);
  listener({ ...state(8), status: "error", serverRevision: 99 }); listener(state(7));
  expect(useMediaStore.getState().fineTune?.status).toBe("ready");
  listener({ ...state(9), status: "disconnected", snapshot: null });
  expect(useMediaStore.getState().fineTune?.snapshot).toBeNull();
  listener({ ...state(10), sessionId: "new", generation: 2, serverRevision: 0 });
  expect(useMediaStore.getState().fineTune?.sessionId).toBe("new"); dispose();
});
