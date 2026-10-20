import { expect, test } from "bun:test";
import { contract } from "./actions-contract";

test("Wave Link target metadata preserves its tag and all qualified fields", () => {
  const types = contract().types;
  expect(types.AudioTarget.variants?.find((variant) => variant.value === "waveLink")?.fields).toEqual([{ name: "target", type: "WaveLinkTarget" }]);
  expect(types.WaveLinkTarget.kind).toBe("union");
  expect(types.WaveLinkTarget.tag).toBe("type");
  expect(types.WaveLinkTarget.variants?.map((variant) => [variant.value, variant.fields.map((field) => field.name)])).toEqual([
    ["channel", ["channelId"]],
    ["channelMix", ["channelId", "mixId"]],
    ["mix", ["mixId"]],
    ["output", ["outputDeviceId", "outputId"]],
    ["input", ["inputDeviceId", "inputId"]],
    ["inputGain", ["inputDeviceId", "inputId"]],
  ]);
});

test("Wave Link routing action payloads use qualified output identities", () => {
  const data = contract();
  expect(data.payloads.waveLinkSetMainOutput.fields.map((field) => field.name)).toEqual(["outputDeviceId", "outputId"]);
  expect(data.payloads.waveLinkSetMonitoredMix.fields.map((field) => field.name)).toEqual(["outputDeviceId", "outputId", "mixId"]);
  expect(data.actions.waveLinkSetMainOutput.menu).toBe(true);
  expect(data.actions.waveLinkSetMonitoredMix.menu).toBe(true);
  expect(data.actions.waveLinkSetMainOutput.wait).toBe(true);
  expect(data.actions.waveLinkSetMonitoredMix.wait).toBe(true);
});
