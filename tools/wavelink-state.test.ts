import { afterEach, expect, test } from "bun:test";
import { api, events, type AudioTarget, type WaveLinkState } from "../src/lib/api";
import { useMediaStore } from "../src/store/media";
import { audioControllable, audioFader } from "../src/lib/systemAudio";
import { waveLinkKey } from "../src/lib/waveLink";
import { isLinkActive } from "../src/lib/stateLink";
import { muteButton } from "../src/lib/buttonPresets";
import { newFader } from "../src/lib/fader";

const originalApi = { ...api }; const originalEvents = { ...events };
afterEach(() => { Object.assign(api, originalApi); Object.assign(events, originalEvents); });
function state(revision = 1): WaveLinkState {
  return { status: "ready", connected: true, error: null, port: 1884, application: { name: "Wave Link", appID: "EWL", operatingSystem: "mac", interfaceRevision: 2 }, version: null, generation: 1, revision,
    controls: [{ target: { type: "channelMix", channelId: "c", mixId: "m" }, name: "Music / Monitor", level: .75, muted: false, canSetLevel: true, canMute: true, gain: null, reason: null }], channels: [{ id: "c", name: "Music", apps: [] }], mixes: [{ id: "m", name: "Monitor" }], outputs: [
      { outputDeviceId: "a", outputId: "same", name: "A", mixId: "", canSelectMain: true, canSetMix: true },
      { outputDeviceId: "b", outputId: "same", name: "B", mixId: null, canSelectMain: true, canSetMix: false },
    ], mainOutput: { outputDeviceId: "a", outputId: "same" } };
}
const target: AudioTarget = { type: "waveLink", target: { type: "channelMix", channelId: "c", mixId: "m" } };

test("qualified identity separates parents, kinds, opaque delimiters and discovered pairs", () => {
  const keys = [
    waveLinkKey({ type: "input", inputDeviceId: "a", inputId: "same" }), waveLinkKey({ type: "input", inputDeviceId: "b", inputId: "same" }),
    waveLinkKey({ type: "output", outputDeviceId: "a", outputId: "same" }), waveLinkKey({ type: "inputGain", inputDeviceId: "a", inputId: "same" }),
    waveLinkKey({ type: "channelMix", channelId: "a:b", mixId: "c" }), waveLinkKey({ type: "channelMix", channelId: "a", mixId: "b:c" }),
  ];
  expect(new Set(keys).size).toBe(keys.length);
  expect(audioControllable({ type: "waveLink", target: { type: "channelMix", channelId: "c", mixId: "absent" } }, null, "volume", state())).toBe(false);
});

test("fader and mute feedback uses authoritative capabilities, nullable levels and readiness", () => {
  const live = state(); const fader = { ...newFader(0, 0, null), min: 0, max: 100, bound: target };
  expect(audioFader(fader, null, live).value).toBe(75);
  live.controls[0].muted = true;
  expect(audioFader(fader, null, live).value).toBe(0);
  const link = { provider: "waveLink" as const, kind: "audioMuted" as const, scene: "", source: "", target };
  expect(isLinkActive(link, null, null, null, live)).toBe(true);
  live.controls[0].canSetLevel = false;
  expect(audioControllable(target, null, "volume", live)).toBe(false);
  expect(audioControllable(target, null, "mute", live)).toBe(true);
  live.controls[0].muted = null;
  expect(isLinkActive(link, null, null, null, live)).toBe(false);
  live.status = "disconnected";
  expect(audioControllable(target, null, "mute", live)).toBe(false);
});

test("routing links compare qualified output and nonempty mix; gain has no writable mapping", () => {
  const live = state();
  const link = { provider: "waveLink" as const, kind: "waveLinkMainOutput" as const, scene: "", source: "", target: { type: "waveLink" as const, target: { type: "output" as const, outputDeviceId: "b", outputId: "same" } } };
  expect(isLinkActive(link, null, null, null, live)).toBe(false);
  link.target.target.outputDeviceId = "a";
  expect(isLinkActive(link, null, null, null, live)).toBe(true);
  expect(isLinkActive({ ...link, kind: "waveLinkMonitoredMix", waveLinkMixId: "" }, null, null, null, live)).toBe(false);
  live.outputs[0].mixId = "m";
  expect(isLinkActive({ ...link, kind: "waveLinkMonitoredMix", waveLinkMixId: "m" }, null, null, null, live)).toBe(true);
  const gainTarget: AudioTarget = { type: "waveLink", target: { type: "inputGain", inputDeviceId: "a", inputId: "same" } };
  live.controls.push({ target: gainTarget.target, name: "Raw gain", level: null, muted: null, canSetLevel: false, canMute: false, gain: { value: .21666667, min: 20, max: 50, unit: "decibel" }, reason: "Unvalidated mapping" });
  expect(audioControllable(gainTarget, null, "volume", live)).toBe(false);
  expect(audioControllable(gainTarget, null, "mute", live)).toBe(false);
  expect(live.controls[1].gain?.value).toBe(.21666667);
});

test("custom Wave Link mute preset retains exact canonical pair and provider", () => {
  const button = muteButton({ target, caption: "MUSIC", color: { mode: "palette", index: 12 }, mutedColor: { mode: "palette", index: 5 } });
  expect(button.down).toHaveLength(1); expect(button.up).toHaveLength(1); expect(button.hold).toEqual([]);
  expect(button.runMode).toBe("parallel"); expect(button.loop).toBe(false); expect(button.holdWait).toBe(false); expect(button.holdMs).toBe(500);
  for (const action of [button.down[0], button.up[0]]) expect(action).toEqual({ id: action.id, wait: true, type: "setAudio", target, mode: "holdMute", volume: 50, volumeFrom: null });
  expect(button.down[0].id).not.toBe(button.up[0].id);
  expect(button.stateLink).toEqual({ provider: "waveLink", kind: "audioMuted", scene: "", source: "", target });
});

test("subscribe precedes initial read; late initial and equal/older snapshots cannot replace newer event", async () => {
  let listener: (value: WaveLinkState) => void = () => {};
  let resolve!: (value: WaveLinkState) => void;
  const initial = new Promise<WaveLinkState>((yes) => { resolve = yes; });
  const calls: string[] = [];
  events.onWaveLinkState = async (cb) => { calls.push("subscribe"); listener = cb; return () => {}; };
  events.onAudioGroups = async () => () => {};
  api.getAudioGroups = async () => [];
  api.waveLinkState = () => { calls.push("read"); return initial; };
  events.onFineTuneState = async () => () => {};
  api.fineTuneState = async () => { throw new Error("fixture"); };
  events.onObsState = events.onSlobsState = events.onHomeAssistantState = events.onSystemAudio = async () => () => {};
  api.obsState = async () => { throw new Error("fixture"); }; api.slobsState = api.obsState;
  api.homeAssistantState = async () => { throw new Error("fixture"); }; api.audioStatus = async () => { throw new Error("fixture"); };
  useMediaStore.setState({ waveLink: null });
  const dispose = await useMediaStore.getState().init();
  expect(calls).toEqual(["subscribe", "read"]);
  listener(state(8)); resolve(state(2)); await initial; await Promise.resolve();
  expect(useMediaStore.getState().waveLink?.revision).toBe(8);
  listener({ ...state(8), status: "error" }); listener(state(7));
  expect(useMediaStore.getState().waveLink?.status).toBe("ready");
  listener({ ...state(9), status: "disconnected" });
  expect(useMediaStore.getState().waveLink?.status).toBe("disconnected"); dispose();
});
