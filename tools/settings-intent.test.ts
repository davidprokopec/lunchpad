import { afterEach, expect, test } from "bun:test";
import { api, events, type AudioSettings, type Settings } from "../src/lib/api";
import { useSettingsStore } from "../src/store/settings";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function settings(audio: AudioSettings): Settings {
  return {
    version: 1, device: null, autoConnect: false, pressFeedback: true,
    pushToTalk: { enabled: false, key: "", modifiers: [] }, audio,
    obs: { enabled: false, host: "localhost", port: 4455, password: "", autoConnect: false },
    slobs: { enabled: false, host: "localhost", port: 59650, token: "", autoConnect: false },
    homeAssistant: { enabled: false, url: "", token: "", ignoreTlsErrors: false },
    window: { stayOnTop: false, minimizeToTray: true, runAtStartup: false, startHidden: false },
    developerMode: false, secrets: [], hub: { url: "", user: null },
  };
}

const original = { getSettings: api.getSettings, setAudioSettings: api.setAudioSettings, setDeveloperMode: api.setDeveloperMode, onSettings: events.onSettings, onInputUnavailable: events.onInputUnavailable };
let dispose = () => {};
afterEach(() => {
  dispose();
  Object.assign(api, { getSettings: original.getSettings, setAudioSettings: original.setAudioSettings, setDeveloperMode: original.setDeveloperMode });
  Object.assign(events, { onSettings: original.onSettings, onInputUnavailable: original.onInputUnavailable });
});

async function transport(enabled: boolean) {
  let persisted = settings({ outputDevice: null, macosAppVolume: enabled });
  let native = enabled;
  let listener: ((value: Settings) => void) | null = null;
  const requests: { config: AudioSettings; result: ReturnType<typeof deferred<Settings>> }[] = [];
  const arrivals = new Map<number, ReturnType<typeof deferred<void>>>();
  let refresh: (() => Promise<Settings>) | null = null;
  api.getSettings = () => refresh ? refresh() : Promise.resolve(structuredClone(persisted));
  api.setAudioSettings = (config) => {
    const result = deferred<Settings>();
    requests.push({ config: structuredClone(config), result });
    arrivals.get(requests.length)?.resolve();
    return result.promise;
  };
  events.onSettings = async (cb) => { listener = cb; return () => { listener = null; }; };
  events.onInputUnavailable = async () => () => {};
  useSettingsStore.setState({ settings: null, audioDesired: null, audioError: null, audioPending: false });
  dispose = await useSettingsStore.getState().init();
  const request = async (number: number) => {
    if (requests.length < number) {
      const arrival = deferred<void>(); arrivals.set(number, arrival); await arrival.promise;
    }
    const value = requests[number - 1];
    if (!value) throw new Error(`missing request ${number}`);
    return value;
  };
  return {
    requests, request,
    persisted: () => persisted.audio,
    native: () => native,
    event: (value: Settings) => listener?.(value),
    refreshWith: (next: (() => Promise<Settings>) | null) => { refresh = next; },
    async succeed(number: number, response?: Settings) {
      const req = await request(number);
      persisted = { ...persisted, audio: req.config }; native = req.config.macosAppVolume;
      listener?.(structuredClone(persisted)); req.result.resolve(structuredClone(response ?? persisted));
    },
    async fail(number: number, saved: boolean) {
      const req = await request(number);
      if (saved) persisted = { ...persisted, audio: req.config };
      listener?.(structuredClone(persisted)); req.result.reject(new Error(saved ? "native apply failed" : "save failed"));
    },
  };
}

test("delayed disable then output-only edit preserves the latest opt-out", async () => {
  const t = await transport(true);
  const disable = useSettingsStore.getState().setAudio({ macosAppVolume: false });
  await t.request(1);
  const output = useSettingsStore.getState().setAudio({ outputDevice: "headphones" });
  expect(useSettingsStore.getState().audioDesired).toEqual({ outputDevice: "headphones", macosAppVolume: false });
  expect(t.requests).toHaveLength(1);
  await t.succeed(1); await disable;
  expect((await t.request(2)).config).toEqual({ outputDevice: "headphones", macosAppVolume: false });
  await t.succeed(2); await output;
  expect(t.persisted()).toEqual({ outputDevice: "headphones", macosAppVolume: false });
  expect(t.native()).toBe(false);
  expect(useSettingsStore.getState().audioPending).toBe(false);
});

test("output first then opt-in and another output preserve each explicit field", async () => {
  const t = await transport(false);
  const first = useSettingsStore.getState().setAudio({ outputDevice: "one" });
  await t.request(1);
  const enable = useSettingsStore.getState().setAudio({ macosAppVolume: true });
  const last = useSettingsStore.getState().setAudio({ outputDevice: "two" });
  await t.succeed(1); await first;
  expect((await t.request(2)).config).toEqual({ outputDevice: "one", macosAppVolume: true });
  await t.succeed(2); await enable;
  expect((await t.request(3)).config).toEqual({ outputDevice: "two", macosAppVolume: true });
  await t.succeed(3); await last;
  expect(t.persisted()).toEqual({ outputDevice: "two", macosAppVolume: true });
  expect(t.native()).toBe(true);
});

