import { describe, expect, test } from "bun:test";
import type { Layout, PadSpec, RemoteState } from "../src/lib/api";
import { qrPath } from "../src/components/QrCode";
import { RATE_LIMITED_DELAY, RECONNECT_STEPS, reconnectDelay } from "../src/remote/backoff";
import { FADER_INTERVAL, HANDSHAKE_TIMEOUT, NAME_KEY, PING_INTERVAL, RemoteClient, SILENCE_LIMIT, TOKEN_KEY, type ClientSnapshot, type KeyValueStore, type SocketLike } from "../src/remote/client";
import { cleanCode, cleanName, defaultPhoneName, formatCode, isFullCode, readPairCode } from "../src/remote/pairing";
import { PressTracker } from "../src/remote/presses";
import { boardOf, faceOf, fitBoard, indexState, levelStep, padRole, stripFraction } from "../src/remote/view";

// ---- fakes -------------------------------------------------------------------------------

class FakeSocket implements SocketLike {
  readyState = 0;
  sent: Record<string, unknown>[] = [];
  closed = false;
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  constructor(public url: string) {}
  send(data: string) {
    if (this.readyState !== 1) throw new Error("not open");
    this.sent.push(JSON.parse(data));
  }
  close() {
    this.closed = true;
    this.readyState = 3;
  }
  // server side
  open() {
    this.readyState = 1;
    this.onopen?.({});
  }
  push(msg: unknown) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
  drop(code = 1006) {
    this.readyState = 3;
    this.onclose?.({ code, reason: "" });
  }
}

function harness(stored: Record<string, string> = {}) {
  const data = new Map(Object.entries(stored));
  const storage: KeyValueStore = { get: (k) => data.get(k) ?? null, set: (k, v) => void data.set(k, v), remove: (k) => void data.delete(k) };
  let now = 1_000_000;
  let nextId = 1;
  const timers = new Map<number, { at: number; fn: () => void }>();
  const sockets: FakeSocket[] = [];
  const client = new RemoteClient({
    url: "ws://192.168.1.5:7373/ws",
    createSocket: (url) => {
      const s = new FakeSocket(url);
      sockets.push(s);
      return s;
    },
    storage,
    setTimeout: (fn, ms) => {
      const id = nextId++;
      timers.set(id, { at: now + ms, fn });
      return id;
    },
    clearTimeout: (id) => void timers.delete(id as number),
    now: () => now,
    random: () => 0.5,
  });
  const snaps: ClientSnapshot[] = [];
  client.subscribe((s) => snaps.push(s));
  /** Move the clock, running timers in order. */
  const advance = (ms: number) => {
    const end = now + ms;
    for (;;) {
      const due = [...timers.entries()].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      timers.delete(due[0]);
      now = due[1].at;
      due[1].fn();
    }
    now = end;
  };
  const last = () => sockets[sockets.length - 1];
  return { client, data, sockets, last, advance, snaps, timers, now: () => now };
}

const STATE: Omit<RemoteState, "v"> & { type: "state"; v: number } = {
  type: "state",
  v: 1,
  device: { model: "LaunchpadX", modelName: "Launchpad X", virtual: false },
  layout: null,
  pages: [
    { id: "p1", name: "Main" },
    { id: "p2", name: "Stream" },
  ],
  activePage: "p1",
  pads: [{ x: 0, y: 0, pressed: false, color: { mode: "palette", index: 5, rgb: "#fc0000" } }],
  faders: [{ id: "f1", name: "Mic", direction: "up", pads: [[8, 0], [8, 1], [8, 2], [8, 3]], value: 50, fraction: 0.5, display: "50%" }],
};

function signedIn() {
  const h = harness({ [TOKEN_KEY]: "tok" });
  h.client.start();
  h.last().open();
  h.last().push({ type: "authOk", v: 1, deviceId: "d1", name: "Pixel" });
  h.last().push(STATE);
  h.last().sent.length = 0;
  return h;
}

