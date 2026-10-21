import { expect, test } from "bun:test";
import fixtures from "./fixtures/audio-group-summary.json";
import type { AudioGroupMemberReport, AudioTarget } from "../src/lib/api";
import { aggregateGroup, audioTargetConfigured } from "../src/lib/systemAudio";
import { muteButton } from "../src/lib/buttonPresets";

for (const fixture of fixtures) {
  test(`group aggregation: ${fixture.name}`, () => {
    const members: AudioGroupMemberReport[] = fixture.members.map((member, index) => ({
      ...member, target: { type: "app", app: { id: `${index}`, name: "" } }, canSetVolume: false, canMute: false, reason: "read-only fixture",
    }));
    expect(aggregateGroup(members)).toEqual(fixture.expected);
  });
}

test("group reference remains structurally valid offline and mute preset stays unsupported", () => {
  const target: AudioTarget = { type: "group", groupId: "comms" };
  expect(audioTargetConfigured(target)).toBe(true);
  expect(audioTargetConfigured({ type: "group", groupId: " " })).toBe(false);
  expect(() => muteButton({ target, caption: "Comms", color: { mode: "palette", index: 1 }, mutedColor: { mode: "palette", index: 2 } })).toThrow("unsupported");
});
