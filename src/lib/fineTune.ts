import { fineTuneAppCatalogue, type FineTuneState, type FineTuneTarget } from "./api";

/** Never resolve a persisted app ID to the first of several current processes. */
export function fineTuneApp(appId: string, state: FineTuneState | null) {
  const apps = state?.status === "ready" ? fineTuneAppCatalogue(state).filter((app) => app.appId === appId) : [];
  return apps.length === 1 && (state?.capabilities?.includes("managedApps") || apps[0].active) ? apps[0] : null;
}

export function fineTuneReason(target: FineTuneTarget, state: FineTuneState | null): string | null {
  if (state?.status !== "ready" || !state.snapshot) return "notReady";
  if (target.type !== "app") return state.snapshot.devices.some((d) => d.uid === target.deviceUid && d.direction === target.type) ? null : "missingDevice";
  if (!target.appId) return "chooseApp";
  const apps = fineTuneAppCatalogue(state).filter((app) => app.appId === target.appId);
  if (apps.length > 1) return "ambiguous";
  if (!apps.length) return "missingApp";
  if (!state.capabilities?.includes("managedApps") && !apps[0].active) return "notRunning";
  if (state.snapshot.permission.audioCapture !== "authorized") return "capturePermission";
  if (state.capabilities?.includes("managedApps") && (!("manageable" in apps[0]) || apps[0].manageable !== true)) return "ignoredApp";
  return null;
}

/** Playback status is information, never a gain/mute admission failure. */
export function fineTunePlayback(appId: string, state: FineTuneState | null) {
  const app = fineTuneApp(appId, state);
  return !app || app.active ? null : "running" in app && app.running === true ? "notPlaying" : "notRunning";
}

export function fineTuneRoutingReason(appId: string, state: FineTuneState | null): string | null {
  const reason = fineTuneReason({ type: "app", appId }, state);
  if (reason) return reason;
  const app = fineTuneApp(appId, state);
  if (!app?.active || app.pids.length !== 1 || app.pids[0] <= 0) return "routingInactive";
  return app.routing.requested.mode === "multi" ? "multiReadOnly" : null;
}
