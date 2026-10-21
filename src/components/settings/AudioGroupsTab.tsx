import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { AudioGroup, Page } from "../../lib/api";
import { audioGroupUses, type AudioGroupUse } from "../../lib/audioGroups";
import { useProfileStore } from "../../store/profile";
import { AudioGroupEditor, AudioGroupStatus } from "../AudioGroupField";
import { Button } from "../ui";
import { Section } from "./SettingsDialog";

const NO_GROUPS: AudioGroup[] = [];
const NO_PAGES: Page[] = [];

/**
 * Settings → Audio groups: manage the current profile's group definitions in one
 * place. Uses the same editor, validation and commands as the fader picker; the
 * level/availability lines come only from confirmed group reports. Nothing here
 * writes audio.
 */
export function AudioGroupsTab() {
  const { t } = useTranslation();
  const groups = useProfileStore((s) => s.profile?.audioGroups) ?? NO_GROUPS;
  const pages = useProfileStore((s) => s.profile?.pages) ?? NO_PAGES;
  const [editing, setEditing] = useState<{ group: AudioGroup; key: number } | null>(null);
  const [pending, setPending] = useState(false);
  const edit = (group: AudioGroup) => setEditing((current) => ({ group, key: (current?.key ?? 0) + 1 }));
  const close = () => setEditing(null);
  const isNew = !!editing && !groups.some((group) => group.id === editing.group.id);
  const editor = editing && <AudioGroupEditor key={editing.key} initial={editing.group} onPendingChange={setPending} onSaved={close} onDeleted={close} onCancel={close} />;

  return (
    <Section title={t("audioGroupsTab.title")} description={t("audioGroupsTab.hint")}>
      <div className="flex max-w-lg flex-col gap-1 text-xs text-stage-400">
        <p>{t("audioGroupsTab.volumeOnly")}</p>
        <p>{t("audioGroupsTab.noAudio")}</p>
      </div>
      <div>
        <Button size="sm" disabled={pending} onClick={() => edit({ id: crypto.randomUUID(), name: "", members: [] })}>{t("audioGroupsTab.new")}</Button>
      </div>
      {isNew && editor}
      {groups.length === 0 && !isNew && <p className="text-xs text-stage-500">{t("audioGroupsTab.empty")}</p>}
      <ul className="flex flex-col gap-2" aria-label={t("settings.tabs.audioGroups")}>
        {groups.map((group) => {
          const uses = audioGroupUses(pages, group.id);
          const open = editing?.group.id === group.id;
          return (
            <li key={group.id} aria-label={group.name} className={"flex flex-col gap-3 rounded-lg border px-3 py-2.5 " + (open ? "border-accent-500/50 bg-stage-800/70" : "border-stage-700 bg-stage-800/40")}>
              <div className="flex items-start justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-baseline gap-2">
                    <span className="truncate text-sm font-medium text-stage-100">{group.name}</span>
                    <span className="shrink-0 text-xs text-stage-500">{t("audioGroupsTab.members", { count: group.members.length })}</span>
                  </div>
                </div>
                {!open && <Button size="sm" disabled={pending} aria-label={`${t("audioGroupsTab.edit")} ${group.name}`} onClick={() => edit(group)}>{t("audioGroupsTab.edit")}</Button>}
              </div>
              <AudioGroupStatus groupId={group.id} details />
              <div className="text-xs">
                <span className="text-stage-400">{t("audioGroupsTab.usedBy")}: </span>
                {uses.length === 0 ? <span className="text-stage-500">{t("audioGroupsTab.unused")}</span> : (
                  <ul className="mt-0.5 flex flex-col gap-0.5 text-stage-300">
                    {uses.map((use, index) => <li key={index}>{describeUse(use, t)}</li>)}
                  </ul>
                )}
                {uses.length > 0 && open && <p className="mt-1 text-stage-500">{t("audioGroupsTab.inUseNote")}</p>}
              </div>
              {open && editor}
            </li>
          );
        })}
      </ul>
    </Section>
  );
}

function describeUse(use: AudioGroupUse, t: (key: string, options?: Record<string, unknown>) => string): string {
  const options = { page: use.pageName, name: use.name };
  if (use.kind === "fader") return t(use.how === "bound" ? "audioGroupsTab.useFader" : "audioGroupsTab.useFaderAction", options);
  return t(use.how === "link" ? "audioGroupsTab.useButtonLink" : "audioGroupsTab.useButtonAction", options);
}
