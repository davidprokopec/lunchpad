import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { fineTuneAppCatalogue, type Action } from "../lib/api";
import { fineTuneApp, fineTuneDevice, fineTunePlayback, fineTuneReason, fineTuneRoutingReason } from "../lib/fineTune";
import { useMediaStore } from "../store/media";
import { Select } from "./Select";
import { isMacOS } from "../lib/platform";
import { DeviceMatchNote } from "./DeviceMatchNote";

export function FineTuneAppField({ value, onChange }: { value: string; onChange: (appId: string) => void }) {
  const { t } = useTranslation();
  const state = useMediaStore((s) => s.fineTune);
  const load = useMediaStore((s) => s.loadFineTune);
  useEffect(() => { if (isMacOS()) void load(); }, [load]);
  if (!isMacOS()) return <p role="status">{value} · {t("fineTune.status.unsupported")}</p>;
  const apps = state?.status === "ready" ? fineTuneAppCatalogue(state) : [];
  const options = apps.map((app, index) => {
    const ambiguous = apps.filter((row) => row.appId === app.appId).length > 1;
    return { value: ambiguous ? JSON.stringify([app.appId, app.pids, index]) : app.appId,
      label: ambiguous ? `${app.name} · ${app.pids.join(", ")} · ${t("fineTune.ambiguousShort")}` : app.active ? app.name : `${app.name} · ${t("fineTune." + ("running" in app && app.running === true ? "notPlaying" : "notRunning"))}`,
      hint: ambiguous ? t("fineTune.ambiguous") : app.appId,
      appId: app.appId, ambiguous };
  });
  if (value && !options.some((option) => option.value === value)) options.push({ value, label: `${value} · ${t(apps.some((app) => app.appId === value) ? "fineTune.ambiguousShort" : "audio.unavailableShort")}`, hint: value, appId: value, ambiguous: true });
  const reason = fineTuneReason({ type: "app", appId: value }, state);
  const playback = fineTunePlayback(value, state);
  return <div className="flex min-w-0 flex-col gap-1 text-xs text-stage-400">
    {t("fineTune.app")}
    <Select size="sm" ariaLabel={t("fineTune.app")} value={value || null} placeholder={t("fineTune.chooseApp")} options={options} onChange={(key) => { const option = options.find((row) => row.value === key); if (option && !option.ambiguous) onChange(option.appId); }} />
    {reason && <p role="status" className="text-xs text-warn">{t(`fineTune.${reason}`)}</p>}
    {playback && <p role="status">{t(`fineTune.${playback}`)} · {t("fineTune.nextPlayback")}</p>}
    {state?.status === "ready" && !state.capabilities?.includes("managedApps") && <p role="status">{t("fineTune.updateForInactive")}</p>}
  </div>;
}

/**
 * Device selection/state only. FineTune does not expose device level writes.
 * `onChange` also reports the chosen device's name, saved as the fallback `label`.
 * A saved UID that resolves by driver swap or label is shown as the live device
 * it resolves to, with a note and an explicit "Update to current device".
 */
export function FineTuneDeviceField({ value, label, direction, onChange, selection = true }: { value: string; label?: string; direction: "input" | "output"; onChange: (deviceUid: string, label: string | undefined) => void; selection?: boolean }) {
  const { t } = useTranslation();
  const state = useMediaStore((s) => s.fineTune);
  const load = useMediaStore((s) => s.loadFineTune);
  useEffect(() => { if (isMacOS()) void load(); }, [load]);
  if (!isMacOS()) return <p role="status">{value} · {t("fineTune.status.unsupported")}</p>;
  const devices = state?.status === "ready" ? state.snapshot?.devices.filter((d) => d.direction === direction && (!selection || d.capabilities.selectable)) ?? [] : [];
  const found = value ? fineTuneDevice({ type: direction, deviceUid: value, label }, state) : null;
  // A fallback-matched device is shown as the live one it resolves to, never as a stale "unavailable" UID.
  const resolved = found && devices.includes(found.device) ? found : null;
  const shown = resolved ? resolved.device.uid : value;
  const options = devices.map((d) => ({ value: d.uid, label: resolved?.device === d && resolved.match !== "exact" ? `${d.name} · ${t(resolved.match === "label" ? "fineTune.matchedLabelShort" : "fineTune.matchedDriverShort")}` : d.name }));
  if (shown && !options.some((option) => option.value === shown)) options.push({ value: shown, label: `${shown} · ${t("audio.unavailableShort")}` });
  return <div className="flex min-w-0 flex-col gap-1 text-xs text-stage-400">
    {t(`fineTune.${direction}`)}
    <Select size="sm" ariaLabel={t(`fineTune.${direction}`)} value={shown || null} options={options} placeholder={t("fineTune.chooseDevice")} onChange={(uid) => onChange(uid, devices.find((d) => d.uid === uid)?.name ?? (uid === value ? label : undefined))} />
    {(state?.status !== "ready" || (value && !resolved)) && <p role="status" className="text-xs text-warn">{t(state?.status !== "ready" ? "fineTune.notReady" : "fineTune.missingDevice")}</p>}
    {resolved && <DeviceMatchNote match={resolved.match} name={resolved.device.name} onUpdate={() => onChange(resolved.device.uid, resolved.device.name)} />}
  </div>;
}

export function FineTuneRoutingEditor({ action, onChange }: { action: Extract<Action, { type: "fineTuneSetAppRouting" }>; onChange: (action: Action) => void }) {
  const { t } = useTranslation();
  const state = useMediaStore((s) => s.fineTune);
  const app = fineTuneApp(action.appId, state);
  const reason = fineTuneRoutingReason(action.appId, state);
  if (!isMacOS()) return <p role="status">{action.appId} · {t("fineTune.status.unsupported")}</p>;
  return <div className="flex flex-col gap-3">
    <FineTuneAppField value={action.appId} onChange={(appId) => onChange({ ...action, appId })} />
    <fieldset disabled={!!reason} className="flex flex-col gap-3 disabled:opacity-60">
      <div className="flex flex-col gap-1 text-xs text-stage-400">{t("fineTune.routing")}
        <Select size="sm" ariaLabel={t("fineTune.routing")} value={action.routing.mode} options={[{ value: "followDefault", label: t("fineTune.followDefault") }, { value: "single", label: t("fineTune.specificOutput") }]} onChange={(mode) => onChange({ ...action, routing: mode === "followDefault" ? { mode } : { mode: "single", deviceUID: "" } })} />
      </div>
      {action.routing.mode === "single" && <FineTuneDeviceField direction="output" value={action.routing.deviceUID} onChange={(deviceUID) => onChange({ ...action, routing: { mode: "single", deviceUID } })} />}
    </fieldset>
    {reason && <p role="status" className="text-xs text-warn">{t(`fineTune.${reason}`)}</p>}
    <p className="text-xs text-stage-500">{t("fineTune.routingHint")}</p>
    {app && <p role="status" className="text-xs text-stage-400">{t("fineTune.routingStatus", { status: t(`fineTune.routingStates.${app.routing.status}`) })}{app.routing.error ? ` · ${app.routing.error.message}` : ""}</p>}
  </div>;
}
