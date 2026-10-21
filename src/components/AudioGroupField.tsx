import { useState } from "react";
import { api, fineTuneAppCatalogue, type AudioGroup, type AudioMemberTarget } from "../lib/api";
import { audioGroupFeedback, audioGroupValidation, audioMemberKey } from "../lib/audioGroups";
import { useMediaStore } from "../store/media";
import { useProfileStore } from "../store/profile";
import { AudioTargetField } from "./AudioTargetField";
import { Select } from "./Select";
import { Button } from "./ui";

const EMPTY_GROUPS: AudioGroup[] = [];

export function AudioGroupStatus({ groupId, details = false }: { groupId: string; details?: boolean }) {
  const memberLabel = useMemberLabel();
  const report = useMediaStore((s) => s.audioGroups.find((item) => item.groupId === groupId));
  const error = useMediaStore((s) => s.audioGroupsError);
  return <div role="status" className="flex flex-col gap-1 text-xs text-stage-400">
    <p className="font-medium text-stage-200">{audioGroupFeedback(report)}</p>
    {error && <p className="text-warn">{error}</p>}
    {details && report?.members.map((member) => <p key={audioMemberKey(member.target)} className={member.reason ? "text-warn" : "text-stage-500"}>
      {memberLabel(member.target)} · {member.volume === null ? "Level unknown" : `${Math.round(member.volume * 100)}%`} · {member.muted === null ? "Mute unknown" : member.muted ? "Muted" : "Unmuted"}{!member.canSetVolume ? " · Read-only / unavailable for volume" : ""}{member.reason ? ` · ${member.reason}` : ""}
    </p>)}
    {details && report?.operation?.members.filter((member) => member.outcome !== "confirmed").map((member) => <p key={audioMemberKey(member.target)} className="text-warn">{memberLabel(member.target)} · {member.outcome}{member.message ? ` · ${member.message}` : ""}</p>)}
  </div>;
}

export function useMemberLabel() {
  const fineTune = useMediaStore((s) => s.fineTune);
  const waveLink = useMediaStore((s) => s.waveLink);
  return (member: AudioMemberTarget): string => {
    if (member.type === "app") return member.app.name || member.app.id;
    if (member.type === "device") return member.device.type === "named" ? member.device.label || member.device.id || "Device" : member.device.type === "defaultInput" ? "Default input" : "Default output";
    if (member.type === "fineTune" && member.target.type === "app") {
      const id = member.target.appId;
      const app = fineTune && fineTuneAppCatalogue(fineTune).find((app) => app.appId === id);
      return `FineTune · ${app?.name ?? id}${app ? "" : " · Saved / unavailable"}`;
    }
    if (member.type === "fineTune" && member.target.type !== "app") {
      const uid = member.target.deviceUid;
      return `FineTune · ${fineTune?.snapshot?.devices.find((device) => device.uid === uid)?.name ?? uid} · Read-only`;
    }
    const key = audioMemberKey(member);
    const name = waveLink?.controls.find((control) => audioMemberKey({ type: "waveLink", target: control.target }) === key)?.name;
    return `Wave Link · ${name ?? Object.values(member.target).join(" · ")}`;
  };
}

/**
 * Editor for one group definition: name, explicit members, save and (for saved
 * groups) delete. Shared by fader/action pickers and Settings → Audio groups.
 * Definition edits never send audio commands; only explicit fader/action input does.
 */
