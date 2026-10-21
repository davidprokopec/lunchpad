import { expect, test } from "bun:test";
import { api, events, type AudioGroupReport, type AudioMemberTarget } from "../src/lib/api";
import { audioGroupFeedback, audioGroupValidation, audioMemberKey, mergeAudioGroupReports } from "../src/lib/audioGroups";
import { aggregateGroup, audioControllable, audioFader, audioFaderReadable } from "../src/lib/systemAudio";
import { useMediaStore } from "../src/store/media";
import { useProfileStore } from "../src/store/profile";
import { newFader } from "../src/lib/fader";
import { isLinkActive } from "../src/lib/stateLink";
import fixtures from "./fixtures/audio-group-summary.json";

const target: AudioMemberTarget = { type: "app", app: { id: "catalogue-discord", name: "Discord" } };
const report = (revision = 1): AudioGroupReport => {
  const members = [{ target, volume: .5, muted: false, canSetVolume: true, canMute: true, reason: null }];
  return { groupId: "comms", revision, members, summary: aggregateGroup(members), busy: false, restoreOwned: false, operation: null };
};
const deferred = <T,>() => { let resolve!: (value: T) => void; const promise = new Promise<T>((yes) => { resolve = yes; }); return { promise, resolve }; };

test("group receipt fence protects empty deletions and ignores late failures/older reads", async () => {
  const store = useMediaStore.getState();
  useMediaStore.setState({ audioGroups: [report(8)], audioGroupsError: null });
  const old = deferred<AudioGroupReport[]>(); api.getAudioGroups = () => old.promise;
  const read = store.loadAudioGroups();
  store.receiveAudioGroups([]); old.resolve([report(9)]); await read;
  expect(useMediaStore.getState().audioGroups).toEqual([]);
  const empty = deferred<AudioGroupReport[]>(); api.getAudioGroups = () => empty.promise;
  const lateEmpty = store.loadAudioGroups(); store.receiveAudioGroups([report(10)]); empty.resolve([]); await lateEmpty;
  expect(useMediaStore.getState().audioGroups[0].revision).toBe(10);
  const first = deferred<AudioGroupReport[]>(); const second = deferred<AudioGroupReport[]>();
  api.getAudioGroups = () => first.promise; const a = store.loadAudioGroups();
  api.getAudioGroups = () => second.promise; const b = store.loadAudioGroups();
  second.resolve([report(12)]); await b; first.resolve([]); await a;
  expect(useMediaStore.getState().audioGroups[0].revision).toBe(12);
  api.getAudioGroups = async () => [report(2)]; await store.loadAudioGroups();
  expect(useMediaStore.getState().audioGroups[0].revision).toBe(12);
  expect(mergeAudioGroupReports([report(12)], [])).toEqual([]);
  const equal = report(12); equal.members[0].volume = .1;
  expect(mergeAudioGroupReports([report(12)], [equal])[0].members[0].volume).toBe(.5);
  let reject!: (error: Error) => void;
  api.getAudioGroups = () => new Promise((_, no) => { reject = no; });
  const failure = store.loadAudioGroups(); store.receiveAudioGroups([]); reject(Error("stale error")); await failure;
  expect(useMediaStore.getState().audioGroupsError).toBeNull();
});

test("group subscription is installed before read; publications never call setters", async () => {
  let listener: (reports: AudioGroupReport[]) => void = () => {};
  const calls: string[] = [];
  events.onAudioGroups = async (cb) => { calls.push("subscribe"); listener = cb; return () => { calls.push("dispose"); }; };
  api.getAudioGroups = async () => { calls.push("read"); return []; };
  events.onFineTuneState = events.onWaveLinkState = events.onObsState = events.onSlobsState = events.onHomeAssistantState = events.onSystemAudio = async () => () => {};
  api.obsState = api.slobsState = async () => { throw Error("offline fixture"); };
  api.homeAssistantState = api.audioStatus = api.waveLinkState = api.fineTuneState = async () => { throw Error("offline fixture"); };
  const oldSet = api.setAudioGroup; let writes = 0;
  api.setAudioGroup = async () => { writes++; };
  const dispose = await useMediaStore.getState().init();
  expect(calls).toEqual(["subscribe", "read"]);
  listener([report(20)]); listener([]); expect(writes).toBe(0); expect(useMediaStore.getState().audioGroups).toEqual([]);
  dispose(); expect(calls.at(-1)).toBe("dispose"); api.setAudioGroup = oldSet;
});

test("offline definitions validate label-independent qualified identities and size", () => {
  const group = { id: "comms", name: "Comms", members: [target] };
  expect(audioGroupValidation(group)).toBeNull();
  expect(audioGroupValidation({ ...group, name: "  " })).toMatch(/name/);
  expect(audioGroupValidation({ ...group, members: [] })).toMatch(/1–32/);
  expect(audioGroupValidation({ ...group, members: [target, { ...target, app: { ...target.app, name: "Renamed" } }] })).toMatch(/once/);
  const wave = (parent: string): AudioMemberTarget => ({ type: "waveLink", target: { type: "output", outputDeviceId: parent, outputId: "same" } });
  expect(audioMemberKey(wave("a"))).not.toBe(audioMemberKey(wave("b")));
  expect(audioGroupValidation({ ...group, members: [wave("a"), wave("b")] })).toBeNull();
  expect(audioGroupValidation({ ...group, members: Array.from({ length: 33 }, (_, n) => wave(String(n))) })).toMatch(/1–32/);
  expect(audioGroupValidation({ ...group, members: [{ type: "device", device: { type: "named", id: "", kind: "output", label: "Offline" } }] })).toMatch(/complete/);
});

