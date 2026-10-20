import { useTranslation } from "react-i18next";
import { SET_AUDIO_MODES, type Action, type Button, type SetAudioMode } from "../../lib/api";
import { AudioTargetField } from "../AudioTargetField";
import { Select } from "../Select";
import { Slider } from "../Slider";
import { useVariableSuggestions } from "./VariableFields";
import { VariableNameField } from "./VariableNameField";
import { useMediaStore } from "../../store/media";
import { audioControllable, audioTargetConfigured } from "../../lib/systemAudio";

export function SetAudioEditor({ action, onChange, button }: { action: Extract<Action, { type: "setAudio" }>; onChange: (next: Action) => void; button: Button }) {
  const { t } = useTranslation();
  const suggestions = useVariableSuggestions(button);
  const level = action.mode === "setVolume" || action.mode === "adjustVolume";
  const adjust = action.mode === "adjustVolume";
  const waveLink = useMediaStore((s) => s.waveLink);
  // Choose the operation before discovery: a blank volume target must not hide
  // Mute and trap the user away from mute-only inputs.
  const allowedModes = SET_AUDIO_MODES.filter((mode) => action.target.type !== "waveLink" || !audioTargetConfigured(action.target) || audioControllable(action.target, null, mode === "setVolume" || mode === "adjustVolume" ? "volume" : "mute", waveLink));
  return <div className="flex flex-col gap-3">
    <AudioTargetField value={action.target} control={level ? "volume" : "mute"} onChange={(target) => { if (target) onChange({ ...action, target }); }} />
    <div className="flex max-w-xs flex-col gap-1 text-xs text-stage-400">
      {t("volume.mode")}
      <Select<SetAudioMode> size="sm" ariaLabel={t("volume.mode")} value={action.mode} options={[...allowedModes, ...(!allowedModes.includes(action.mode) ? [action.mode] : [])].map((mode) => ({ value: mode, label: t(`audio.modes.${mode}`) + (!allowedModes.includes(mode) ? ` · ${t("audio.unavailableShort")}` : "") }))} onChange={(mode) => onChange({ ...action, mode, volume: mode === "adjustVolume" ? 5 : Math.max(0, action.volume) })} />
    </div>
    {level && <>
      <div className="grid grid-cols-[1fr_auto] items-end gap-3">
        <div className={"flex flex-col gap-1 text-xs text-stage-400 " + (action.volumeFrom ? "opacity-50" : "")}>
          {t(adjust ? "volume.step" : "volume.level")}
          <Slider value={Math.round(action.volume)} min={adjust ? -100 : 0} max={100} onChange={(volume) => onChange({ ...action, volume })} format={(v) => `${adjust && v >= 0 ? "+" : ""}${v} %`} ariaLabel={t(adjust ? "volume.step" : "volume.level")} />
        </div>
        <div className="flex w-48 flex-col gap-1 text-xs text-stage-400">
          {t("volume.fromVariable")}
          <VariableNameField ariaLabel={t("volume.fromVariable")} value={action.volumeFrom ?? ""} onChange={(value) => onChange({ ...action, volumeFrom: value || null })} suggestions={suggestions} placeholder={t("volume.fromPlaceholder")} />
        </div>
      </div>
      <p className="text-xs text-stage-500">{t(adjust ? "volume.adjustHint" : "volume.hint")}</p>
    </>}
    {action.mode === "holdMute" && <p className="text-xs text-stage-500">{t("audio.holdHint")}</p>}
  </div>;
}
