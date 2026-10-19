import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useMediaStore } from "../../store/media";
import { useSettingsStore } from "../../store/settings";
import { Select } from "../Select";
import { Button, Toggle } from "../ui";
import { Section } from "./SettingsDialog";

const SYSTEM_DEFAULT = "__system__";
const isMac = navigator.platform.toLowerCase().includes("mac");

export function SoundTab() {
  const { t } = useTranslation();
  const settings = useSettingsStore((s) => s.settings);
  const setAudio = useSettingsStore((s) => s.setAudio);
  const audioDevices = useMediaStore((s) => s.audioDevices);
  const loadAudioDevices = useMediaStore((s) => s.loadAudioDevices);
  const status = useMediaStore((s) => s.systemAudio);
  const refresh = useMediaStore((s) => s.loadSystemAudio);
  const [error, setError] = useState<string | null>(null);
  // The control remains disabled until the backend exposes and persists this key.
  const audio = settings?.audio as (NonNullable<typeof settings>["audio"] & { macosAppVolume?: boolean }) | undefined;
  const supported = typeof audio?.macosAppVolume === "boolean";

  useEffect(() => {
    void loadAudioDevices().catch(() => undefined);
  }, [loadAudioDevices]);

  return (
    <><Section title={t("settings.audioDevice")} description={t("settings.audioDeviceHint")}>
      <div className="flex max-w-lg items-center gap-2">
        <Select
          className="flex-1"
          value={settings?.audio.outputDevice ?? SYSTEM_DEFAULT}
          options={[
            { value: SYSTEM_DEFAULT, label: audioDevices?.default ? `${t("settings.systemDefaultDevice")} (${audioDevices.default})` : t("settings.systemDefaultDevice") },
            ...(audioDevices?.devices ?? []).map((d) => ({ value: d, label: d })),
          ]}
          onChange={(v) => void setAudio({ ...settings?.audio, outputDevice: v === SYSTEM_DEFAULT ? null : v })}
        />
        <Button size="sm" onClick={() => void loadAudioDevices()}>
          {t("settings.refreshDevices")}
        </Button>
      </div>
    </Section>
    {isMac && <Section title={t("audio.macTitle")} description={t("audio.macHint")}>
      <Toggle checked={audio?.macosAppVolume ?? false} disabled={!supported} label={t("audio.macToggle")} hint={!supported ? t("audio.macUnavailable") : undefined} onChange={(macosAppVolume) => {
        if (!audio || !supported) return;
        const config = { ...audio, macosAppVolume };
        setError(null);
        void setAudio(config).then(refresh).catch((reason) => setError(String(reason)));
      }} />
      <div className="mt-3 flex items-start justify-between gap-3">
        <div className="flex flex-col gap-1 text-xs text-stage-400" role="status">
          <p>{t("audio.permission", { status: t(`audio.permissions.${status?.permission ?? "unknown"}`) })}</p>
          {status?.message && <p className="text-warn">{status.message}</p>}
          {error && <p className="text-danger">{error}</p>}
          <p className="text-stage-500">{t("audio.retryHint")}</p>
        </div>
        <Button size="sm" onClick={() => void refresh()}>{t("audio.retry")}</Button>
      </div>
    </Section>}</>
  );
}