for (const saved of [false, true]) {
  test(`${saved ? "apply" : "save"} failure keeps desired opt-out, rejects, refreshes persistence and allows retry`, async () => {
    const t = await transport(true);
    const result = useSettingsStore.getState().setAudio({ macosAppVolume: false });
    const rejected = result.then<unknown, unknown>(() => null, (error: unknown) => error);
    await t.fail(1, saved);
    const failure = await rejected;
    expect(() => { throw failure; }).toThrow(saved ? "native apply failed" : "save failed");
    expect(t.native()).toBe(true);
    expect(useSettingsStore.getState().settings?.audio.macosAppVolume).toBe(!saved);
    expect(useSettingsStore.getState().audioDesired?.macosAppVolume).toBe(false);
    expect(useSettingsStore.getState().audioError).toContain(saved ? "native apply failed" : "save failed");
    expect(useSettingsStore.getState().audioPending).toBe(false);
    const retry = useSettingsStore.getState().setAudio({ outputDevice: "retry-output" });
    expect((await t.request(2)).config.macosAppVolume).toBe(false);
    await t.succeed(2); await retry;
    expect(t.native()).toBe(false);
    expect(useSettingsStore.getState().audioError).toBeNull();
  });
}

test("stale settings events and non-audio responses cannot overwrite newer audio intent", async () => {
  const t = await transport(true);
  const stale = settings({ outputDevice: "old", macosAppVolume: true });
  const other = deferred<Settings>();
  api.setDeveloperMode = () => other.promise;
  const changingOther = useSettingsStore.getState().setDeveloperMode(true);
  const disable = useSettingsStore.getState().setAudio({ macosAppVolume: false });
  await t.request(1);
  const output = useSettingsStore.getState().setAudio({ outputDevice: "new" });
  t.event(stale);
  await t.succeed(1); await disable;
  expect(useSettingsStore.getState().audioDesired).toEqual({ outputDevice: "new", macosAppVolume: false });
  await t.succeed(2, stale); await output;
  other.resolve(stale); await changingOther;
  const refreshed = deferred<Settings>();
  t.refreshWith(() => refreshed.promise);
  t.event(stale);
  expect(useSettingsStore.getState().settings?.audio).toEqual({ outputDevice: "new", macosAppVolume: false });
  const newIntent = useSettingsStore.getState().setAudio({ outputDevice: "newest" });
  refreshed.resolve(stale);
  t.refreshWith(null);
  await t.succeed(3); await newIntent;
  expect(t.persisted()).toEqual({ outputDevice: "newest", macosAppVolume: false });
  expect(useSettingsStore.getState().audioDesired).toEqual(t.persisted());
});

test("a failed disable does not poison an already queued output edit or revive its stale opt-in", async () => {
  const t = await transport(true);
  const disable = useSettingsStore.getState().setAudio({ macosAppVolume: false });
  const rejected = disable.then<unknown, unknown>(() => null, (error: unknown) => error);
  await t.request(1);
  const output = useSettingsStore.getState().setAudio({ outputDevice: "after-error" });
  await t.fail(1, true);
  const failure = await rejected;
  expect(() => { throw failure; }).toThrow("native apply failed");
  expect((await t.request(2)).config).toEqual({ outputDevice: "after-error", macosAppVolume: false });
  await t.succeed(2); await output;
  expect(t.persisted()).toEqual({ outputDevice: "after-error", macosAppVolume: false });
  expect(t.native()).toBe(false);
  expect(useSettingsStore.getState().audioError).toBeNull();
});

test("an edit arriving during settlement invalidates that read and keeps the queue pending", async () => {
  const t = await transport(true);
  const settlement = deferred<Settings>();
  const entered = deferred<void>();
  t.refreshWith(() => { entered.resolve(); return settlement.promise; });
  const disable = useSettingsStore.getState().setAudio({ macosAppVolume: false });
  await t.succeed(1);
  await entered.promise;
  const output = useSettingsStore.getState().setAudio({ outputDevice: "during-settlement" });
  settlement.resolve(settings({ outputDevice: null, macosAppVolume: true }));
  await disable;
  expect(useSettingsStore.getState().audioPending).toBe(true);
  expect(useSettingsStore.getState().audioDesired?.macosAppVolume).toBe(false);
  t.refreshWith(null);
  expect((await t.request(2)).config).toEqual({ outputDevice: "during-settlement", macosAppVolume: false });
  await t.succeed(2); await output;
  expect(useSettingsStore.getState().settings?.audio).toEqual(t.persisted());
  expect(useSettingsStore.getState().audioPending).toBe(false);
});

test("refresh failure does not report settlement success and a retry survives it", async () => {
  const t = await transport(true);
  t.refreshWith(async () => { throw new Error("refresh failed"); });
  const attempt = useSettingsStore.getState().setAudio({ macosAppVolume: false });
  const rejected = attempt.then<unknown, unknown>(() => null, (error: unknown) => error);
  await t.succeed(1);
  const failure = await rejected;
  expect(() => { throw failure; }).toThrow("refresh failed");
  expect(t.native()).toBe(false);
  expect(useSettingsStore.getState().audioError).toContain("refresh failed");
  expect(useSettingsStore.getState().audioDesired?.macosAppVolume).toBe(false);
  expect(useSettingsStore.getState().audioPending).toBe(false);
  t.refreshWith(null);
  const retry = useSettingsStore.getState().setAudio({});
  expect((await t.request(2)).config.macosAppVolume).toBe(false);
  await t.succeed(2); await retry;
  expect(useSettingsStore.getState().audioError).toBeNull();
});