// ---- pairing helpers ---------------------------------------------------------------------

describe("pairing link and code", () => {
  test("reads the code from the QR fragment", () => {
    expect(readPairCode("#pair=12345678")).toBe("12345678");
    expect(readPairCode("pair=12345678")).toBe("12345678");
    expect(readPairCode("#x=1&pair=8765%204321")).toBe("87654321");
    expect(readPairCode("#pair=1234-5678")).toBe("12345678");
  });
  test("ignores missing, short, long, non-digit and broken codes", () => {
    for (const hash of ["", "#", "#pair=", "#pair=1234567", "#pair=123456789", "#pair=1234abcd", "#pair=%E0%A4%A", "#code=12345678"]) expect(readPairCode(hash)).toBeNull();
  });
  test("typed codes keep digits only, eight at most", () => {
    expect(cleanCode("1234 5678")).toBe("12345678");
    expect(cleanCode("12a3-4 56789999")).toBe("12345678");
    expect(isFullCode("12345678")).toBe(true);
    expect(isFullCode("1234567")).toBe(false);
    expect(formatCode("12345678")).toBe("1234 5678");
    expect(formatCode("123")).toBe("123");
  });
  test("names: a sensible default, trimmed and capped", () => {
    expect(defaultPhoneName("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)")).toBe("iPhone");
    expect(defaultPhoneName("Mozilla/5.0 (Linux; Android 14; Pixel 8) Mobile Safari")).toBe("Android phone");
    expect(defaultPhoneName("Mozilla/5.0 (Linux; Android 14; SM-X710) Safari")).toBe("Android tablet");
    expect(defaultPhoneName("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)", 5)).toBe("iPad");
    expect(defaultPhoneName("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)", 0)).toBe("Phone");
    expect(cleanName("  My   phone \n")).toBe("My phone");
    expect(cleanName("x".repeat(60))).toHaveLength(40);
  });
});

// ---- backoff ------------------------------------------------------------------------------

test("reconnect waits grow to a cap, spread by ±20%", () => {
  expect(RECONNECT_STEPS.map((_, i) => reconnectDelay(i, 0.5))).toEqual([500, 1000, 2000, 4000, 8000, 15000]);
  expect(reconnectDelay(99, 0.5)).toBe(15000);
  expect(reconnectDelay(-3, 0.5)).toBe(500);
  expect(reconnectDelay(2, 0)).toBe(1600);
  expect(reconnectDelay(2, 1)).toBe(2400);
});

// ---- presses -----------------------------------------------------------------------------

describe("press tracking", () => {
  test("each finger is its own press; two fingers on a pad are one hold", () => {
    const p = new PressTracker();
    expect(p.down(1, "0,0")).toBe(true);
    expect(p.down(2, "1,0")).toBe(true);
    expect(p.down(3, "0,0")).toBe(false);
    expect(p.up(1)).toBeNull(); // finger 3 still holds 0,0
    expect(p.isHeld("0,0")).toBe(true);
    expect(p.up(3)).toBe("0,0");
    expect(p.up(3)).toBeNull();
    expect(p.up(99)).toBeNull();
    expect(p.held()).toEqual(["1,0"]);
    expect(p.releaseAll()).toEqual(["1,0"]);
    expect(p.held()).toEqual([]);
  });
  test("a finger that never lifted lets go of its old pad when it lands again", () => {
    const p = new PressTracker();
    p.down(1, "0,0");
    expect(p.down(1, "2,2")).toBe(true);
    expect(p.held()).toEqual(["2,2"]);
  });
});

// ---- client ------------------------------------------------------------------------------

