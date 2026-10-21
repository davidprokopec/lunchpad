import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import type { AudioTarget } from "../lib/api";
import { audioControllable, DEFAULT_INPUT, DEFAULT_OUTPUT, savedDeviceMatch } from "../lib/systemAudio";
import { useMediaStore } from "../store/media";
import { Select } from "./Select";
import { Button } from "./ui";
import { waveLinkControl, waveLinkKey } from "../lib/waveLink";
import { FineTuneAppField, FineTuneDeviceField } from "./FineTuneFields";
import { fineTuneReason } from "../lib/fineTune";
import { platformOptionAvailable } from "../lib/platform";
import { AudioGroupField } from "./AudioGroupField";
import { DeviceMatchNote } from "./DeviceMatchNote";

type Source = "manual" | "device" | "app" | "waveLink" | "fineTune" | "group";
const targetKey = (target: AudioTarget): string => target.type === "group" ? `group:${target.groupId}` : target.type === "fineTune" ? `fineTune:${JSON.stringify(target.target)}` : target.type === "waveLink" ? waveLinkKey(target.target) : target.type === "app" ? `app:${target.app.id}` : target.device.type === "named" ? `device:${target.device.kind}:${target.device.id || target.device.label}` : target.device.type;

/** The same explicit device/app picker for faders, actions and active-state links. */
export function AudioTargetField({ value, onChange, manual = false, control = "volume", allowGroups = true }: {
  value: AudioTarget | null | undefined;
  onChange: (value: AudioTarget | null) => void;
  manual?: boolean;
  control?: "volume" | "mute";
  allowGroups?: boolean;
}) {
  const { t } = useTranslation();
  const status = useMediaStore((s) => s.systemAudio);
  const error = useMediaStore((s) => s.systemAudioError);
  const refresh = useMediaStore((s) => s.loadSystemAudio);
  const waveLink = useMediaStore((s) => s.waveLink);
  const loadWaveLink = useMediaStore((s) => s.loadWaveLink);
  const fineTune = useMediaStore((s) => s.fineTune);
  const loadFineTune = useMediaStore((s) => s.loadFineTune);
  useEffect(() => { if (value?.type !== "fineTune") void refresh(); }, [refresh, value?.type]);
  const source: Source = value?.type ?? (manual ? "manual" : "device");
  // A saved named device in use through a driver or name fallback (exact matches need no note).
  const saved = value?.type === "device" && value.device.type === "named" ? value.device : null;
  const found = saved ? savedDeviceMatch(value, status) : null;
  const matched = saved && found && found.match !== "exact" ? found : null;
  const targets: { target: AudioTarget; label: string }[] = source === "fineTune" ? [] : source === "waveLink"
    ? (waveLink?.status === "ready" ? waveLink.controls : []).filter((item) => !allowGroups || (control === "volume" ? item.canSetLevel && item.level !== null : item.canMute && item.muted !== null)).map((item) => ({ target: { type: "waveLink", target: item.target }, label: `${t(`waveLink.kinds.${item.target.type}`)} · ${item.name}` }))
    : source === "app"
    ? (status?.apps ?? []).map((app) => ({ target: { type: "app", app: { id: app.id, name: app.name } }, label: app.present ? app.name : t("audio.notRunning", { name: app.name }) }))
    : [
      { target: DEFAULT_INPUT, label: t("audio.defaultInput") },
      { target: DEFAULT_OUTPUT, label: t("audio.defaultOutput") },
      ...(status?.devices ?? [])
        // The report keeps a not-connected entry for the saved id; hide it while that id resolves to a live device.
        .filter((d) => !(matched && saved && !d.present && d.kind === saved.kind && d.id === saved.id))
        .map((d) => ({ target: { type: "device" as const, device: { type: "named" as const, id: d.id, kind: d.kind, label: d.name } }, label: t(!d.present ? "audio.missingDevice" : matched && saved && d.present && d.id === matched.id && d.kind === saved.kind ? (matched.match === "label" ? "audio.matchedLabelShort" : "audio.matchedDriverShort") : "audio.namedDevice", { name: d.name, kind: t(`audio.${d.kind}`) }) })),
    ];
  if (value && value.type !== "group" && !matched && !targets.some((item) => targetKey(item.target) === targetKey(value))) {
    const name = value.type === "fineTune" ? "FineTune" : value.type === "waveLink" ? waveLinkControl(value.target, waveLink)?.name ?? waveLinkKey(value.target) : value.type === "app" ? value.app.name : value.device.type === "named" ? value.device.label : "";
    targets.push({ target: value, label: t(value.type === "app" ? "audio.notRunning" : "audio.missingTarget", { name }) });
  }
  const unavailable = value && !audioControllable(value, status, control, waveLink, fineTune);
  const selected = value?.type === "waveLink" ? waveLinkControl(value.target, waveLink) : null;
  const channelId = value?.type === "waveLink" && (value.target.type === "channel" || value.target.type === "channelMix") ? value.target.channelId : null;
  const members = waveLink?.channels.find((channel) => channel.id === channelId)?.apps ?? [];
  const reason = source === "fineTune" ? t(value?.type === "fineTune" && value.target.type !== "app" ? "fineTune.deviceLinksOnly" : `fineTune.${value?.type === "fineTune" ? fineTuneReason(value.target, fineTune) ?? "chooseApp" : "chooseApp"}`) : source === "waveLink" ? waveLink?.status !== "ready" ? t("waveLink.notReady") : selected?.target.type === "inputGain" ? t("waveLink.gainReadOnly") : selected?.reason || t("waveLink.unsupportedControl") : status?.message || (value?.type === "app" && (status?.permission === "denied" || status?.permission === "unconfirmed")
    ? t("audio.permission", { status: t(`audio.permissions.${status.permission}`) })
    : t(status ? "audio.unavailable" : "audio.loading"));
  return (
    <div className="flex flex-col gap-2">
      <div className="grid grid-cols-2 items-end gap-3">
        <div className="flex flex-col gap-1 text-xs text-stage-400">
          {t(manual ? "audio.source" : "audio.target")}
          <Select<Source> size="sm" ariaLabel={t(manual ? "audio.source" : "audio.target")} value={source} placeholder={source === "fineTune" ? `${t("fineTune.name")} · ${t("audio.unavailableShort")}` : undefined} options={[
            ...(manual ? [{ value: "manual" as const, label: t("audio.manual") }] : []),
            { value: "device", label: t(manual ? "audio.deviceVolume" : "volume.device") },
            { value: "app", label: t(manual ? "audio.appVolume" : "audio.app") },
            { value: "waveLink", label: t("waveLink.name") },
            ...(platformOptionAvailable("fineTune") ? [{ value: "fineTune" as const, label: t("fineTune.name") }] : []),
            ...(allowGroups && (control === "volume" || value?.type === "group") ? [{ value: "group" as const, label: "Audio group · volume only" }] : []),
          ]} onChange={(next) => { if (next === "group") { onChange({ type: "group", groupId: "" }); } else if (next === "fineTune") { void loadFineTune(); onChange({ type: "fineTune", target: { type: "app", appId: "" } }); } else if (next === "waveLink") { void loadWaveLink(); onChange({ type: "waveLink", target: { type: "channel", channelId: "" } }); } else onChange(next === "manual" ? null : next === "device" ? DEFAULT_OUTPUT : { type: "app", app: { id: "", name: "" } }); }} />
        </div>
        {source === "fineTune" ? <div className="flex min-w-0 flex-col gap-2">
          {!allowGroups && platformOptionAvailable("fineTune") && <Select size="sm" ariaLabel="FineTune member kind" value={value?.type === "fineTune" ? value.target.type : "app"} options={[{ value: "app", label: "App" }, { value: "output", label: "Output · read-only" }, { value: "input", label: "Input · read-only" }]} onChange={(type) => onChange({ type: "fineTune", target: type === "app" ? { type, appId: "" } : { type, deviceUid: "" } })} />}
          {value?.type === "fineTune" && value.target.type !== "app" ? <FineTuneDeviceField selection={false} direction={value.target.type} value={value.target.deviceUid} label={value.target.label} onChange={(deviceUid, label) => { if (value.target.type !== "app") onChange({ type: "fineTune", target: { type: value.target.type, deviceUid, ...(label ? { label } : {}) } }); }} /> : <FineTuneAppField value={value?.type === "fineTune" && value.target.type === "app" ? value.target.appId : ""} onChange={(appId) => onChange({ type: "fineTune", target: { type: "app", appId } })} />}
        </div> : source !== "manual" && source !== "group" && <div className="flex min-w-0 flex-col gap-1 text-xs text-stage-400">
          {t(source === "waveLink" ? "waveLink.control" : source === "app" ? "audio.app" : "volume.device")}
          <Select size="sm" ariaLabel={t(source === "waveLink" ? "waveLink.control" : source === "app" ? "audio.app" : "volume.device")} value={matched ? targetKey(matched.update) : value && (value.type !== "app" || value.app.id) ? targetKey(value) : null} placeholder={t("waveLink.chooseTarget")} options={targets.filter(({ target }) => target.type !== "app" || !!target.app.id).map(({ target, label }) => ({ value: targetKey(target), label }))} onChange={(key) => { const item = targets.find(({ target }) => targetKey(target) === key); if (item) onChange(item.target); }} />
        </div>}
      </div>
      {matched && <DeviceMatchNote match={matched.match} name={matched.name} onUpdate={() => onChange(matched.update)} />}
      {source === "group" && <AudioGroupField groupId={value?.type === "group" ? value.groupId : ""} volumeOnly={control === "volume"} onChange={(groupId) => onChange({ type: "group", groupId })} />}
      {source !== "manual" && source !== "group" && <div className="flex items-start justify-between gap-3">
        <p role="status" className={"text-xs " + (unavailable || error ? "text-warn" : "text-stage-500")}>
          {(source === "waveLink" || source === "fineTune" ? null : error) || (unavailable && (source !== "fineTune" || value?.type === "fineTune" && value.target.type !== "app") ? reason : "")}
        </p>
        <Button size="sm" onClick={() => void (source === "fineTune" ? loadFineTune() : source === "waveLink" ? loadWaveLink() : refresh())}>{t("audio.retry")}</Button>
      </div>}
      {channelId && <p className="text-xs text-stage-500">{t("waveLink.groupedApps", { apps: members.map((app) => app.name).join(", ") || t("waveLink.noApps") })}</p>}
      {selected?.gain && <p className="text-xs text-stage-500">{t("waveLink.gainMetadata", selected.gain)}</p>}
    </div>
  );
}
