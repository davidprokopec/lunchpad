import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import type { AudioTarget } from "../lib/api";
import { audioControllable, DEFAULT_INPUT, DEFAULT_OUTPUT } from "../lib/systemAudio";
import { useMediaStore } from "../store/media";
import { Select } from "./Select";
import { Button } from "./ui";

type Source = "manual" | "device" | "app" | "waveLink";
const targetKey = (target: AudioTarget): string => target.type === "waveLink" ? JSON.stringify(target) : target.type === "app" ? `app:${target.app.id}` : target.device.type === "named" ? `device:${target.device.kind}:${target.device.id || target.device.label}` : target.device.type;

/** The same explicit device/app picker for faders, actions and active-state links. */
export function AudioTargetField({ value, onChange, manual = false, control = "volume" }: {
  value: AudioTarget | null | undefined;
  onChange: (value: AudioTarget | null) => void;
  manual?: boolean;
  control?: "volume" | "mute";
}) {
  const { t } = useTranslation();
  const status = useMediaStore((s) => s.systemAudio);
  const error = useMediaStore((s) => s.systemAudioError);
  const refresh = useMediaStore((s) => s.loadSystemAudio);
  useEffect(() => { void refresh(); }, [refresh]);
  const source: Source = value?.type ?? (manual ? "manual" : "device");
  const targets: { target: AudioTarget; label: string }[] = source === "app"
    ? (status?.apps ?? []).map((app) => ({ target: { type: "app", app: { id: app.id, name: app.name } }, label: app.present ? app.name : t("audio.notRunning", { name: app.name }) }))
    : [
      { target: DEFAULT_INPUT, label: t("audio.defaultInput") },
      { target: DEFAULT_OUTPUT, label: t("audio.defaultOutput") },
      ...(status?.devices ?? []).map((d) => ({ target: { type: "device" as const, device: { type: "named" as const, id: d.id, kind: d.kind, label: d.name } }, label: t(d.present ? "audio.namedDevice" : "audio.missingDevice", { name: d.name, kind: t(`audio.${d.kind}`) }) })),
    ];
  if (value && !targets.some((item) => targetKey(item.target) === targetKey(value))) {
    const name = value.type === "waveLink" ? "Wave Link" : value.type === "app" ? value.app.name : value.device.type === "named" ? value.device.label : "";
    targets.push({ target: value, label: t(value.type === "app" ? "audio.notRunning" : "audio.missingTarget", { name }) });
  }
  const unavailable = value && !audioControllable(value, status, control);
  const reason = status?.message || (value?.type === "app" && (status?.permission === "denied" || status?.permission === "unconfirmed")
    ? t("audio.permission", { status: t(`audio.permissions.${status.permission}`) })
    : t(status ? "audio.unavailable" : "audio.loading"));
  return (
    <div className="flex flex-col gap-2">
      <div className="grid grid-cols-2 items-end gap-3">
        <div className="flex flex-col gap-1 text-xs text-stage-400">
          {t(manual ? "audio.source" : "audio.target")}
          <Select<Source> size="sm" ariaLabel={t(manual ? "audio.source" : "audio.target")} value={source} options={[
            ...(manual ? [{ value: "manual" as const, label: t("audio.manual") }] : []),
            { value: "device", label: t(manual ? "audio.deviceVolume" : "volume.device") },
            { value: "app", label: t(manual ? "audio.appVolume" : "audio.app") },
            ...(value?.type === "waveLink" ? [{ value: "waveLink" as const, label: "Wave Link" }] : []),
          ]} onChange={(next) => { if (next !== "waveLink") onChange(next === "manual" ? null : next === "device" ? DEFAULT_OUTPUT : { type: "app", app: { id: "", name: "" } }); }} />
        </div>
        {source !== "manual" && <div className="flex min-w-0 flex-col gap-1 text-xs text-stage-400">
          {t(source === "app" ? "audio.app" : "volume.device")}
          <Select size="sm" ariaLabel={t(source === "app" ? "audio.app" : "volume.device")} value={value && (value.type !== "app" || value.app.id) ? targetKey(value) : null} placeholder={t("audio.chooseApp")} options={targets.filter(({ target }) => target.type !== "app" || !!target.app.id).map(({ target, label }) => ({ value: targetKey(target), label }))} onChange={(key) => { const item = targets.find(({ target }) => targetKey(target) === key); if (item) onChange(item.target); }} />
        </div>}
      </div>
      {source !== "manual" && <div className="flex items-start justify-between gap-3">
        <p role="status" className={"text-xs " + (unavailable || error ? "text-warn" : "text-stage-500")}>
          {error || (unavailable ? reason : "")}
        </p>
        <Button size="sm" onClick={() => void refresh()}>{t("audio.retry")}</Button>
      </div>}
    </div>
  );
}
