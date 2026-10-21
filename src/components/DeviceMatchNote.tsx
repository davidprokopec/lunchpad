import { Cable, TriangleAlert } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { DeviceMatch } from "../lib/deviceIdentity";
import { Button } from "./ui";

/**
 * Shown under a saved device that is in use through a fallback match.
 * A driver match is the same device under another driver, so it gets a quiet
 * note. A name match may be a different physical device, so it gets a visible
 * caution. "Update to current device" only edits the draft; nothing is saved
 * or sent to audio until the editor's own Save.
 */
export function DeviceMatchNote({ match, name, onUpdate, blocked }: { match: DeviceMatch; name: string; onUpdate: () => void; blocked?: string }) {
  const { t } = useTranslation();
  if (match === "exact") return null;
  const label = match === "label";
  const Icon = label ? TriangleAlert : Cable;
  return <div role="status" data-device-match={match} className={label
    ? "flex flex-col gap-2 rounded-lg border border-warn/40 bg-warn/10 px-2.5 py-2 text-xs text-warn"
    : "flex flex-col gap-1.5 rounded-lg border border-stage-700 bg-stage-900/60 px-2.5 py-2 text-xs text-stage-300"}>
    <p className="flex items-start gap-2">
      <Icon aria-hidden className="mt-px size-3.5 shrink-0" strokeWidth={2.25} />
      <span className="min-w-0">
        <span className={label ? "font-semibold" : "font-medium text-stage-200"}>{t(label ? "audio.matchedLabel" : "audio.matchedDriver", { name })}</span>
        {" "}<span className={label ? "text-warn/80" : "text-stage-400"}>{t(label ? "audio.matchedLabelHint" : "audio.matchedDriverHint")}</span>
      </span>
    </p>
    <div className="flex flex-wrap items-center gap-2 pl-5.5">
      <Button size="sm" className={label ? "bg-warn/15 text-warn hover:bg-warn/25" : undefined} disabled={!!blocked} onClick={onUpdate}>{t("audio.updateToCurrent")}</Button>
      {blocked && <span className={label ? "text-warn/80" : "text-stage-400"}>{blocked}</span>}
    </div>
  </div>;
}