test("group feedback and fader remain observed, mixed/partial/nullable/read-only/busy are independent", () => {
  const r = report();
  r.members.push({ ...r.members[0], target: { type: "waveLink", target: { type: "channel", channelId: "other" } }, volume: .9, muted: null, canSetVolume: false, reason: "Permission denied" });
  const bound = { type: "group" as const, groupId: "comms" };
  const fader = { ...newFader(0, 0, null), bound, value: 99 };
  expect(audioFader(fader, null, null, null, [r]).value).toBeCloseTo(70);
  expect(audioGroupFeedback(r)).toMatch(/Mixed · 50%–90% · mean 70%/);
  r.members[1].volume = null;
  expect(audioGroupFeedback(r)).toMatch(/Partial · 1\/2 readable/);
  r.members[0].muted = true;
  const link = { provider: "system" as const, kind: "audioMuted" as const, scene: "", source: "", target: bound };
  expect(isLinkActive(link, null, null, null, null, null, [r])).toBe(false);
  expect(audioFader(fader, null, null, null, [r]).value).toBe(50);
  expect(audioGroupFeedback(r)).not.toMatch(/All muted/);
  r.members[1].muted = true;
  expect(isLinkActive(link, null, null, null, null, null, [r])).toBe(true);
  expect(isLinkActive(link, null, null, null, null, null, [])).toBe(false);
  expect(audioFader(fader, null, null, null, [r]).value).toBe(0);
  r.members[0].canSetVolume = false;
  expect(audioFaderReadable(bound, null, null, null, [r])).toBe(true);
  expect(audioControllable(bound, null, "volume", null, null, [r])).toBe(false);
  expect(audioGroupFeedback(r)).toMatch(/Read-only/);
  r.members[0].canSetVolume = true; r.busy = true;
  expect(audioControllable(bound, null, "volume", null, null, [r])).toBe(false);
  expect(audioGroupFeedback(r)).toMatch(/pending/);
  expect(audioControllable(bound, null, "mute", null, null, [r])).toBe(false);
  r.members[0].volume = null;
  expect(audioFaderReadable(bound, null, null, null, [r])).toBe(false);
  expect(audioGroupFeedback(r)).toMatch(/^Unavailable/);
});

test("UI aggregation uses the backend shared fixtures", () => {
  for (const fixture of fixtures) {
    const r = report();
    r.members = fixture.members.map((member, index) => ({ ...member, target: { type: "app", app: { id: String(index), name: "" } }, canSetVolume: false, canMute: false, reason: null }));
    const bound = { type: "group" as const, groupId: r.groupId };
    expect(audioFaderReadable(bound, null, null, null, [r])).toBe(fixture.expected.volume !== null);
    expect(audioFader({ ...newFader(0, 0, null), bound }, null, null, null, [r]).value).toBeCloseTo((fixture.expected.volume ?? 0) * 100);
    expect(audioGroupFeedback(r).startsWith("Unavailable")).toBe(fixture.expected.volume === null);
  }
  const r = report(); r.operation = { id: 1, members: [{ target, outcome: "unconfirmed", message: "No readback" }] };
  expect(audioGroupFeedback(r)).toMatch(/1 unconfirmed/);
  r.operation.members[0].outcome = "failed";
  expect(audioGroupFeedback(r)).toMatch(/1 failed/);
});

test("profile initial fetch cannot erase groups delivered by a newer profile receipt", async () => {
  const originalWindow = globalThis.window;
  globalThis.window = { setTimeout: () => 1, clearTimeout: () => {} } as unknown as Window & typeof globalThis;
  try {
    let listener: Parameters<typeof events.onProfile>[0] = () => {};
    events.onProfile = async (cb) => { listener = cb; return () => {}; };
    events.onHistory = async () => () => {};
    const initial = deferred<Awaited<ReturnType<typeof api.getProfile>>>();
    api.getProfile = () => initial.promise;
    api.historyState = async () => ({ undo: 0, redo: 0 });
    api.profileHealth = async () => { throw Error("offline fixture"); };
    const init = useProfileStore.getState().init();
    await Promise.resolve(); await Promise.resolve();
    const profile = { version: 1, activePage: "p", pages: [], audioGroups: [{ id: "comms", name: "Comms", members: [target] }] };
    listener(profile);
    initial.resolve({ ...profile, audioGroups: [] });
    const dispose = await init;
    expect(useProfileStore.getState().profile?.audioGroups).toEqual(profile.audioGroups);
    dispose();
  } finally { globalThis.window = originalWindow; }
});

test("FineTune member identity ignores the optional label, matching Rust", () => {
  const bare: AudioMemberTarget = { type: "fineTune", target: { type: "output", deviceUid: "LogiGamingAudio:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1" } };
  const labelled: AudioMemberTarget = { type: "fineTune", target: { type: "output", deviceUid: "LogiGamingAudio:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1", label: "PRO X 2 LIGHTSPEED" } };
  const otherDirection: AudioMemberTarget = { type: "fineTune", target: { type: "input", deviceUid: "LogiGamingAudio:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1", label: "PRO X 2 LIGHTSPEED" } };
  expect(audioMemberKey(labelled)).toBe(audioMemberKey(bare));
  expect(audioMemberKey(otherDirection)).not.toBe(audioMemberKey(bare));
  expect(audioGroupValidation({ id: "g", name: "G", members: [bare, labelled] })).toBe("Each member can only appear once.");
});
