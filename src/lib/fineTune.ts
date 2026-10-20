import type { FineTuneState, FineTuneTarget } from "./api";

/** Never resolve a persisted app ID to the first of several current processes. */
export function fineTuneApp(appId: string, state: FineTuneState | null) {
  const apps = state?.status === "ready" ? state.snapshot?.apps.filter((app) => app.appId === appId) ?? [] : [];
  return apps.length === 1 && apps[0].active ? apps[0] : null;
}

export function fineTuneReason(target: FineTuneTarget, state: FineTuneState | null): string | null {
  if (state?.status !== "ready" || !state.snapshot) return "notReady";
  if (target.type !== "app") return state.snapshot.devices.some((d) => d.uid === target.deviceUid && d.direction === target.type) ? null : "missingDevice";
  if (!target.appId) return "chooseApp";
  const apps = state.snapshot.apps.filter((app) => app.appId === target.appId);
  if (apps.length > 1) return "ambiguous";
  if (!apps.length || !apps[0].active) return "notRunning";
  if (state.snapshot.permission.audioCapture !== "authorized") return "capturePermission";
  return null;
}
