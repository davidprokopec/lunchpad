import { openUrl } from "@tauri-apps/plugin-opener";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type WaveLinkSettings } from "../../lib/api";
import { waveLinkKey } from "../../lib/waveLink";
import { useMediaStore } from "../../store/media";
import { useSettingsStore } from "../../store/settings";
import { Button, Toggle } from "../ui";
import { fieldCls, Section } from "./SettingsDialog";

export function WaveLinkTab() {
  const { t } = useTranslation();
  const confirmed = useSettingsStore((s) => s.settings?.waveLink);
  const desired = useSettingsStore((s) => s.waveLinkDesired);
  const pending = useSettingsStore((s) => s.waveLinkPending);
  const saveError = useSettingsStore((s) => s.waveLinkError);
  const save = useSettingsStore((s) => s.setWaveLink);
  const state = useMediaStore((s) => s.waveLink);
  const stateError = useMediaStore((s) => s.waveLinkError);
  const receive = useMediaStore((s) => s.receiveWaveLink);
  const load = useMediaStore((s) => s.loadWaveLink);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [portDraft, setPortDraft] = useState<string | null>(null);
  useEffect(() => { void load(); }, [load]);
  const config = desired ?? confirmed ?? { enabled: false, autoConnect: true, port: null };
  const disabled = pending || busy || !confirmed;
  const change = (value: Partial<WaveLinkSettings>) => { void save(value).catch(() => undefined); };
  const draftPort = portDraft !== null && /^\d+$/.test(portDraft) ? Number(portDraft) : NaN;
  const validPort = Number.isInteger(draftPort) && draftPort >= 1 && draftPort <= 65535;
  const commitPort = async () => {
    if (disabled || !validPort) return;
    if (draftPort === confirmed?.port && !saveError) { setPortDraft(null); return; }
    try { await save({ port: draftPort }); setPortDraft(null); }
    catch { /* Keep the draft available for an explicit retry; the store shows the error. */ }
  };
  const command = async (run: typeof api.waveLinkConnect) => {
    setBusy(true); setError(null);
    try { receive(await run()); } catch (e) { setError(String(e)); }
    finally { setBusy(false); }
  };
  return <>
    <Section description={t("waveLink.intro")}>
      <Toggle checked={config.enabled} disabled={disabled} onChange={(enabled) => change({ enabled })} label={t("waveLink.enable")} hint={t("waveLink.optional")} />
      {pending && <p role="status" className="text-xs text-stage-400">{t("waveLink.saving")}</p>}
      {saveError && <p role="alert" className="text-xs text-danger">{t("waveLink.saveError", { error: saveError })}</p>}
    </Section>
    <Section title={t("waveLink.setup")}>
      <ol className="list-decimal space-y-2 pl-4 text-xs leading-relaxed text-stage-400">
        <li>{t("waveLink.install")}</li><li>{t("waveLink.permissions")}</li><li>{t("waveLink.routing")}</li>
      </ol>
      <Button size="sm" className="self-start" onClick={() => { void openUrl("https://www.elgato.com/downloads").catch((e: unknown) => setError(String(e))); }}>{t("waveLink.download")}</Button>
      <div className="rounded-lg border border-warn/20 bg-warn/5 p-3 text-xs leading-relaxed text-stage-300">{t("waveLink.caution")}</div>
    </Section>
    {config.enabled && <>
      <Section title={t("waveLink.connection")} description={t("waveLink.localOnly")}>
        <Toggle checked={config.autoConnect} disabled={disabled} onChange={(autoConnect) => change({ autoConnect })} label={t("waveLink.autoConnect")} />
        <Toggle checked={config.port === null} disabled={disabled} onChange={(automatic) => { setPortDraft(null); change({ port: automatic ? null : 1884 }); }} label={t("waveLink.automaticPort")} hint={t("waveLink.portHint")} />
        {config.port !== null && <div className="flex flex-col gap-2">
          <div className="flex items-end gap-2">
            <label className="flex max-w-36 flex-col gap-1 text-xs text-stage-400">{t("waveLink.port")}<input type="text" inputMode="numeric" value={portDraft ?? String(config.port)} disabled={disabled} aria-invalid={portDraft !== null && !validPort} aria-describedby="wave-link-port-hint" onChange={(e) => setPortDraft(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); void commitPort(); } }} className={fieldCls} /></label>
            <Button size="sm" disabled={disabled || !validPort} onClick={() => void commitPort()}>{t("waveLink.applyPort")}</Button>
          </div>
          <p id="wave-link-port-hint" role="status" className={"text-xs " + (portDraft !== null && !validPort ? "text-warn" : "text-stage-500")}>{t(portDraft !== null && !validPort ? "waveLink.invalidPort" : "waveLink.portDraftHint")}</p>
        </div>}
      </Section>
      <Section title={t("waveLink.statusTitle")}>
        <div className="rounded-lg border border-stage-700 bg-stage-800/50 p-3">
          <div className="flex items-center gap-2 text-sm text-stage-200" role="status"><span className={"h-2 w-2 rounded-full " + (state?.status === "ready" ? "bg-ok" : "bg-stage-500")} />{t(`waveLink.status.${state?.status ?? "disconnected"}`)}</div>
          {state?.application && <p className="mt-2 text-xs text-stage-400">{t("waveLink.interface", { name: state.application.name, revision: state.application.interfaceRevision, os: state.application.operatingSystem, port: state.port, version: state.version ?? t("waveLink.versionUnknown") })}</p>}
          {(state?.error || stateError || error) && <p role="alert" className="mt-2 break-words text-xs text-warn">{state?.error || stateError || error}</p>}
          <div className="mt-3 flex gap-2">
            <Button size="sm" disabled={disabled} onClick={() => void command(api.waveLinkConnect)}>{t("waveLink.connect")}</Button>
            <Button size="sm" disabled={disabled} onClick={() => void command(api.waveLinkDisconnect)}>{t("waveLink.disconnect")}</Button>
            <Button size="sm" disabled={disabled || state?.status !== "ready"} onClick={() => void command(api.waveLinkRefresh)}>{t("waveLink.refresh")}</Button>
          </div>
        </div>
      </Section>
      <Section title={t("waveLink.channels")} description={t("waveLink.channelHint")}>
        {state?.status === "ready" && state.channels.map((channel) => <div key={channel.id} className="border-l-2 border-stage-700 pl-3"><p className="text-sm text-stage-200">{channel.name}</p><p className="mt-1 text-xs text-stage-400">{t("waveLink.groupedApps", { apps: channel.apps.map((app) => app.name).join(", ") || t("waveLink.noApps") })}</p></div>)}
        <p className="text-xs text-stage-500">{t("waveLink.gainReadOnly")}</p>
        {state?.status === "ready" && state.controls.filter((control) => control.gain).map((control) => <p key={waveLinkKey(control.target)} className="text-xs text-stage-400">{control.name} · {t("waveLink.gainMetadata", { ...control.gain })}</p>)}
      </Section>
    </>}
    {!config.enabled && error && <p role="alert" className="text-xs text-warn">{error}</p>}
  </>;
}