export function AudioGroupEditor({ initial, onSaved, onDeleted, onCancel, onPendingChange }: {
  initial: AudioGroup;
  onSaved: (groupId: string) => void;
  onDeleted: (groupId: string) => void;
  onCancel: () => void;
  onPendingChange?: (pending: boolean) => void;
}) {
  const memberLabel = useMemberLabel();
  const groups = useProfileStore((s) => s.profile?.audioGroups) ?? EMPTY_GROUPS;
  const [draft, setDraft] = useState<AudioGroup>(() => ({ ...initial, members: [...initial.members] }));
  const [candidate, setCandidate] = useState<AudioMemberTarget | null>(null);
  const [pending, setPendingState] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const setPending = (value: boolean) => { setPendingState(value); onPendingChange?.(value); };
  const saved = groups.some((item) => item.id === draft.id);
  const validation = audioGroupValidation(draft);
  const duplicate = !!candidate && draft.members.some((member) => audioMemberKey(member) === audioMemberKey(candidate));
  const candidateError = candidate ? audioGroupValidation({ id: "candidate", name: "candidate", members: [candidate] }) : "Choose a target.";
  const run = async (operation: () => Promise<void>, done: () => void) => {
    setPending(true); setError(null);
    try { await operation(); done(); }
    catch (error) { const message = String(error); setError(message); useProfileStore.setState({ error: message }); }
    finally { setPending(false); }
  };
  return <div className="flex flex-col gap-3">
    <fieldset disabled={pending} className="flex min-w-0 flex-col gap-3 rounded-xl border border-stage-700 bg-stage-950/40 p-3">
      <div className="flex items-center justify-between"><h3 className="text-sm font-semibold text-stage-100">{saved ? "Edit audio group" : "New audio group"}</h3><span className="text-xs text-stage-500">{draft.members.length}/32 members</span></div>
      <label className="flex flex-col gap-1 text-xs text-stage-400">Group name<input aria-label="Group name" className="h-8 rounded-md bg-stage-800 px-2.5 text-sm text-stage-100 outline-none focus:ring-1 focus:ring-accent-400" value={draft.name} placeholder="e.g. Comms" onChange={(event) => setDraft({ ...draft, name: event.target.value })} /></label>
      <p className="text-xs text-stage-500">Saved with this profile. Editing a group updates every binding that uses it; it does not change audio.</p>
      <ul className="flex flex-col gap-1">{draft.members.map((member, index) => <li key={`${audioMemberKey(member)}:${index}`} className="flex items-center justify-between gap-3 rounded-md bg-stage-800 px-2 py-1 text-xs text-stage-200"><span className="min-w-0 break-all">{memberLabel(member)}</span><Button size="sm" aria-label={`Remove ${memberLabel(member)}`} onClick={() => setDraft({ ...draft, members: draft.members.filter((_, at) => at !== index) })}>Remove</Button></li>)}</ul>
      <div className="flex flex-col gap-2 border-t border-stage-800 pt-3"><span className="text-xs font-medium text-stage-300">Add a member</span><AudioTargetField value={candidate} onChange={(target) => { if (!target || target.type !== "group") setCandidate(target); }} allowGroups={false} />
        {duplicate && <p role="alert" className="text-xs text-warn">This member is already in the group.</p>}
        <Button size="sm" className="self-start" disabled={!candidate || !!candidateError || duplicate || draft.members.length >= 32} onClick={() => { if (candidate) { setDraft({ ...draft, members: [...draft.members, candidate] }); setCandidate(null); } }}>Add member</Button>
      </div>
      {validation && <p className="text-xs text-stage-500">{validation}</p>}
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" variant="primary" disabled={!!validation} onClick={() => void run(() => api.setAudioGroup({ ...draft, name: draft.name.trim() }), () => onSaved(draft.id))}>{pending ? "Saving…" : "Save group"}</Button>
        <Button size="sm" onClick={onCancel}>Cancel</Button>
        {saved && <Button size="sm" variant="danger" className="ml-auto" onClick={() => setConfirmDelete(true)}>Delete group</Button>}
      </div>
      {confirmDelete && <div role="alert" className="flex flex-wrap items-center gap-2 border-t border-stage-700 pt-3 text-xs text-stage-300"><p className="w-full">Delete “{draft.name}”? Groups in use cannot be deleted.</p><Button size="sm" variant="danger" onClick={() => { setConfirmDelete(false); void run(() => api.removeAudioGroup(draft.id), () => onDeleted(draft.id)); }}>Confirm delete group</Button><Button size="sm" onClick={() => setConfirmDelete(false)}>Keep group</Button></div>}
    </fieldset>
    {error && <p role="alert" className="text-xs text-warn">{error}</p>}
  </div>;
}

/** Group picker for a fader or action, with "New group"/"Edit group" nearby. */
export function AudioGroupField({ groupId, onChange, volumeOnly = true }: { groupId: string; onChange: (groupId: string) => void; volumeOnly?: boolean }) {
  const groups = useProfileStore((s) => s.profile?.audioGroups) ?? EMPTY_GROUPS;
  const group = groups.find((item) => item.id === groupId);
  const [editing, setEditing] = useState<{ group: AudioGroup; key: number } | null>(null);
  const [pending, setPending] = useState(false);
  const edit = (next: AudioGroup) => setEditing((current) => ({ group: next, key: (current?.key ?? 0) + 1 }));
  return <div className="flex flex-col gap-3">
    <div className="flex flex-wrap items-end gap-2">
      <div className="min-w-40 flex-1"><Select className="w-full" size="sm" disabled={pending} ariaLabel="Audio group" value={groupId || null} placeholder="Choose a group" options={[...groups.map((item) => ({ value: item.id, label: item.name })), ...(!group && groupId ? [{ value: groupId, label: `${groupId} · Missing group` }] : [])]} onChange={(id) => { onChange(id); setEditing(null); }} /></div>
      <Button size="sm" disabled={pending} onClick={() => edit({ id: crypto.randomUUID(), name: "", members: [] })}>New group</Button>
      {group && <Button size="sm" disabled={pending} onClick={() => edit(group)}>Edit group</Button>}
    </div>
    <p className="text-xs text-stage-500">Volume only · one absolute level for every available member. Group mute, hold and relative adjustments are not supported.</p>
    {!volumeOnly && <p role="alert" className="text-xs text-warn">This saved group operation is unavailable. Choose Set volume or a single target.</p>}
    {groupId && <AudioGroupStatus groupId={groupId} details />}
    {editing && <AudioGroupEditor key={editing.key} initial={editing.group} onPendingChange={setPending}
      onSaved={(id) => { onChange(id); setEditing(null); }}
      onDeleted={(id) => { if (groupId === id) onChange(""); setEditing(null); }}
      onCancel={() => setEditing(null)} />}
  </div>;
}
