import { expect, test } from "bun:test";
import { emptyButton, type Action, type Page } from "../src/lib/api";
import { audioGroupUses } from "../src/lib/audioGroups";
import { newFader } from "../src/lib/fader";

const group = (groupId: string) => ({ type: "group" as const, groupId });
const setVolume = (id: string, groupId: string): Action => ({ id, wait: true, type: "setAudio", target: group(groupId), mode: "setVolume", volume: 50, volumeFrom: null });

test("audioGroupUses finds every place the backend checks, per group, read-only", () => {
  const pages: Page[] = [
    {
      id: "p1", name: "Main",
      buttons: [
        { ...emptyButton(), x: 2, y: 0, down: [setVolume("a", "comms")] },
        { ...emptyButton(), x: 0, y: 3, hold: [setVolume("b", "other")], stateLink: { provider: "system", kind: "audioMuted", scene: "", source: "", target: group("comms") } },
      ],
      faders: [
        { ...newFader(4, 0, null), id: "f1", name: "Comms", bound: group("comms") },
        { ...newFader(5, 0, null), id: "f2", name: "", bound: null, onRelease: [setVolume("c", "comms")] },
      ],
    },
    { id: "p2", name: "Stream", buttons: [], faders: [{ ...newFader(0, 0, null), id: "f3", name: "Music", bound: group("other"), onChange: [setVolume("d", "comms")] }] },
  ];
  const before = JSON.stringify(pages);
  expect(audioGroupUses(pages, "comms")).toEqual([
    { pageId: "p1", pageName: "Main", kind: "button", name: "3,1", how: "action" },
    { pageId: "p1", pageName: "Main", kind: "button", name: "1,4", how: "link" },
    { pageId: "p1", pageName: "Main", kind: "fader", name: "Comms", how: "bound" },
    { pageId: "p1", pageName: "Main", kind: "fader", name: "f2", how: "action" },
    { pageId: "p2", pageName: "Stream", kind: "fader", name: "Music", how: "action" },
  ]);
  expect(audioGroupUses(pages, "other")).toEqual([
    { pageId: "p1", pageName: "Main", kind: "button", name: "1,4", how: "action" },
    { pageId: "p2", pageName: "Stream", kind: "fader", name: "Music", how: "bound" },
  ]);
  expect(audioGroupUses(pages, "unused")).toEqual([]);
  expect(audioGroupUses([], "comms")).toEqual([]);
  // Single targets and other actions never count; input is not modified.
  const single: Page = { id: "p3", name: "Single", buttons: [{ ...emptyButton(), x: 0, y: 0, down: [{ id: "e", wait: true, type: "setAudio", target: { type: "device", device: { type: "defaultOutput" } }, mode: "setVolume", volume: 50, volumeFrom: null }] }], faders: [{ ...newFader(0, 1, null), id: "f4", bound: { type: "device", device: { type: "defaultInput" } } }] };
  expect(audioGroupUses([single], "x")).toEqual([]);
  expect(JSON.stringify(pages)).toBe(before);
});
