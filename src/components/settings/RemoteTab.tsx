import { clsx } from "clsx";
import { LockOpen, ShieldAlert, Smartphone, Zap } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, events, type RemoteDeviceStatus, type RemotePairing, type RemoteStatus } from "../../lib/api";
import { useNow } from "../../lib/useNow";
import { QrCode } from "../QrCode";
import { Button, Spinner, Toggle } from "../ui";
import { fieldCls, Section } from "./SettingsDialog";

const MIN_PORT = 1024;
const MAX_PORT = 65535;
/** How long the app keeps a pairing code (remote/pairing.rs). */
const CODE_TTL = 120_000;

/** What became of the code on screen once the app no longer offers it. */
type PairOutcome = { kind: "gone" } | { kind: "done"; name: string } | null;

const formatCode = (code: string) => (code.length > 4 ? `${code.slice(0, 4)} ${code.slice(4)}` : code);
const formatLeft = (ms: number) => {
  const s = Math.max(0, Math.ceil(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
};

/** "3 minutes ago", "yesterday": the time since a moment, in the user's language. */
function ago(at: number, now: number, locale?: string): string {
  const rtf = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
  const s = Math.round((at - now) / 1000);
  const abs = Math.abs(s);
  if (abs < 45) return rtf.format(0, "second");
  if (abs < 45 * 60) return rtf.format(Math.round(s / 60), "minute");
  if (abs < 22 * 3600) return rtf.format(Math.round(s / 3600), "hour");
  if (abs < 26 * 86400) return rtf.format(Math.round(s / 86400), "day");
  return new Date(at).toLocaleDateString(locale, { dateStyle: "medium" });
}

/** Settings → Phone remote: switch it on, pair phones with a one-time code, remove them. */
export function RemoteTab() {
  const { t, i18n } = useTranslation();
  const [status, setStatus] = useState<RemoteStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [portDraft, setPortDraft] = useState<string | null>(null);
  const [shown, setShown] = useState<RemotePairing | null>(null);
  const [outcome, setOutcome] = useState<PairOutcome>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const shownRef = useRef<RemotePairing | null>(null);
  const devicesRef = useRef<Set<string> | null>(null);
  // Re-render twice a second for the countdown; read the clock at render so a fresh code starts at 2:00.
  useNow(500);
  const now = Date.now();

  const show = (pairing: RemotePairing | null, next: PairOutcome = null) => {
    shownRef.current = pairing;
    setShown(pairing);
    setOutcome(next);
  };

  /** Every status, from a command's answer or the app's `remote:status` event. */
  const receive = useCallback((next: RemoteStatus) => {
    const before = devicesRef.current;
    devicesRef.current = new Set(next.devices.map((d) => d.id));
    setStatus(next);
    setLoadError(null);
    const current = shownRef.current;
    // A code past its time is as good as gone, even if a status from just before it ran out says otherwise.
    const pairing = next.pairing && next.pairing.expiresAt > Date.now() ? next.pairing : null;
    if (pairing && pairing.code !== current?.code) {
      show(pairing);
    } else if (current && !pairing) {
      // The code is gone: a phone used it (a new phone in the list), or it expired or was burnt.
      const added = before ? next.devices.find((d) => !before.has(d.id)) : undefined;
      show(null, added ? { kind: "done", name: added.name } : { kind: "gone" });
    } else if (current && pairing) {
      shownRef.current = pairing;
      setShown(pairing);
    }
  }, []);

  useEffect(() => {
    let alive = true;
    let unlisten: (() => void) | null = null;
    api.remoteStatus().then(
      (s) => alive && receive(s),
      (e: unknown) => alive && setLoadError(String(e)),
    );
    void events.onRemoteStatus((s) => alive && receive(s)).then((fn) => {
      if (alive) unlisten = fn;
      else fn();
    });
    return () => {
      alive = false;
      unlisten?.();
    };
  }, [receive]);

  // The code ran out on screen: say so at once and ask the app for the current state.
  const left = shown ? shown.expiresAt - now : 0;
  useEffect(() => {
    if (!shown || left > 0) return;
    show(null, { kind: "gone" });
    api.remoteStatus().then(receive, () => undefined);
  }, [shown, left, receive]);

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  if (!status) {
    return loadError ? <p role="alert" className="text-xs text-danger">{t("remote.loadError", { error: loadError })}</p> : <Spinner />;
  }

  const draftPort = portDraft !== null && /^\d+$/.test(portDraft) ? Number(portDraft) : NaN;
  const validPort = Number.isInteger(draftPort) && draftPort >= MIN_PORT && draftPort <= MAX_PORT;
  const commitPort = () =>
    run(async () => {
      if (!validPort) return;
      if (draftPort === status.port) {
        setPortDraft(null);
        return;
      }
      receive(await api.setRemoteSettings(status.enabled, draftPort));
      setPortDraft(null);
    });
  const toggle = (enabled: boolean) =>
    run(async () => {
      if (!enabled) show(null);
      receive(await api.setRemoteSettings(enabled, status.port));
    });
  const startPairing = () =>
    run(async () => {
      const pairing = await api.remoteStartPairing();
      show(pairing);
    });
  const cancelPairing = () =>
    run(async () => {
      show(null);
      receive(await api.remoteCancelPairing());
    });
  const revoke = (device: RemoteDeviceStatus) =>
    run(async () => {
      setConfirming(null);
      receive(await api.remoteRevokeDevice(device.id));
    });

  const connected = status.connected.length;

  return (
    <>
      <Section description={t("remote.intro")}>
        <Toggle checked={status.enabled} disabled={busy} onChange={(on) => void toggle(on)} label={t("remote.enable")} hint={t("remote.enableHint")} />
        <div className="flex flex-col gap-1.5">
          <div className="flex items-end gap-2">
            <label className="flex max-w-36 flex-col gap-1 text-xs text-stage-400">
              {t("remote.port")}
              <input
                type="text"
                inputMode="numeric"
                value={portDraft ?? String(status.port)}
                disabled={busy}
                aria-invalid={portDraft !== null && !validPort}
                aria-describedby="remote-port-hint"
                onChange={(e) => setPortDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    void commitPort();
                  }
                }}
                className={fieldCls}
              />
            </label>
            <Button size="sm" disabled={busy || portDraft === null || !validPort} onClick={() => void commitPort()}>
              {t("remote.applyPort")}
            </Button>
          </div>
          <p id="remote-port-hint" className={clsx("text-xs", portDraft !== null && !validPort ? "text-warn" : "text-stage-500")}>
            {t(portDraft !== null && !validPort ? "remote.invalidPort" : "remote.portDraftHint")}
          </p>
        </div>
        {busy && <p role="status" className="text-xs text-stage-400">{t("remote.saving")}</p>}
        {error && <p role="alert" className="max-w-lg break-words text-xs text-danger">{t("remote.saveError", { error })}</p>}
      </Section>

      <Section title={t("remote.warningsTitle")}>
        <ul className="flex max-w-lg flex-col gap-2.5 rounded-lg border border-warn/20 bg-warn/5 p-3 text-xs leading-relaxed text-stage-300">
          <li className="flex gap-2.5"><LockOpen aria-hidden className="mt-0.5 h-3.5 w-3.5 shrink-0 text-warn" />{t("remote.warnUnencrypted")}</li>
          <li className="flex gap-2.5"><Zap aria-hidden className="mt-0.5 h-3.5 w-3.5 shrink-0 text-warn" />{t("remote.warnActions")}</li>
          <li className="flex gap-2.5"><ShieldAlert aria-hidden className="mt-0.5 h-3.5 w-3.5 shrink-0 text-warn" />{t("remote.warnFirewall")}</li>
        </ul>
      </Section>

      {status.enabled && (
        <Section title={t("remote.statusTitle")}>
          <div className="flex max-w-lg flex-col gap-3 rounded-lg border border-stage-700 bg-stage-800/50 p-3">
            <div className="flex items-center gap-2 text-sm text-stage-200" role="status">
              <span className={clsx("h-2 w-2 rounded-full", status.running ? "bg-ok shadow-[0_0_8px_var(--color-ok)]" : "bg-warn")} />
              {status.running ? t("remote.running") : t("remote.notRunning")}
              {status.running && connected > 0 && <span className="ml-auto text-xs text-stage-400">{t("remote.phonesConnected", { count: connected })}</span>}
            </div>
            {status.error && <p role="alert" className="break-words text-xs text-warn">{t("remote.startError", { error: status.error })}</p>}
            {status.running &&
              (status.urls.length > 0 ? (
                <div className="flex flex-col gap-1">
                  <span className="text-xs text-stage-400">{t("remote.openOnPhone")}</span>
                  {status.urls.map((url) => (
                    <code key={url} className="w-fit select-text rounded bg-stage-900 px-2 py-1 font-mono text-sm text-stage-100">{url}</code>
                  ))}
                </div>
              ) : (
                <p className="text-xs text-warn">{t("remote.noAddress")}</p>
              ))}
          </div>
        </Section>
      )}

      {status.enabled && (
        <Section title={t("remote.pairTitle")} description={t("remote.pairHint")}>
          {!status.running ? (
            <p className="text-xs text-stage-500">{t("remote.pairOff")}</p>
          ) : shown ? (
            <PairingPanel pairing={shown} left={left} busy={busy} onNew={() => void startPairing()} onCancel={() => void cancelPairing()} />
          ) : (
            <div className="flex max-w-lg flex-col items-start gap-3">
              {outcome?.kind === "gone" && (
                <p role="alert" className="rounded-lg border border-warn/25 bg-warn/10 px-3 py-2 text-xs leading-relaxed text-stage-200">{t("remote.pairGone")}</p>
              )}
              {outcome?.kind === "done" && (
                <p role="status" className="rounded-lg border border-ok/25 bg-ok/10 px-3 py-2 text-xs text-stage-100">{t("remote.pairDone", { name: outcome.name })}</p>
              )}
              <Button variant="primary" size="sm" disabled={busy} onClick={() => void startPairing()}>
                <Smartphone aria-hidden className="h-3.5 w-3.5" />
                {outcome ? t("remote.pairNew") : t("remote.pairStart")}
              </Button>
            </div>
          )}
        </Section>
      )}

      <Section title={t("remote.devicesTitle")} description={t("remote.devicesHint")}>
        {status.devices.length === 0 ? (
          <p className="text-xs text-stage-500">{t("remote.noDevices")}</p>
        ) : (
          <ul className="flex max-w-lg flex-col gap-1.5" aria-label={t("remote.devicesTitle")}>
            {status.devices.map((d) => (
              <li key={d.id} aria-label={d.name} className="flex min-h-[46px] items-center gap-3 rounded-lg border border-stage-700 bg-stage-800/40 px-2.5 py-1.5">
                <Smartphone aria-hidden className={clsx("h-4 w-4 shrink-0", d.connected ? "text-ok" : "text-stage-500")} />
                {confirming === d.id ? (
                  <>
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm text-stage-100">{t("remote.revokeTitle", { name: d.name })}</div>
                      <div className="text-xs text-stage-400">{t("remote.revokeBody")}</div>
                    </div>
                    <Button size="sm" disabled={busy} onClick={() => setConfirming(null)}>{t("common.cancel")}</Button>
                    <Button size="sm" variant="danger" disabled={busy} onClick={() => void revoke(d)}>{t("remote.revokeConfirm")}</Button>
                  </>
                ) : (
                  <>
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm text-stage-100">{d.name}</div>
                      <div className="text-xs text-stage-500">
                        {t("remote.pairedOn", { date: new Date(d.createdAt).toLocaleDateString(i18n.language, { dateStyle: "medium" }) })}
                        {" · "}
                        {d.connected ? <span className="text-ok">{t("remote.connectedNow")}</span> : d.lastSeen ? t("remote.lastSeen", { date: ago(d.lastSeen, now, i18n.language) }) : t("remote.neverSeen")}
                      </div>
                    </div>
                    <Button size="sm" variant="danger" disabled={busy} onClick={() => setConfirming(d.id)}>{t("remote.revoke")}</Button>
                  </>
                )}
              </li>
            ))}
          </ul>
        )}
      </Section>
    </>
  );
}

