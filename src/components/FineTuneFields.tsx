import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import type { Action } from "../lib/api";
import { fineTuneApp, fineTuneReason } from "../lib/fineTune";
import { useMediaStore } from "../store/media";
import { Select } from "./Select";
import { isMacOS } from "../lib/platform";

export function FineTuneAppField({ value, onChange }: { value: string; onChange: (appId: string) => void }) {
  const { t } = useTranslation();
  const state = useMediaStore((s) => s.fineTune);
  const load = useMediaStore((s) => s.loadFineTune);
  useEffect(() => { if (isMacOS()) void load(); }, [load]);
  if (!isMacOS()) return <p role="status">{value} · {t("fineTune.status.unsupported")}</p>;
  const apps = state?.status === "ready" ? state.snapshot?.apps ?? [] : [];
  const options = apps.map((app, index) => {
    const ambiguous = apps.filter((row) => row.appId === app.appId).length > 1;
    return { value: ambiguous ? JSON.stringify([app.appId, app.pids, index]) : app.appId,
      label: ambiguous ? `${app.name} · ${app.pids.join(", ")} · ${t("fineTune.ambiguousShort")}` : app.name,
      hint: ambiguous ? t("fineTune.ambiguous") : app.appId,
      appId: app.appId, ambiguous };
  });
  if (value && !options.some((option) => option.value === value)) options.push({ value, label: `${value} · ${t(apps.some((app) => app.appId === value) ? "fineTune.ambiguousShort" : "audio.unavailableShort")}`, hint: value, appId: value, ambiguous: true });
  const reason = fineTuneReason({ type: "app", appId: value }, state);
  return <div className="flex min-w-0 flex-col gap-1 text-xs text-stage-400">
    {t("fineTune.app")}
    <Select size="sm" ariaLabel={t("fineTune.app")} value={value || null} placeholder={t("fineTune.chooseApp")} options={options} onChange={(key) => { const option = options.find((row) => row.value === key); if (option && !option.ambiguous) onChange(option.appId); }} />
    {reason && <p role="status" className="text-xs text-warn">{t(`fineTune.${reason}`)}</p>}
  </div>;
}

/** Device selection/state only. FineTune does not expose device level writes. */
export function FineTuneDeviceField({ value, direction, onChange, selection = true }: { value: string; direction: "input" | "output"; onChange: (deviceUid: string) => void; selection?: boolean }) {
  const { t } = useTranslation();
  const state = useMediaStore((s) => s.fineTune);
  const load = useMediaStore((s) => s.loadFineTune);
  useEffect(() => { if (isMacOS()) void load(); }, [load]);
  if (!isMacOS()) return <p role="status">{value} · {t("fineTune.status.unsupported")}</p>;
  const devices = state?.status === "ready" ? state.snapshot?.devices.filter((d) => d.direction === direction && (!selection || d.capabilities.selectable)) ?? [] : [];
  const options = devices.map((d) => ({ value: d.uid, label: d.name }));
  if (value && !options.some((option) => option.value === value)) options.push({ value, label: `${value} · ${t("audio.unavailableShort")}` });
  return <div className="flex min-w-0 flex-col gap-1 text-xs text-stage-400">
    {t(`fineTune.${direction}`)}
    <Select size="sm" ariaLabel={t(`fineTune.${direction}`)} value={value || null} options={options} placeholder={t("fineTune.chooseDevice")} onChange={onChange} />
    {(state?.status !== "ready" || (value && !devices.some((d) => d.uid === value))) && <p role="status" className="text-xs text-warn">{t(state?.status !== "ready" ? "fineTune.notReady" : "fineTune.missingDevice")}</p>}
  </div>;
}

export function FineTuneRoutingEditor({ action, onChange }: { action: Extract<Action, { type: "fineTuneSetAppRouting" }>; onChange: (action: Action) => void }) {
  const { t } = useTranslation();
  const state = useMediaStore((s) => s.fineTune);
  const app = fineTuneApp(action.appId, state);
  if (!isMacOS()) return <p role="status">{action.appId} · {t("fineTune.status.unsupported")}</p>;
  return <div className="flex flex-col gap-3">
    <FineTuneAppField value={action.appId} onChange={(appId) => onChange({ ...action, appId })} />
    <div className="flex flex-col gap-1 text-xs text-stage-400">{t("fineTune.routing")}
      <Select size="sm" ariaLabel={t("fineTune.routing")} value={action.routing.mode} options={[{ value: "followDefault", label: t("fineTune.followDefault") }, { value: "single", label: t("fineTune.specificOutput") }]} onChange={(mode) => onChange({ ...action, routing: mode === "followDefault" ? { mode } : { mode: "single", deviceUID: "" } })} />
    </div>
    {action.routing.mode === "single" && <FineTuneDeviceField direction="output" value={action.routing.deviceUID} onChange={(deviceUID) => onChange({ ...action, routing: { mode: "single", deviceUID } })} />}
    <p className="text-xs text-stage-500">{t("fineTune.routingHint")}</p>
    {app?.routing.requested.mode === "multi" && <p role="status" className="text-xs text-warn">{t("fineTune.multiReadOnly")}</p>}
    {app && <p role="status" className="text-xs text-stage-400">{t("fineTune.routingStatus", { status: t(`fineTune.routingStates.${app.routing.status}`) })}{app.routing.error ? ` · ${app.routing.error.message}` : ""}</p>}
  </div>;
}
