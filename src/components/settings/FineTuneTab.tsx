import { openUrl } from "@tauri-apps/plugin-opener";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type FineTuneSettings } from "../../lib/api";
import { useMediaStore } from "../../store/media";
import { useSettingsStore } from "../../store/settings";
import { Button, Toggle } from "../ui";
import { Section } from "./SettingsDialog";

export function FineTuneTab() {
  const { t } = useTranslation();
  const confirmed = useSettingsStore((s) => s.settings?.fineTune);
  const desired = useSettingsStore((s) => s.fineTuneDesired);
  const pending = useSettingsStore((s) => s.fineTunePending);
  const saveError = useSettingsStore((s) => s.fineTuneError);
  const save = useSettingsStore((s) => s.setFineTune);
  const state = useMediaStore((s) => s.fineTune);
  const stateError = useMediaStore((s) => s.fineTuneError);
  const receive = useMediaStore((s) => s.receiveFineTune);
  const load = useMediaStore((s) => s.loadFineTune);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => { void load(); }, [load]);
  const config = desired ?? confirmed ?? { enabled: false, autoConnect: true };
  const disabled = pending || busy || !confirmed;
  const change = (value: Partial<FineTuneSettings>) => { void save(value).catch(() => undefined); };
  const command = async (run: typeof api.fineTuneConnect) => {
    setBusy(true); setError(null);
    try { receive(await run()); } catch (e) { setError(String(e)); }
    finally { setBusy(false); }
  };
  return <>
    <Section description={t("fineTune.intro")}>
      <Toggle checked={config.enabled} disabled={disabled} onChange={(enabled) => change({ enabled })} label={t("fineTune.enable")} hint={t("fineTune.optional")} />
      {pending && <p role="status" className="text-xs text-stage-400">{t("fineTune.saving")}</p>}
      {saveError && <p role="alert" className="text-xs text-danger">{t("fineTune.saveError", { error: saveError })}</p>}
    </Section>
    <Section title={t("fineTune.setup")}>
      <ol className="list-decimal space-y-2 pl-4 text-xs leading-relaxed text-stage-400"><li>{t("fineTune.install")}</li><li>{t("fineTune.controllerAccess")}</li><li>{t("fineTune.pickApps")}</li></ol>
      <Button size="sm" className="self-start" onClick={() => { void openUrl("https://github.com/ronitsingh10/FineTune").catch((e: unknown) => setError(String(e))); }}>{t("fineTune.download")}</Button>
      <p className="text-xs leading-relaxed text-stage-500">{t("fineTune.buildNote")}</p>
      {state?.status === "ready" && !state.capabilities?.includes("managedApps") && <p role="status" className="text-xs leading-relaxed text-stage-400">{t("fineTune.updateForInactive")}</p>}
      <div className="rounded-lg border border-warn/20 bg-warn/5 p-3 text-xs leading-relaxed text-stage-300">{t("fineTune.caution")}</div>
    </Section>
    {config.enabled && <>
      <Section title={t("fineTune.connection")} description={t("fineTune.localOnly")}>
        <Toggle checked={config.autoConnect} disabled={disabled} onChange={(autoConnect) => change({ autoConnect })} label={t("fineTune.autoConnect")} />
      </Section>
      <Section title={t("fineTune.statusTitle")}>
        <div className="rounded-lg border border-stage-700 bg-stage-800/50 p-3">
          <div className="flex items-center gap-2 text-sm text-stage-200" role="status"><span className={"h-2 w-2 rounded-full " + (state?.status === "ready" ? "bg-ok" : "bg-stage-500")} />{t(`fineTune.status.${state?.status ?? "disconnected"}`)}</div>
          <p className="mt-2 text-xs text-stage-400">{t("fineTune.appVersion", { version: state?.appVersion ?? t("fineTune.versionUnknown") })}</p>
          {(error || state?.error || stateError) && <p role="alert" className="mt-2 break-words text-xs text-warn">{error || state?.error || stateError}</p>}
          <div className="mt-3 flex gap-2">
            <Button size="sm" disabled={disabled} onClick={() => void command(api.fineTuneConnect)}>{t("fineTune.connect")}</Button>
            <Button size="sm" disabled={disabled} onClick={() => void command(api.fineTuneDisconnect)}>{t("fineTune.disconnect")}</Button>
            <Button size="sm" disabled={disabled || state?.status !== "ready"} onClick={() => void command(api.fineTuneRefresh)}>{t("fineTune.refresh")}</Button>
          </div>
        </div>
      </Section>
    </>}
    {!config.enabled && error && <p role="alert" className="text-xs text-warn">{error}</p>}
  </>;
}