/** The live code: QR code, the digits to type, and how long it still works. */
function PairingPanel({ pairing, left, busy, onNew, onCancel }: { pairing: RemotePairing; left: number; busy: boolean; onNew: () => void; onCancel: () => void }) {
  const { t } = useTranslation();
  // The app gives a code two minutes; the bar shows what is left of them.
  const share = Math.max(0, Math.min(1, left / Math.max(CODE_TTL, left)));
  return (
    <div className="flex max-w-lg flex-wrap items-stretch gap-5 rounded-xl border border-stage-700 bg-stage-800/50 p-4">
      {pairing.url ? (
        <div className="flex flex-col items-center gap-2">
          <div className="rounded-lg bg-white p-1.5 shadow-[0_8px_24px_-12px_rgba(0,0,0,0.9)]">
            <QrCode text={pairing.url} label={t("remote.pairQr")} className="block h-40 w-40" />
          </div>
          <span className="text-[11px] text-stage-400">{t("remote.pairScan")}</span>
        </div>
      ) : null}
      <div className="flex min-w-48 flex-1 flex-col gap-3">
        <div>
          <div className="text-xs text-stage-400">{pairing.url ? t("remote.pairOrType") : t("remote.pairCode")}</div>
          <div aria-label={t("remote.pairCode")} className="mt-1 select-text font-mono text-3xl font-semibold tracking-[0.12em] text-stage-100 tabular-nums">{formatCode(pairing.code)}</div>
        </div>
        {!pairing.url && <p className="text-xs text-warn">{t("remote.pairNoUrl")}</p>}
        <div className="flex flex-col gap-1.5" role="timer" aria-live="off">
          <div className="h-1 overflow-hidden rounded-full bg-stage-700">
            <div className={clsx("h-full rounded-full transition-[width] duration-500 ease-linear", share > 0.25 ? "bg-accent-500" : "bg-warn")} style={{ width: `${share * 100}%` }} />
          </div>
          <span className="text-xs text-stage-400 tabular-nums">{t("remote.pairExpires", { time: formatLeft(left) })}</span>
        </div>
        <div className="mt-auto flex flex-wrap gap-2">
          <Button size="sm" disabled={busy} onClick={onNew}>{t("remote.pairNew")}</Button>
          <Button size="sm" variant="danger" disabled={busy} onClick={onCancel}>{t("remote.pairCancel")}</Button>
        </div>
      </div>
    </div>
  );
}
