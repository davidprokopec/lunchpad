import { afterEach, expect, test } from "bun:test";
import { api, events, type Settings, type WaveLinkSettings } from "../src/lib/api";
import { useSettingsStore } from "../src/store/settings";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const originalApi = { ...api };
const originalEvents = { ...events };
let dispose = () => {};
afterEach(() => { dispose(); Object.assign(api, originalApi); Object.assign(events, originalEvents); });

async function fixture() {
  let persisted: Settings = {
    version: 1, device: null, autoConnect: false, pressFeedback: true,
    pushToTalk: { enabled: false, key: "", modifiers: [] }, audio: { outputDevice: null, macosAppVolume: false },
    waveLink: { enabled: false, autoConnect: true, port: null },
    obs: { enabled: false, host: "localhost", port: 4455, password: "", autoConnect: false },
    slobs: { enabled: false, host: "localhost", port: 59650, token: "", autoConnect: false },
    homeAssistant: { enabled: false, url: "", token: "", ignoreTlsErrors: false },
    window: { stayOnTop: false, minimizeToTray: true, runAtStartup: false, startHidden: false },
    developerMode: false, secrets: [], hub: { url: "", user: null },
  };
  let listener: (settings: Settings) => void = () => {};
  const writes: { config: WaveLinkSettings; result: ReturnType<typeof deferred<Settings>> }[] = [];
  const arrival = new Map<number, ReturnType<typeof deferred<void>>>();
  api.getSettings = async () => structuredClone(persisted);
  api.setWaveLinkSettings = (config) => {
    const result = deferred<Settings>(); writes.push({ config, result }); arrival.get(writes.length)?.resolve(); return result.promise;
  };
  events.onSettings = async (cb) => { listener = cb; return () => {}; };
  events.onInputUnavailable = async () => () => {};
  useSettingsStore.setState({ settings: null, audioDesired: null, audioError: null, audioPending: false, waveLinkDesired: null, waveLinkError: null, waveLinkPending: false });
  dispose = await useSettingsStore.getState().init();
  return {
    snapshot: () => structuredClone(persisted),
    event: (settings: Settings) => listener(settings),
    writes,
    async request(n: number) { if (writes.length < n) { const d = deferred<void>(); arrival.set(n, d); await d.promise; } return writes[n - 1]; },
    async succeed(n: number) { persisted = { ...persisted, waveLink: writes[n - 1].config }; listener(structuredClone(persisted)); writes[n - 1].result.resolve(structuredClone(persisted)); },
    persist: (settings: Settings) => { persisted = settings; },
  };
}

test("Wave Link field intents serialize; stale audio/unrelated events and responses cannot revive disabled config", async () => {
  const f = await fixture(); const stale = f.snapshot();
  const first = useSettingsStore.getState().setWaveLink({ enabled: true }); await f.request(1);
  const second = useSettingsStore.getState().setWaveLink({ enabled: false });
  const third = useSettingsStore.getState().setWaveLink({ port: 1890 });
  expect(f.writes).toHaveLength(1);
  f.event(stale);
  expect(useSettingsStore.getState().waveLinkDesired).toEqual({ enabled: false, autoConnect: true, port: 1890 });
  await f.succeed(1); await first; await f.request(2);
  expect(f.writes[1].config.enabled).toBe(false);
  await f.succeed(2); await second; await f.request(3); await f.succeed(3); await third;
  api.setDeveloperMode = async () => ({ ...stale, developerMode: true });
  await useSettingsStore.getState().setDeveloperMode(true);
  f.event(stale);
  expect(useSettingsStore.getState().settings?.waveLink).toEqual({ enabled: false, autoConnect: true, port: 1890 });
  expect(useSettingsStore.getState().waveLinkPending).toBe(false);
});

test("Wave Link settlement merges only its field while audio and window updates finish", async () => {
  const f = await fixture();
  const changing = useSettingsStore.getState().setWaveLink({ enabled: true }); await f.request(1);
  const read = deferred<Settings>(); const entered = deferred<void>();
  api.getSettings = () => { entered.resolve(); return read.promise; };
  await f.succeed(1); await entered.promise;
  const settled = f.snapshot();
  const newer = { ...settled, audio: { outputDevice: "new-output", macosAppVolume: false }, window: { ...settled.window, stayOnTop: true } };
  api.setAudioSettings = async () => newer;
  api.setWindowSettings = async () => newer;
  // Audio has its own fenced read; supply the newer persisted snapshot to that read.
  api.getSettings = async () => newer;
  const audio = useSettingsStore.getState().setAudio({ outputDevice: "new-output" });
  await useSettingsStore.getState().setWindow(newer.window); await audio;
  read.resolve(settled); await changing;
  expect(useSettingsStore.getState().settings?.audio).toEqual(newer.audio);
  expect(useSettingsStore.getState().settings?.window.stayOnTop).toBe(true);
  expect(useSettingsStore.getState().settings?.waveLink.enabled).toBe(true);
  expect(useSettingsStore.getState().audioPending).toBe(false);
  expect(useSettingsStore.getState().waveLinkPending).toBe(false);
});

test("save failure retains desired config, confirms persisted field and permits retry", async () => {
  const f = await fixture();
  const attempt = useSettingsStore.getState().setWaveLink({ enabled: true });
  const caught = attempt.catch((e: unknown) => e); await f.request(1);
  f.writes[0].result.reject(new Error("disk full")); await caught;
  expect(useSettingsStore.getState().waveLinkError).toContain("disk full");
  expect(useSettingsStore.getState().settings?.waveLink.enabled).toBe(false);
  expect(useSettingsStore.getState().waveLinkDesired?.enabled).toBe(true);
  expect(useSettingsStore.getState().waveLinkPending).toBe(false);
  const retry = useSettingsStore.getState().setWaveLink({}); await f.request(2);
  expect(f.writes[1].config.enabled).toBe(true);
  await f.succeed(2); await retry;
  expect(useSettingsStore.getState().waveLinkError).toBeNull();
});

test("new Wave Link intent during settlement rejects its older read without settling the queue", async () => {
  const f = await fixture();
  const first = useSettingsStore.getState().setWaveLink({ enabled: true }); await f.request(1);
  const read = deferred<Settings>(); const entered = deferred<void>();
  api.getSettings = () => { entered.resolve(); return read.promise; };
  await f.succeed(1); await entered.promise;
  const second = useSettingsStore.getState().setWaveLink({ enabled: false });
  read.resolve(f.snapshot()); await first;
  expect(useSettingsStore.getState().waveLinkPending).toBe(true);
  api.getSettings = async () => f.snapshot();
  await f.request(2); await f.succeed(2); await second;
  expect(useSettingsStore.getState().settings?.waveLink.enabled).toBe(false);
  expect(useSettingsStore.getState().waveLinkPending).toBe(false);
});

test("Wave Link confirmation failure reports error, keeps intent and does not replace unrelated fields", async () => {
  const f = await fixture();
  const attempt = useSettingsStore.getState().setWaveLink({ enabled: true });
  const caught = attempt.catch((e: unknown) => e); await f.request(1);
  api.getSettings = async () => { throw new Error("read failed"); };
  await f.succeed(1); await caught;
  expect(useSettingsStore.getState().settings?.waveLink.enabled).toBe(true);
  expect(useSettingsStore.getState().waveLinkDesired?.enabled).toBe(true);
  expect(useSettingsStore.getState().waveLinkError).toContain("read failed");
  expect(useSettingsStore.getState().waveLinkPending).toBe(false);
});
