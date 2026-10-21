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

test("FineTune target and routing metadata preserve frozen UID spelling", () => {
  const data = contract();
  expect(data.types.AudioTarget.variants?.find((variant) => variant.value === "fineTune")?.fields).toEqual([{ name: "target", type: "FineTuneTarget" }]);
  expect(data.types.FineTuneTarget.tag).toBe("type");
  expect(data.types.FineTuneTarget.variants?.map((variant) => [variant.value, variant.fields.map((field) => field.name)])).toEqual([
    ["app", ["appId"]], ["output", ["deviceUid", "label"]], ["input", ["deviceUid", "label"]],
  ]);
  // The fallback label is optional, so profiles without it stay valid.
  for (const variant of data.types.FineTuneTarget.variants?.filter((v) => v.value !== "app") ?? []) expect(variant.fields.find((field) => field.name === "label")).toMatchObject({ type: "text", nullable: true, optional: true });
  for (const type of ["fineTuneSetDefaultOutput", "fineTuneSetPreferredInput"]) {
    expect(data.payloads[type].fields.map((field) => field.name)).toEqual(["deviceUid", "label"]);
    expect(data.payloads[type].fields[1]).toMatchObject({ nullable: true, optional: true });
  }
  expect(data.types.FineTuneRoutingRequest.tag).toBe("mode");
  expect(data.types.FineTuneRoutingRequest.variants?.map((variant) => [variant.value, variant.fields.map((field) => field.name)])).toEqual([
    ["followDefault", []], ["single", ["deviceUID"]],
  ]);
  expect(data.payloads.fineTuneSetAppRouting.fields.map((field) => [field.name, field.type])).toEqual([["appId", "text"], ["routing", "FineTuneRoutingRequest"]]);
  for (const type of ["fineTuneSetAppRouting", "fineTuneSetDefaultOutput", "fineTuneSetPreferredInput"]) {
    expect(data.actions[type].menu).toBe(true);
    expect(data.actions[type].wait).toBe(true);
  }
});
