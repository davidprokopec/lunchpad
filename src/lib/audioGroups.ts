import type { AudioGroup, AudioGroupReport, AudioMemberTarget } from "./api";
import { audioTargetConfigured, aggregateGroup } from "./systemAudio";

/** Labels are decoration, not endpoint identity. Keep every qualified parent ID. */
export function audioMemberKey(member: AudioMemberTarget): string {
  if (member.type === "app") return JSON.stringify([member.type, member.app.id]);
  if (member.type === "device") return JSON.stringify(member.device.type === "named" ? [member.type, member.device.type, member.device.kind, member.device.id] : [member.type, member.device.type]);
  const target = member.target;
  return JSON.stringify([member.type, ...Object.keys(target).sort().map((key) => [key, target[key as keyof typeof target]])]);
}

export function audioGroupValidation(group: AudioGroup): string | null {
  if (!group.id.trim()) return "This group needs an ID.";
  if (!group.name.trim()) return "Give this group a name.";
  if (group.members.length < 1 || group.members.length > 32) return "Choose 1–32 members.";
  if (group.members.some((member) => !audioTargetConfigured(member) || member.type === "device" && member.device.type === "named" && !member.device.id?.trim() || member.type === "app" && !member.app.id.trim() || (member.type === "fineTune" || member.type === "waveLink") && Object.values(member.target).some((value) => !value.trim()))) return "Choose a complete target for every member.";
  if (new Set(group.members.map(audioMemberKey)).size !== group.members.length) return "Each member can only appear once.";
  return null;
}

/** Full-array publications remove absent groups, while old per-group revisions cannot regress. */
export function mergeAudioGroupReports(current: AudioGroupReport[], incoming: AudioGroupReport[]): AudioGroupReport[] {
  return incoming.map((report) => {
    const previous = current.find((item) => item.groupId === report.groupId);
    return previous && previous.revision >= report.revision ? previous : report;
  });
}

export function audioGroupFeedback(report: AudioGroupReport | undefined, compact = false): string {
  if (!report) return compact ? "Unavailable" : "Unavailable · waiting for observations";
  const s = aggregateGroup(report.members);
  const percent = (value: number) => `${Math.round(value * 100)}%`;
  if (compact) return [s.volume === null ? "Unavailable" : `${s.mixed ? "Mixed " : ""}${percent(s.volume)}`, s.coverage === "partial" ? "Partial" : null, report.busy ? "Pending" : null, !report.members.some((member) => member.canSetVolume) && s.readable ? "Read-only" : null, report.operation?.members.some((member) => member.outcome === "failed" || member.outcome === "unconfirmed") ? "Check result" : null].filter(Boolean).join(" · ");
  return [s.volume === null ? "Unavailable" : s.mixed ? `Mixed · ${percent(s.min!)}–${percent(s.max!)} · mean ${percent(s.volume)}` : percent(s.volume),
    s.coverage === "partial" ? `Partial · ${s.readable}/${report.members.length} readable` : null,
    s.mute === "all" ? "All muted" : `${s.muted} muted${s.unknownMute ? ` · ${s.unknownMute} unknown mute` : ""}`,
    report.busy ? "Operation pending" : null,
    !report.members.some((member) => member.canSetVolume) && s.readable ? "Read-only" : null,
    ...["failed", "unconfirmed", "skipped"].flatMap((outcome) => {
      const count = report.operation?.members.filter((member) => member.outcome === outcome).length ?? 0;
      return count ? [`${count} ${outcome}`] : [];
    }),
  ].filter(Boolean).join(" · ");
}
