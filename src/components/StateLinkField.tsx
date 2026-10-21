import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { LINK_KINDS, type LinkKind, type LinkProvider, type StateLink } from "../lib/api";
import { NamePicker, ProviderStatus, useProviderView } from "./actions/ObsEditors";
import { Select } from "./Select";
import { useSettingsStore } from "../store/settings";
import { AudioTargetField } from "./AudioTargetField";
import { DEFAULT_INPUT } from "../lib/systemAudio";
import { WaveLinkRoutingField } from "./WaveLinkRoutingField";
import { FineTuneDeviceField } from "./FineTuneFields";
import { platformOptionAvailable } from "../lib/platform";

type ProviderChoice = LinkProvider | "none";

/** "Active while …" picker: app, state, and the scene / source it refers to. */
export function StateLinkField({ value, onChange }: { value: StateLink | null; onChange: (next: StateLink | null) => void }) {
  const { t } = useTranslation();
  // Only OBS and Streamlabs have scene / source lists to load.
  const view = useProviderView(value?.provider === "slobs" ? "slobs" : "obs");
  const { loaded, load } = view;

  useEffect(() => {
    if (value && (value.provider === "obs" || value.provider === "slobs") && !loaded) void load();
  }, [value, loaded, load]);

  const obsEnabled = useSettingsStore((s) => s.settings?.obs.enabled ?? false);
  const slobsEnabled = useSettingsStore((s) => s.settings?.slobs.enabled ?? false);
  const providerOptions: { value: ProviderChoice; label: string }[] = [
    { value: "none", label: t("editor.linkNone") },
    { value: "system", label: t("audio.system") },
    { value: "waveLink", label: t("waveLink.name") },
    ...(platformOptionAvailable("fineTune") ? [{ value: "fineTune" as const, label: t("fineTune.name") }] : []),
    ...(obsEnabled || value?.provider === "obs" ? [{ value: "obs" as const, label: t("integrations.obs") }] : []),
    ...(slobsEnabled || value?.provider === "slobs" ? [{ value: "slobs" as const, label: t("integrations.slobs") }] : []),
  ];

  return (
    <div className="flex flex-col gap-3">
      <div className="grid grid-cols-2 gap-3">
        <div className="flex flex-col gap-1 text-xs text-stage-400">
          {t("editor.linkApp")}
          <Select<ProviderChoice>
            size="sm"
            value={value ? value.provider : "none"}
            placeholder={value?.provider === "fineTune" ? `${t("fineTune.name")} · ${t("audio.unavailableShort")}` : undefined}
            options={providerOptions}
            onChange={(v) => onChange(v === "none" ? null : v === "system" || v === "waveLink" || v === "fineTune" ? { provider: v, kind: "audioMuted", scene: "", source: "", target: v === "system" ? DEFAULT_INPUT : v === "fineTune" ? { type: "fineTune", target: { type: "app", appId: "" } } : { type: "waveLink", target: { type: "channel", channelId: "" } } } : { provider: v, kind: "scene", scene: "", source: "" })}
          />
        </div>
        {value && (
          <div className="flex flex-col gap-1 text-xs text-stage-400">
            {t("editor.linkKind")}
            <Select<LinkKind> size="sm" value={value.kind} options={(value.provider === "system" ? ["audioMuted" as const] : value.provider === "waveLink" ? ["audioMuted", "waveLinkMainOutput", "waveLinkMonitoredMix"] as const : value.provider === "fineTune" ? ["audioMuted", "fineTuneDefaultOutput", "fineTunePreferredInput", "fineTuneDefaultInput"] as const : LINK_KINDS.filter((kind) => kind !== "audioMuted" && !kind.startsWith("waveLink") && !kind.startsWith("fineTune"))).map((k) => ({ value: k, label: t(k === "audioMuted" ? "audio.muted" : k.startsWith("fineTune") ? `fineTune.links.${k}` : k.startsWith("waveLink") ? `waveLink.links.${k}` : `editor.linkKinds.${k}`) }))} onChange={(kind) => onChange({ ...value, kind, ...(value.provider === "fineTune" ? { target: { type: "fineTune", target: kind === "audioMuted" ? { type: "app", appId: "" } : { type: kind === "fineTuneDefaultOutput" ? "output" : "input", deviceUid: "" } } } : {}) })} />
          </div>
        )}
      </div>
      {value && (value.kind === "scene" || value.kind === "sourceVisible") && (
        <div className="grid grid-cols-2 gap-3">
          <NamePicker
            view={view}
            label={t("obs.scene")}
            value={value.scene}
            list={view.scenes}
            onChange={(scene) => onChange({ ...value, scene })}
            allowEmpty={value.kind === "sourceVisible" ? t("editor.linkCurrentScene") : undefined}
          />
          {value.kind === "sourceVisible" && <NamePicker view={view} label={t("obs.source")} value={value.source} list={view.sources} onChange={(source) => onChange({ ...value, source })} />}
        </div>
      )}
      {value && value.kind === "sourceMuted" && (
        <div className="max-w-xs">
          <NamePicker view={view} label={t("obs.source")} value={value.source} list={view.audioSources} onChange={(source) => onChange({ ...value, source })} />
        </div>
      )}
      {(value?.provider === "system" || value?.provider === "waveLink" || value?.provider === "fineTune") && value.kind === "audioMuted" && <AudioTargetField control="mute" value={value.target ?? DEFAULT_INPUT} onChange={(target) => onChange({ ...value, target, provider: target?.type === "fineTune" ? "fineTune" : target?.type === "waveLink" ? "waveLink" : "system" })} />}
      {value?.provider === "fineTune" && value.kind !== "audioMuted" && <FineTuneDeviceField selection={false} direction={value.kind === "fineTuneDefaultOutput" ? "output" : "input"} value={value.target?.type === "fineTune" && value.target.target.type !== "app" ? value.target.target.deviceUid : ""} label={value.target?.type === "fineTune" && value.target.target.type !== "app" ? value.target.target.label : undefined} onChange={(deviceUid, label) => onChange({ ...value, target: { type: "fineTune", target: { type: value.kind === "fineTuneDefaultOutput" ? "output" : "input", deviceUid, ...(label ? { label } : {}) } } })} />}
      {value?.provider === "waveLink" && (value.kind === "waveLinkMainOutput" || value.kind === "waveLinkMonitoredMix") && <WaveLinkRoutingField outputDeviceId={value.target?.type === "waveLink" && value.target.target.type === "output" ? value.target.target.outputDeviceId : ""} outputId={value.target?.type === "waveLink" && value.target.target.type === "output" ? value.target.target.outputId : ""} mixId={value.waveLinkMixId} monitored={value.kind === "waveLinkMonitoredMix"} onChange={(next) => onChange({ ...value, target: { type: "waveLink", target: { type: "output", outputDeviceId: next.outputDeviceId, outputId: next.outputId } }, waveLinkMixId: next.mixId })} />}
      {value && (value.provider === "obs" || value.provider === "slobs") && <ProviderStatus view={view} />}
    </div>
  );
}