describe("remote client", () => {
  test("without a token it waits for pairing; pairing stores the token and signs in", () => {
    const h = harness();
    h.client.start();
    expect(h.sockets).toHaveLength(0);
    expect(h.client.snapshot().status).toBe("unpaired");
    h.client.pair("12345678", "Pixel");
    expect(h.client.snapshot().status).toBe("connecting");
    h.last().open();
    expect(h.last().sent).toEqual([{ type: "pair", v: 1, code: "12345678", name: "Pixel" }]);
    expect(h.client.snapshot().status).toBe("signingIn");
    h.last().push({ type: "paired", v: 1, deviceId: "d1", name: "Pixel", token: "secret-token" });
    expect(h.data.get(TOKEN_KEY)).toBe("secret-token");
    expect(h.data.get(NAME_KEY)).toBe("Pixel");
    expect(h.client.snapshot()).toMatchObject({ status: "connected", paired: true, deviceName: "Pixel", problem: null });
    h.last().push(STATE);
    expect(h.client.snapshot().state?.activePage).toBe("p1");
    expect(h.client.snapshot().state).not.toHaveProperty("type");
  });

  test("an empty name is left to the app", () => {
    const h = harness();
    h.client.pair("12345678", "");
    h.last().open();
    expect(h.last().sent).toEqual([{ type: "pair", v: 1, code: "12345678" }]);
  });

  test("a stored token signs in with auth; the token never goes into the URL", () => {
    const h = harness({ [TOKEN_KEY]: "tok" });
    h.client.start();
    expect(h.last().url).toBe("ws://192.168.1.5:7373/ws");
    h.last().open();
    expect(h.last().sent).toEqual([{ type: "auth", v: 1, token: "tok" }]);
  });

  test("a refused token is forgotten and the phone shows it was removed", () => {
    const h = harness({ [TOKEN_KEY]: "tok" });
    h.client.start();
    h.last().open();
    h.last().push({ type: "error", code: "unauthorized", message: "sign-in refused" });
    h.last().drop(4401);
    expect(h.data.has(TOKEN_KEY)).toBe(false);
    expect(h.client.snapshot()).toMatchObject({ status: "unpaired", problem: "revoked", paired: false, state: null });
    h.advance(60_000);
    expect(h.sockets).toHaveLength(1); // no reconnect loop
  });

  test("pairing refusals say why and do not retry", () => {
    for (const [code, problem] of [["unauthorized", "wrongCode"], ["noPairing", "noCode"], ["rateLimited", "rateLimited"]] as const) {
      const h = harness();
      h.client.pair("12345678", "x");
      h.last().open();
      h.last().push({ type: "error", code, message: "" });
      h.last().drop(4401);
      expect(h.client.snapshot()).toMatchObject({ status: "unpaired", problem });
      expect(h.data.has(TOKEN_KEY)).toBe(false);
      h.advance(120_000);
      expect(h.sockets).toHaveLength(1);
    }
  });

  test("a pairing connection that drops without an answer goes back to the form", () => {
    const h = harness();
    h.client.pair("12345678", "x");
    h.last().drop();
    expect(h.client.snapshot()).toMatchObject({ status: "unpaired", problem: "lost" });
  });

  test("a lost connection reconnects with growing waits and resets after signing in", () => {
    const h = signedIn();
    h.client.press(1, 0, 0);
    h.last().drop();
    let s = h.client.snapshot();
    expect(s.status).toBe("waiting");
    expect(s.problem).toBe("lost");
    expect(s.held).toEqual([]); // the app let go when the socket closed
    expect(s.state?.activePage).toBe("p1"); // the pads stay in view, dimmed
    expect(s.retryAt! - h.now()).toBe(500);
    h.advance(500);
    expect(h.sockets).toHaveLength(2);
    h.last().drop(); // refused: the computer is asleep
    expect(h.client.snapshot().retryAt! - h.now()).toBe(1000);
    h.advance(1000);
    h.last().drop();
    expect(h.client.snapshot().retryAt! - h.now()).toBe(2000);
    h.advance(2000);
    h.last().open();
    expect(h.last().sent).toEqual([{ type: "auth", v: 1, token: "tok" }]);
    h.last().push({ type: "authOk", v: 1, deviceId: "d1", name: "Pixel" });
    s = h.client.snapshot();
    expect(s).toMatchObject({ status: "connected", attempt: 0, problem: null });
    h.last().drop();
    expect(h.client.snapshot().retryAt! - h.now()).toBe(500);
  });

  test("retryNow skips the wait (page back in front, network back)", () => {
    const h = signedIn();
    h.last().drop();
    h.client.retryNow();
    expect(h.sockets).toHaveLength(2);
    expect(h.client.snapshot().status).toBe("connecting");
    h.client.retryNow(); // not waiting: nothing happens
    expect(h.sockets).toHaveLength(2);
  });

  test("rate limited and full: wait longer before signing in again", () => {
    const h = harness({ [TOKEN_KEY]: "tok" });
    h.client.start();
    h.last().open();
    h.last().push({ type: "error", code: "rateLimited", message: "" });
    expect(h.client.snapshot()).toMatchObject({ status: "waiting", problem: "rateLimited" });
    expect(h.client.snapshot().retryAt! - h.now()).toBe(RATE_LIMITED_DELAY);
    expect(h.data.get(TOKEN_KEY)).toBe("tok");
    h.advance(RATE_LIMITED_DELAY);
    h.last().open();
    h.last().push({ type: "error", code: "tooManySessions", message: "" });
    expect(h.client.snapshot()).toMatchObject({ status: "waiting", problem: "tooManyPhones" });
    expect(h.client.snapshot().retryAt! - h.now()).toBeGreaterThanOrEqual(8000);
  });

  test("an unsupported version stops for good", () => {
    const h = harness({ [TOKEN_KEY]: "tok" });
    h.client.start();
    h.last().open();
    h.last().push({ type: "error", code: "unsupportedVersion", message: "" });
    h.last().drop(4401);
    expect(h.client.snapshot()).toMatchObject({ status: "stopped", problem: "outdated" });
    h.advance(120_000);
    expect(h.sockets).toHaveLength(1);
  });

  test("sign-in that hangs is dropped and retried", () => {
    const h = harness({ [TOKEN_KEY]: "tok" });
    h.client.start();
    h.last().open();
    h.advance(HANDSHAKE_TIMEOUT);
    expect(h.sockets[0].closed).toBe(true);
    expect(h.client.snapshot().status).toBe("waiting");
  });

  test("pings while connected; a silent connection is dropped and retried", () => {
    const h = signedIn();
    h.advance(PING_INTERVAL);
    expect(h.last().sent).toEqual([{ type: "ping" }]);
    h.last().push({ type: "pong" });
    h.advance(PING_INTERVAL);
    expect(h.last().sent).toHaveLength(2);
    // no pong any more (Wi-Fi gone without a close)
    h.advance(SILENCE_LIMIT);
    expect(h.sockets[0].closed).toBe(true);
    // dropped after 30 s of silence, then a new attempt 0.5 s later
    expect(h.sockets).toHaveLength(2);
    expect(h.client.snapshot().status).toBe("connecting");
  });

  test("press, hold and release; multi-touch; nothing sent while offline", () => {
    const h = signedIn();
    expect(h.client.press(1, 0, 0)).toBe(true);
    expect(h.client.press(2, 0, 0)).toBe(true); // second finger on the same pad
    expect(h.client.press(3, 4, 4)).toBe(true);
    expect(h.client.snapshot().held.sort()).toEqual(["0,0", "4,4"]);
    h.client.release(1);
    h.client.release(3);
    h.client.release(2);
    h.client.release(2);
    expect(h.last().sent).toEqual([
      { type: "press", x: 0, y: 0 },
      { type: "press", x: 4, y: 4 },
      { type: "release", x: 4, y: 4 },
      { type: "release", x: 0, y: 0 },
    ]);
    h.last().drop();
    expect(h.client.press(1, 0, 0)).toBe(false);
  });

  test("releaseAll lets go of every held pad (page hidden, focus lost, touch cancelled)", () => {
    const h = signedIn();
    h.client.press(1, 0, 0);
    h.client.press(2, 1, 0);
    h.client.releaseAll();
    expect(h.last().sent.filter((m) => m.type === "release")).toEqual([
      { type: "release", x: 0, y: 0 },
      { type: "release", x: 1, y: 0 },
    ]);
    expect(h.client.snapshot().held).toEqual([]);
    h.client.release(1); // the finger's own lift afterwards sends nothing more
    expect(h.last().sent).toHaveLength(4);
  });

  test("refused commands show a notice without dropping the connection", () => {
    const h = signedIn();
    h.last().push({ type: "error", code: "busy", message: "" });
    expect(h.client.snapshot().notice?.code).toBe("busy");
    expect(h.client.snapshot().status).toBe("connected");
  });

  test("fader moves are paced and the last value always arrives", () => {
    const h = signedIn();
    h.client.setFader("f1", 0.1);
    h.client.setFader("f1", 0.2);
    h.client.setFader("f1", 0.3);
    expect(h.last().sent).toEqual([{ type: "setFader", faderId: "f1", value: 0.1 }]);
    expect(h.client.snapshot().faderDrafts.f1).toBe(0.3);
    h.advance(FADER_INTERVAL);
    expect(h.last().sent.at(-1)).toEqual({ type: "setFader", faderId: "f1", value: 0.3 });
    h.client.setFader("f1", 1.7, true); // clamped, sent at once on lift
    expect(h.last().sent.at(-1)).toEqual({ type: "setFader", faderId: "f1", value: 1 });
    expect(h.last().sent).toHaveLength(3);
    h.advance(1000);
    expect(h.client.snapshot().faderDrafts.f1).toBeUndefined();
    // ~20 messages a second per fader at most, well under the app's 120
    h.last().sent.length = 0;
    for (let i = 0; i < 100; i++) {
      h.client.setFader("f1", i / 100);
      h.advance(10);
    }
    expect(h.last().sent.length).toBeLessThanOrEqual(21);
  });

  test("page switch waits for the app's state", () => {
    const h = signedIn();
    h.client.switchPage("p1"); // already there
    expect(h.last().sent).toEqual([]);
    h.client.switchPage("p2");
    expect(h.last().sent).toEqual([{ type: "switchPage", pageId: "p2" }]);
    expect(h.client.snapshot().pendingPage).toBe("p2");
    h.last().push({ ...STATE, activePage: "p2" });
    expect(h.client.snapshot().pendingPage).toBeNull();
    h.client.switchPage("gone");
    h.last().push({ type: "error", code: "unknownPage", message: "" });
    expect(h.client.snapshot().pendingPage).toBeNull();
  });

  test("garbage from the socket is ignored", () => {
    const h = signedIn();
    h.last().onmessage?.({ data: "{not json" });
    h.last().onmessage?.({ data: new ArrayBuffer(2) });
    h.last().onmessage?.({ data: "null" });
    expect(h.client.snapshot().status).toBe("connected");
  });

  test("revoked while connected: the socket closes, the reconnect is refused, the phone shows it", () => {
    const h = signedIn();
    h.last().drop(1000);
    h.advance(500);
    h.last().open();
    h.last().push({ type: "error", code: "unauthorized", message: "" });
    h.last().drop(4401);
    expect(h.client.snapshot()).toMatchObject({ status: "unpaired", problem: "revoked" });
  });
});

