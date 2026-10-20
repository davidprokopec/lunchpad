import { useTranslation } from "react-i18next";
import { useMediaStore } from "../store/media";
import { waveLinkOutputKey } from "../lib/waveLink";
import { Select } from "./Select";

/** Only qualified discovered endpoints and nonempty discovered mixes can be chosen. */
export function WaveLinkRoutingField({ outputDeviceId, outputId, mixId, monitored, onChange }: {
  outputDeviceId: string; outputId: string; mixId?: string; monitored: boolean;
  onChange: (value: { outputDeviceId: string; outputId: string; mixId?: string }) => void;
}) {
  const { t } = useTranslation();
  const state = useMediaStore((s) => s.waveLink);
  const key = waveLinkOutputKey(outputDeviceId, outputId);
  const outputs = (state?.status === "ready" ? state.outputs : []).filter((output) => monitored ? output.canSetMix : output.canSelectMain);
  const found = outputs.find((output) => waveLinkOutputKey(output.outputDeviceId, output.outputId) === key);
  const options = outputs.map((output) => ({ value: waveLinkOutputKey(output.outputDeviceId, output.outputId), label: output.name }));
  if (!found && (outputDeviceId || outputId)) options.push({ value: key, label: t("audio.missingTarget", { name: key }) });
  const mixes = (state?.mixes ?? []).filter((mix) => !!mix.id).map((mix) => ({ value: mix.id, label: mix.name }));
  if (mixId && !mixes.some((mix) => mix.value === mixId)) mixes.push({ value: mixId, label: t("audio.missingTarget", { name: mixId }) });
  return <div className="flex flex-col gap-2">
    <div className="grid grid-cols-2 gap-3">
      <div className="flex min-w-0 flex-col gap-1 text-xs text-stage-400">{t("waveLink.output")}<Select size="sm" ariaLabel={t("waveLink.output")} value={outputId ? key : null} placeholder={t("waveLink.chooseTarget")} options={options} onChange={(next) => { const output = outputs.find((item) => waveLinkOutputKey(item.outputDeviceId, item.outputId) === next); if (output) onChange({ outputDeviceId: output.outputDeviceId, outputId: output.outputId, mixId }); }} /></div>
      {monitored && <div className="flex min-w-0 flex-col gap-1 text-xs text-stage-400">{t("waveLink.mix")}<Select size="sm" ariaLabel={t("waveLink.mix")} disabled={state?.status !== "ready"} value={mixId || null} options={mixes} placeholder={t("waveLink.chooseMix")} onChange={(mixId) => onChange({ outputDeviceId, outputId, mixId })} /></div>}
    </div>
    <p role="status" className="text-xs text-stage-500">{state?.status !== "ready" ? t("waveLink.notReady") : !found ? t("waveLink.chooseTarget") : t("waveLink.assignedMix", { mix: found.mixId === null ? t("waveLink.mixUnknown") : found.mixId === "" ? t("waveLink.unassigned") : state.mixes.find((mix) => mix.id === found.mixId)?.name ?? found.mixId })}</p>
    <p className="text-xs text-stage-500">{t("waveLink.routingHint")}</p>
  </div>;
}
