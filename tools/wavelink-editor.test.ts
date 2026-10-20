import { expect, test } from "bun:test";
import type { Action, AudioTarget } from "../src/lib/api";
import { audioActionConfigured } from "../src/lib/systemAudio";

const setAudio = (target: AudioTarget): Action => ({ id: "audio", wait: true, type: "setAudio", target, mode: "mute", volume: 50, volumeFrom: null });

test("routing actions require every qualified identifier, not discovery or readiness", () => {
  const main: Action = { id: "main", wait: true, type: "waveLinkSetMainOutput", outputDeviceId: "offline-device", outputId: "same-child" };
  const mix: Action = { ...main, type: "waveLinkSetMonitoredMix", mixId: "offline-mix" };
  expect(audioActionConfigured(main)).toBe(true);
  expect(audioActionConfigured(mix)).toBe(true);
  for (const action of [main, mix]) {
    expect(audioActionConfigured({ ...action, outputDeviceId: "" })).toBe(false);
    expect(audioActionConfigured({ ...action, outputId: "" })).toBe(false);
  }
  expect(audioActionConfigured({ ...mix, mixId: "" })).toBe(false);
  // IDs are opaque: don't trim, normalize or replace an unavailable reference.
  const opaque: Action = { ...mix, outputDeviceId: "Case:/one", outputId: "Case:/two", mixId: "Case:/three" };
  const before = structuredClone(opaque);
  expect(audioActionConfigured(opaque)).toBe(true);
  expect(opaque).toEqual(before);
});

test("Wave Link SetAudio validates all target identity parts without rewriting saved actions", () => {
  const targets: AudioTarget[] = [
    { type: "waveLink", target: { type: "channel", channelId: "c" } },
    { type: "waveLink", target: { type: "channelMix", channelId: "c", mixId: "m" } },
    { type: "waveLink", target: { type: "mix", mixId: "m" } },
    { type: "waveLink", target: { type: "output", outputDeviceId: "d", outputId: "same" } },
    { type: "waveLink", target: { type: "input", inputDeviceId: "d", inputId: "same" } },
    { type: "waveLink", target: { type: "inputGain", inputDeviceId: "d", inputId: "same" } },
  ];
  for (const target of targets) {
    const action = setAudio(target); const before = structuredClone(action);
    expect(audioActionConfigured(action)).toBe(true);
    expect(action).toEqual(before);
  }
  expect(audioActionConfigured(setAudio({ type: "waveLink", target: { type: "channel", channelId: "" } }))).toBe(false);
  expect(audioActionConfigured(setAudio({ type: "waveLink", target: { type: "channelMix", channelId: "c", mixId: "" } }))).toBe(false);
  expect(audioActionConfigured(setAudio({ type: "waveLink", target: { type: "output", outputDeviceId: "", outputId: "same" } }))).toBe(false);
  expect(audioActionConfigured(setAudio({ type: "waveLink", target: { type: "input", inputDeviceId: "d", inputId: "" } }))).toBe(false);
});

test("inline nested control-flow bodies are validated, including else branches", () => {
  const actions: Action[] = [
    { id: "outer", wait: true, type: "ifStart", variable: "x", op: "equals", value: "1", elseId: "else", endId: "end" },
    { id: "inner", wait: true, type: "ifStart", variable: "y", op: "equals", value: "2", elseId: "inner-else", endId: "inner-end" },
    { id: "inner-else", wait: true, type: "ifElse", startId: "inner", endId: "inner-end" },
    { id: "route", wait: true, type: "waveLinkSetMonitoredMix", outputDeviceId: "d", outputId: "same", mixId: "" },
    { id: "inner-end", wait: true, type: "ifEnd", startId: "inner", elseId: "inner-else" },
    { id: "else", wait: true, type: "ifElse", startId: "outer", endId: "end" },
    { id: "end", wait: true, type: "ifEnd", startId: "outer", elseId: "else" },
  ];
  expect(actions.every(audioActionConfigured)).toBe(false);
  actions[3] = { id: "route", wait: true, type: "waveLinkSetMonitoredMix", outputDeviceId: "d", outputId: "same", mixId: "offline" };
  expect(actions.every(audioActionConfigured)).toBe(true);
});
