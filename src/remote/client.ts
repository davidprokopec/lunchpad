/**
 * The phone page's connection to Lunchpad: pairing, signing in with the stored
 * token, the live pad state, and the few commands a phone may send (press,
 * release, fader, page). Pure TypeScript with the socket, storage and clock passed
 * in, so every path can be tested without a browser or a server.
 */
import type { RemoteClientMessage, RemoteServerMessage, RemoteState } from "../lib/api";
import { RATE_LIMITED_DELAY, reconnectDelay } from "./backoff";
import { padKeyOf, parsePadKey, PressTracker } from "./presses";

/** Protocol version this page speaks (`REMOTE_PROTOCOL_VERSION` in api.ts, kept out of the bundle's imports). */
export const PROTOCOL_VERSION = 1;

/** Where the phone's token and name are kept in the browser. */
export const TOKEN_KEY = "lunchpad-remote.token";
export const NAME_KEY = "lunchpad-remote.name";

/** Sign in must finish within this time, or the attempt counts as lost. */
export const HANDSHAKE_TIMEOUT = 10_000;
/** A ping is sent this often while connected… */
export const PING_INTERVAL = 10_000;
/** …and the connection counts as dead when nothing came back for this long. */
export const SILENCE_LIMIT = 25_000;
/** At most one fader message per fader this often (the app allows 120 messages a second in total). */
export const FADER_INTERVAL = 50;
/** How long a moved fader keeps showing the phone's value before the app's takes over again. */
export const FADER_SETTLE = 600;
/** How long a page switch may wait for the app before the request counts as lost. */
export const PAGE_TIMEOUT = 3_000;

export interface SocketLike {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: { code: number; reason: string }) => void) | null;
  onerror: ((ev: unknown) => void) | null;
}

export interface KeyValueStore {
  get(key: string): string | null;
  set(key: string, value: string): void;
  remove(key: string): void;
}

export interface ClientDeps {
  url: string;
  createSocket: (url: string) => SocketLike;
  storage: KeyValueStore;
  setTimeout: (fn: () => void, ms: number) => unknown;
  clearTimeout: (id: unknown) => void;
  now: () => number;
  random?: () => number;
}

/**
 * - `unpaired`: no token; the pairing form shows (with `problem` saying why, if any)
 * - `connecting`: opening the connection
 * - `signingIn`: pair/auth sent, waiting for the answer
 * - `connected`: signed in; pads work
 * - `waiting`: lost or refused for now; the next try is at `retryAt`
 * - `stopped`: nothing this page can fix (an older page than the app)
 */
export type ClientStatus = "unpaired" | "connecting" | "signingIn" | "connected" | "waiting" | "stopped";

export type ClientProblem =
  /** The stored token was refused: the phone was removed in Settings. */
  | "revoked"
  /** The typed code is not the one Lunchpad shows. */
  | "wrongCode"
  /** No code waiting: it was used, it expired, or none was made. */
  | "noCode"
  /** Too many wrong tries from this phone; wait a minute. */
  | "rateLimited"
  /** Lunchpad already has as many phones connected as it takes. */
  | "tooManyPhones"
  /** The page is older or newer than the app. */
  | "outdated"
  /** The connection dropped or could not be made. */
  | "lost";

/** Something a command bumped into; shown briefly. */
export type ClientNotice = "busy" | "noDevice" | "unknownPage" | "unknownFader" | "badRequest";

export interface ClientSnapshot {
  status: ClientStatus;
  problem: ClientProblem | null;
  /** Unix ms of the next try while `waiting`. */
  retryAt: number | null;
  /** Tries since the last good connection. */
  attempt: number;
  /** The last state from the app (kept while reconnecting, so the pads stay in view). */
  state: RemoteState | null;
  /** What the app calls this phone. */
  deviceName: string | null;
  /** A token is stored. */
  paired: boolean;
  notice: { code: ClientNotice; at: number } | null;
  /** Pads this phone holds ("x,y"). */
  held: string[];
  /** A page asked for and not yet shown. */
  pendingPage: string | null;
  /** Fader values the phone is moving, shown instead of the app's until it catches up. */
  faderDrafts: Record<string, number>;
}

type Mode = "pair" | "auth";

const OPEN = 1;

