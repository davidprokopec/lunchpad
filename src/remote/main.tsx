/**
 * Entry of the phone page (`remote.html`), served by Lunchpad's phone remote on the
 * local network. No Tauri here: it talks to the app over `ws://<host>/ws` only.
 */
import React from "react";
import ReactDOM from "react-dom/client";
import "../index.css";
import "./remote.css";
import { RemoteClient, type KeyValueStore, type SocketLike } from "./client";
import { readPairCode } from "./pairing";
import { RemoteApp } from "./RemoteApp";

/** localStorage, or memory when the browser refuses it (private mode): pairing then lasts until the tab closes. */
function browserStorage(): KeyValueStore {
  const memory = new Map<string, string>();
  const ls = (() => {
    try {
      const probe = "lunchpad-remote.probe";
      window.localStorage.setItem(probe, "1");
      window.localStorage.removeItem(probe);
      return window.localStorage;
    } catch {
      return null;
    }
  })();
  return {
    get: (key) => (ls ? ls.getItem(key) : (memory.get(key) ?? null)),
    set: (key, value) => {
      try {
        if (ls) ls.setItem(key, value);
        else memory.set(key, value);
      } catch {
        memory.set(key, value);
      }
    },
    remove: (key) => {
      try {
        ls?.removeItem(key);
      } catch {
        // nothing stored
      }
      memory.delete(key);
    },
  };
}

// The code from the QR link is read once and taken out of the address bar, so it is not
// kept in history or bookmarks.
const linkCode = readPairCode(window.location.hash);
if (window.location.hash) window.history.replaceState(null, "", window.location.pathname + window.location.search);

const client = new RemoteClient({
  url: `${window.location.protocol === "https:" ? "wss" : "ws"}://${window.location.host}/ws`,
  createSocket: (url) => new WebSocket(url) as unknown as SocketLike,
  storage: browserStorage(),
  setTimeout: (fn, ms) => window.setTimeout(fn, ms),
  clearTimeout: (id) => window.clearTimeout(id as number),
  now: () => Date.now(),
});

// A pad must never stay held because the phone looked away: let go when the page hides,
// loses focus or the system cancels the touches (a call, a notification, the app switcher).
const letGo = () => client.releaseAll();
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "hidden") letGo();
  else client.retryNow();
});
window.addEventListener("blur", letGo);
window.addEventListener("pagehide", letGo);
window.addEventListener("touchcancel", letGo);
window.addEventListener("online", () => client.retryNow());
// No pinch zoom or long-press menu while playing.
for (const type of ["gesturestart", "gesturechange"]) document.addEventListener(type, (e) => e.preventDefault());
document.addEventListener("contextmenu", (e) => {
  if (!(e.target instanceof HTMLInputElement)) e.preventDefault();
});

client.start();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <RemoteApp client={client} linkCode={linkCode} />
  </React.StrictMode>,
);