// ---- view helpers ------------------------------------------------------------------------

const spec = (x: number, y: number, shape: PadSpec["shape"] = "pad", extra: Partial<PadSpec> = {}): PadSpec => ({ x, y, shape, region: "grid", label: null, note: 11, cc: false, led: "rgb", rows: 1, cols: 1, edge: "free", mask: false, centred: false, momentary: false, ...extra });

describe("view", () => {
  test("board geometry follows the desktop grid", () => {
    const layout: Layout = { model: "LaunchpadX", modelName: "X", width: 9, height: 9, rowWeights: Array(9).fill(1), pads: [spec(0, 0), spec(8, 8, "logo"), spec(7, 8, "empty"), spec(8, 0, "round")], limitedColor: false, velocitySensitive: true };
    const b = boardOf(layout);
    expect(b.columns).toBe(9);
    expect(b.aspect).toBe(1);
    expect(b.cells.map((p) => `${p.x},${p.y}`)).toEqual(["8,8", "0,0", "8,0"]);
    expect(fitBoard(1, 390, 500)).toEqual({ width: 390, height: 390 });
    expect(fitBoard(2, 390, 100)).toEqual({ width: 200, height: 100 });
    expect(fitBoard(1, 0, 100)).toEqual({ width: 0, height: 0 });
  });
  test("what a finger does on each control", () => {
    expect(padRole(spec(0, 0))).toBe("press");
    expect(padRole(spec(0, 0, "logo"))).toBe("none");
    expect(padRole(spec(0, 0, "knob"))).toBe("control");
    expect(padRole(spec(0, 0, "strip"))).toBe("control");
    expect(padRole(spec(0, 0, "rect", { note: null }))).toBe("none");
  });
  test("faces use the colours the app sends", () => {
    expect(faceOf({ mode: "off" }).off).toBe(true);
    expect(faceOf(undefined).off).toBe(true);
    const red = faceOf({ mode: "palette", index: 5, rgb: "#fc0000" });
    expect(red).toMatchObject({ off: false, bg: "rgb(252, 0, 0)", flashing: false, pulsing: false });
    const flash = faceOf({ mode: "flashing", index: 5, alt: 21, rgb: "#fc0000", altRgb: "#00fc00" });
    expect(flash).toMatchObject({ flashing: true, bg: "rgb(252, 0, 0)", alt: "rgb(0, 252, 0)" });
    expect(faceOf({ mode: "pulsing", index: 5, rgb: "#fc0000" }).pulsing).toBe(true);
    expect(faceOf({ mode: "rgb", rgb: "#ffffff" }).text).toBe("#101014");
    expect(faceOf({ mode: "rgb", rgb: "#000000" }).off).toBe(true);
  });
  test("fader level pad and strip fractions", () => {
    const f = { pads: [[0, 0], [0, 1], [0, 2], [0, 3]] as [number, number][], fraction: 0.5 };
    expect(levelStep(f)).toBe(2);
    expect(levelStep(f, 0)).toBe(0);
    expect(levelStep(f, 1)).toBe(3);
    expect(levelStep({ pads: [[0, 0]], fraction: 0.7 })).toBe(0);
    const box = { left: 10, top: 100, width: 200, height: 50 };
    expect(stripFraction({ x: 110, y: 0 }, box, "horizontal")).toBe(0.5);
    expect(stripFraction({ x: 0, y: 0 }, box, "horizontal")).toBe(0);
    expect(stripFraction({ x: 0, y: 150 }, box, "vertical")).toBe(0);
    expect(stripFraction({ x: 0, y: 90 }, box, "vertical")).toBe(1);
    const idx = indexState({ ...STATE, v: 1 });
    expect(idx.faderAt.get("8,2")).toMatchObject({ step: 2 });
    expect(idx.pads.get("0,0")?.color.mode).toBe("palette");
    expect(indexState(null).pads.size).toBe(0);
  });
});

test("QR code: a valid symbol with a quiet zone, the same for the same text", () => {
  const url = "http://192.168.1.5:7373/#pair=12345678";
  const a = qrPath(url);
  expect(a).toEqual(qrPath(url));
  expect((a.size - 8 - 17) % 4).toBe(0); // version size 17 + 4v, plus a 4-module border
  expect(a.path.startsWith("M4 4h7")).toBe(true); // the top-left finder pattern
  expect(qrPath("http://192.168.1.5:7373/#pair=87654321").path).not.toBe(a.path);
});