export class RemoteClient {
  private deps: ClientDeps;
  private random: () => number;
  private socket: SocketLike | null = null;
  private mode: Mode = "auth";
  private pairing: { code: string; name: string } | null = null;
  private presses = new PressTracker();
  private listeners = new Set<(s: ClientSnapshot) => void>();
  private timers = { retry: null as unknown, handshake: null as unknown, ping: null as unknown, page: null as unknown };
  private lastHeard = 0;
  private faders = new Map<string, { lastSent: number; sent: number | null; pending: number | null; timer: unknown; settle: unknown }>();
  private snap: ClientSnapshot;

  constructor(deps: ClientDeps) {
    this.deps = deps;
    this.random = deps.random ?? Math.random;
    const token = deps.storage.get(TOKEN_KEY);
    this.snap = {
      status: "unpaired",
      problem: null,
      retryAt: null,
      attempt: 0,
      state: null,
      deviceName: deps.storage.get(NAME_KEY),
      paired: !!token,
      notice: null,
      held: [],
      pendingPage: null,
      faderDrafts: {},
    };
  }

  snapshot(): ClientSnapshot {
    return this.snap;
  }

  subscribe(fn: (s: ClientSnapshot) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** Sign in with the stored token, or wait for pairing when there is none. */
  start(): void {
    if (this.deps.storage.get(TOKEN_KEY)) this.open("auth");
    else this.update({ status: "unpaired" });
  }

  /** Pair with a code from Lunchpad's Settings. */
  pair(code: string, name: string): void {
    this.pairing = { code, name };
    this.update({ problem: null });
    this.open("pair");
  }

  /** Try again now instead of waiting (the page came back to the front, or the user asked). */
  retryNow(): void {
    if (this.snap.status !== "waiting") return;
    this.open("auth");
  }

  /** Forget the token on this phone (the app still lists it until removed there). */
  forget(): void {
    this.deps.storage.remove(TOKEN_KEY);
    this.detach();
    this.clearTimers();
    this.update({ status: "unpaired", problem: null, retryAt: null, paired: false, state: null, held: [] });
  }

  /** Close for good (the page is going away). */
  destroy(): void {
    this.releaseAll();
    this.detach();
    this.clearTimers();
    this.listeners.clear();
  }

  // ---- commands --------------------------------------------------------------------------

  /** A finger lands on a pad. False when the pad cannot be pressed now (not connected). */
  press(pointer: number, x: number, y: number): boolean {
    if (this.snap.status !== "connected") return false;
    if (this.presses.down(pointer, padKeyOf(x, y))) this.send({ type: "press", x, y });
    this.update({ held: this.presses.held() });
    return true;
  }

  /** A finger lifts or is cancelled. */
  release(pointer: number): void {
    const key = this.presses.up(pointer);
    if (key === null) return;
    const { x, y } = parsePadKey(key);
    this.send({ type: "release", x, y });
    this.update({ held: this.presses.held() });
  }

  /** Let go of every pad this phone holds (the page hid or lost focus, the touch was cancelled). */
  releaseAll(): void {
    const keys = this.presses.releaseAll();
    for (const key of keys) {
      const { x, y } = parsePadKey(key);
      this.send({ type: "release", x, y });
    }
    if (keys.length) this.update({ held: [] });
  }

  /** Move a fader to `fraction` (0..1); `done` when the finger lifts. */
  setFader(faderId: string, fraction: number, done = false): void {
    if (this.snap.status !== "connected") return;
    const value = Math.round(Math.min(1, Math.max(0, Number.isFinite(fraction) ? fraction : 0)) * 1000) / 1000;
    let f = this.faders.get(faderId);
    if (!f) {
      f = { lastSent: -Infinity, sent: null, pending: null, timer: null, settle: null };
      this.faders.set(faderId, f);
    }
    const entry = f;
    if (entry.settle !== null) {
      this.deps.clearTimeout(entry.settle);
      entry.settle = null;
    }
    this.update({ faderDrafts: { ...this.snap.faderDrafts, [faderId]: value } });
    const flush = () => {
      entry.timer = null;
      if (entry.pending === null) return;
      const v = entry.pending;
      entry.pending = null;
      if (v === entry.sent) return;
      entry.sent = v;
      entry.lastSent = this.deps.now();
      this.send({ type: "setFader", faderId, value: v });
    };
    entry.pending = value;
    const wait = entry.lastSent + FADER_INTERVAL - this.deps.now();
    if (done || wait <= 0) {
      if (entry.timer !== null) this.deps.clearTimeout(entry.timer);
      flush();
    } else if (entry.timer === null) {
      entry.timer = this.deps.setTimeout(flush, wait);
    }
    if (done) {
      entry.settle = this.deps.setTimeout(() => {
        entry.settle = null;
        const drafts = { ...this.snap.faderDrafts };
        delete drafts[faderId];
        this.update({ faderDrafts: drafts });
      }, FADER_SETTLE);
    }
  }

  switchPage(pageId: string): void {
    if (this.snap.status !== "connected" || this.snap.state?.activePage === pageId) return;
    this.send({ type: "switchPage", pageId });
    if (this.timers.page !== null) this.deps.clearTimeout(this.timers.page);
    this.timers.page = this.deps.setTimeout(() => {
      this.timers.page = null;
      this.update({ pendingPage: null });
    }, PAGE_TIMEOUT);
    this.update({ pendingPage: pageId });
  }

  // ---- connection ------------------------------------------------------------------------

  private open(mode: Mode): void {
    this.detach();
    this.clearTimers();
    this.mode = mode;
    let socket: SocketLike;
    try {
      socket = this.deps.createSocket(this.deps.url);
    } catch {
      this.lost();
      return;
    }
    this.socket = socket;
    this.update({ status: "connecting", retryAt: null });
    this.timers.handshake = this.deps.setTimeout(() => {
      this.timers.handshake = null;
      if (this.snap.status === "connected") return;
      this.drop();
    }, HANDSHAKE_TIMEOUT);
    socket.onopen = () => {
      if (mode === "pair" && this.pairing) {
        const name = this.pairing.name;
        this.send({ type: "pair", v: PROTOCOL_VERSION, code: this.pairing.code, ...(name ? { name } : {}) });
      } else {
        const token = this.deps.storage.get(TOKEN_KEY);
        if (!token) {
          this.finish({ status: "unpaired", problem: null });
          return;
        }
        this.send({ type: "auth", v: PROTOCOL_VERSION, token });
      }
      this.update({ status: "signingIn" });
    };
    socket.onmessage = (ev) => this.receive(ev.data);
    socket.onerror = () => undefined;
    socket.onclose = () => this.closed();
  }

  private receive(data: unknown): void {
    if (typeof data !== "string") return;
    let msg: RemoteServerMessage;
    try {
      msg = JSON.parse(data) as RemoteServerMessage;
    } catch {
      return;
    }
    if (!msg || typeof msg !== "object") return;
    this.lastHeard = this.deps.now();
    switch (msg.type) {
      case "paired":
        this.deps.storage.set(TOKEN_KEY, msg.token);
        this.deps.storage.set(NAME_KEY, msg.name);
        this.pairing = null;
        this.mode = "auth";
        this.signedIn(msg.name);
        return;
      case "authOk":
        this.deps.storage.set(NAME_KEY, msg.name);
        this.signedIn(msg.name);
        return;
      case "state": {
        if (this.snap.status !== "connected") return;
        const { type: _type, ...state } = msg;
        const pendingPage = this.snap.pendingPage !== null && state.activePage === this.snap.pendingPage ? null : this.snap.pendingPage;
        if (pendingPage === null && this.timers.page !== null) {
          this.deps.clearTimeout(this.timers.page);
          this.timers.page = null;
        }
        this.update({ state, pendingPage });
        return;
      }
      case "pong":
        return;
      case "error":
        this.refused(msg.code);
        return;
    }
  }

  private signedIn(name: string): void {
    if (this.timers.handshake !== null) {
      this.deps.clearTimeout(this.timers.handshake);
      this.timers.handshake = null;
    }
    this.update({ status: "connected", problem: null, retryAt: null, attempt: 0, deviceName: name, paired: true });
    this.schedulePing();
  }

  private refused(code: string): void {
    if (this.snap.status === "connected") {
      if (code === "busy" || code === "noDevice" || code === "unknownPage" || code === "unknownFader" || code === "badRequest") {
        const patch: Partial<ClientSnapshot> = { notice: { code, at: this.deps.now() } };
        if (code === "unknownPage") patch.pendingPage = null;
        this.update(patch);
      }
      // timeout / tooManyMessages: the app closes the connection; `closed` reconnects.
      return;
    }
    // Refused while signing in: the app closes the connection right after; decide here.
    const pairing = this.mode === "pair";
    switch (code) {
      case "unauthorized":
        if (pairing) return this.finish({ status: "unpaired", problem: "wrongCode" });
        this.deps.storage.remove(TOKEN_KEY);
        return this.finish({ status: "unpaired", problem: "revoked", paired: false, state: null });
      case "noPairing":
        return this.finish({ status: "unpaired", problem: "noCode" });
      case "rateLimited":
        if (pairing) return this.finish({ status: "unpaired", problem: "rateLimited" });
        return this.retry(RATE_LIMITED_DELAY, "rateLimited");
      case "unsupportedVersion":
        return this.finish({ status: "stopped", problem: "outdated" });
      case "tooManySessions":
        return this.retry(Math.max(8000, reconnectDelay(this.snap.attempt, this.random())), "tooManyPhones");
      default:
        if (pairing) return this.finish({ status: "unpaired", problem: "lost" });
        return this.retry(reconnectDelay(this.snap.attempt, this.random()), "lost");
    }
  }

  /** The connection ended without a decision: try again (or, while pairing, let the user). */
  private closed(): void {
    this.detach();
    this.lost();
  }

  private lost(): void {
    this.clearTimers();
    // The app lets go of this phone's pads when the connection ends.
    this.presses.releaseAll();
    this.update({ held: [] });
    if (this.mode === "pair") return this.finish({ status: "unpaired", problem: "lost" });
    if (!this.deps.storage.get(TOKEN_KEY)) return this.finish({ status: "unpaired", problem: null });
    this.retry(reconnectDelay(this.snap.attempt, this.random()), "lost");
  }

  /** Close the socket on purpose (silent too long, sign-in took too long) and reconnect. */
  private drop(): void {
    const socket = this.socket;
    this.detach();
    try {
      socket?.close();
    } catch {
      // already closed
    }
    this.lost();
  }

  private retry(delay: number, problem: ClientProblem): void {
    this.detachAndClose();
    this.clearTimers();
    this.presses.releaseAll();
    const retryAt = this.deps.now() + delay;
    this.timers.retry = this.deps.setTimeout(() => {
      this.timers.retry = null;
      this.open("auth");
    }, delay);
    this.update({ status: "waiting", problem, retryAt, attempt: this.snap.attempt + 1, held: [], pendingPage: null });
  }

  private finish(patch: Partial<ClientSnapshot>): void {
    this.detachAndClose();
    this.clearTimers();
    this.presses.releaseAll();
    this.update({ retryAt: null, held: [], pendingPage: null, ...patch });
  }

  private schedulePing(): void {
    this.timers.ping = this.deps.setTimeout(() => {
      this.timers.ping = null;
      if (this.snap.status !== "connected") return;
      if (this.deps.now() - this.lastHeard > SILENCE_LIMIT) {
        this.drop();
        return;
      }
      this.send({ type: "ping" });
      this.schedulePing();
    }, PING_INTERVAL);
  }

  private send(msg: RemoteClientMessage): void {
    const socket = this.socket;
    if (!socket || socket.readyState !== OPEN) return;
    try {
      socket.send(JSON.stringify(msg));
    } catch {
      // The close handler takes it from here.
    }
  }

  private detach(): void {
    const socket = this.socket;
    if (!socket) return;
    socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
    this.socket = null;
  }

  private detachAndClose(): void {
    const socket = this.socket;
    this.detach();
    try {
      socket?.close();
    } catch {
      // already closed
    }
  }

  private clearTimers(): void {
    for (const key of Object.keys(this.timers) as (keyof typeof this.timers)[]) {
      if (this.timers[key] !== null) this.deps.clearTimeout(this.timers[key]);
      this.timers[key] = null;
    }
    for (const f of this.faders.values()) {
      if (f.timer !== null) this.deps.clearTimeout(f.timer);
      if (f.settle !== null) this.deps.clearTimeout(f.settle);
    }
    this.faders.clear();
    if (Object.keys(this.snap.faderDrafts).length) this.snap = { ...this.snap, faderDrafts: {} };
  }

  private update(patch: Partial<ClientSnapshot>): void {
    this.snap = { ...this.snap, ...patch };
    for (const fn of this.listeners) fn(this.snap);
  }
}
