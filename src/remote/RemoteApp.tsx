import { clsx } from "clsx";
import { useEffect, useMemo, useState, useSyncExternalStore } from "react";
import type { RemoteClient } from "./client";
import { FaderStrips } from "./FaderStrips";
import { PadBoard } from "./PadBoard";
import { PageStrip } from "./PageStrip";
import { Mark } from "./Mark";
import { PairScreen } from "./PairScreen";
import { t } from "./strings";
import { boardOf } from "./view";
import { useElementSize } from "../lib/useElementSize";

/** The phone page: pairing until signed in, then the pads, faders and pages. */
export function RemoteApp({ client, linkCode }: { client: RemoteClient; linkCode: string | null }) {
  const snap = useSyncExternalStore(
    (fn) => client.subscribe(fn),
    () => client.snapshot(),
  );
  // The QR link's code works once: after signing in it is not offered again (e.g. after a revoke).
  const [link, setLink] = useState(linkCode);
  useEffect(() => {
    if (snap.status === "connected") setLink(null);
  }, [snap.status]);
  const { ref, width, height } = useElementSize<HTMLDivElement>();
  const landscape = width > height && height > 0;

  const showPads = snap.state !== null && snap.paired;
  let body: React.ReactNode;
  if (snap.status === "unpaired" || snap.status === "stopped" || (!snap.paired && snap.status !== "connected")) {
    body = <PairScreen client={client} snap={snap} linkCode={link} />;
  } else if (!showPads) {
    body = <Connecting snap={snap} onRetry={() => client.retryNow()} />;
  } else {
    body = <Remote client={client} snap={snap} landscape={landscape} />;
  }

  return (
    <div ref={ref} className="rp-stage rp-safe flex h-full w-full flex-col text-stage-100">
      {body}
    </div>
  );
}

type Snap = ReturnType<RemoteClient["snapshot"]>;

function Connecting({ snap, onRetry }: { snap: Snap; onRetry: () => void }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-5 px-8 text-center" role="status">
      <Mark big />
      <div className="text-base font-medium text-stage-100">{snap.status === "signingIn" ? t("signingIn") : snap.status === "waiting" ? t("reconnecting") : t("connecting")}</div>
      {snap.status === "waiting" && (
        <>
          {snap.problem && <p className="max-w-xs text-sm leading-relaxed text-stage-400">{t(`problems.${snap.problem}`)}</p>}
          <RetryButton snap={snap} onRetry={onRetry} />
        </>
      )}
    </div>
  );
}

/** "Try now", with the seconds left until the next automatic try. */
function RetryButton({ snap, onRetry, compact = false }: { snap: Snap; onRetry: () => void; compact?: boolean }) {
  const seconds = useCountdown(snap.retryAt);
  return (
    <button
      type="button"
      onClick={onRetry}
      className={clsx(
        "rounded-full bg-stage-700 font-medium text-stage-100 active:bg-stage-600",
        compact ? "min-h-9 px-3.5 text-xs" : "min-h-11 px-5 text-sm",
      )}
    >
      {t("retryNow")}
      {seconds !== null && <span className="ml-1.5 text-stage-400 tabular-nums">· {seconds} s</span>}
    </button>
  );
}

function useCountdown(at: number | null): number | null {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (at === null) return;
    const id = window.setInterval(() => setNow(Date.now()), 250);
    return () => window.clearInterval(id);
  }, [at]);
  return at === null ? null : Math.max(0, Math.ceil((at - now) / 1000));
}

function Remote({ client, snap, landscape }: { client: RemoteClient; snap: Snap; landscape: boolean }) {
  const state = snap.state!;
  const live = snap.status === "connected";
  const pages = <PageStrip pages={state.pages} active={state.activePage} pending={snap.pendingPage} disabled={!live} onSwitch={(id) => client.switchPage(id)} />;
  const aspect = useMemo(() => (state.layout ? boardOf(state.layout).aspect : 1), [state.layout]);
  const faders = state.device && state.layout && state.faders.length > 0 ? <FaderStrips state={state} drafts={snap.faderDrafts} disabled={!live} vertical={landscape} client={client} /> : null;

  return (
    <>
      <header className={clsx("flex shrink-0 items-center gap-3 px-4", landscape ? "h-12" : "h-14")}>
        <div className="flex min-w-0 items-center gap-2.5">
          <Mark />
          <div className="min-w-0 leading-tight">
            <div className="truncate text-sm font-semibold tracking-tight">{t("title")}</div>
            {state.device && <div className="truncate text-[11px] text-stage-400">{state.device.modelName}</div>}
          </div>
        </div>
        {landscape && <div className="min-w-0 flex-1">{pages}</div>}
        <div className={clsx("flex shrink-0 items-center gap-2", !landscape && "ml-auto")}>
          <ConnectionPill snap={snap} />
        </div>
      </header>
      {!landscape && <div className="shrink-0 px-3 pb-2">{pages}</div>}

      {snap.status !== "connected" && (
        <div role="status" className="mx-3 mb-2 flex shrink-0 items-center gap-3 rounded-xl border border-warn/25 bg-warn/10 px-3 py-2 text-xs text-stage-200">
          <span className="min-w-0 flex-1 leading-snug">{snap.problem ? t(`problems.${snap.problem}`) : t("reconnecting")}</span>
          {snap.status === "waiting" && <RetryButton snap={snap} onRetry={() => client.retryNow()} compact />}
        </div>
      )}

      {/* The board keeps its shape and shrinks to fit; the faders sit right beside or under it. */}
      <main className={clsx("flex min-h-0 flex-1 items-center justify-center gap-3 px-3 pb-3", landscape ? "flex-row" : "flex-col")}>
        <div
          className={clsx("min-h-0 min-w-0 shrink transition-[opacity,filter] duration-300", landscape ? "h-full max-w-full" : "w-full max-h-full", !live && "pointer-events-none opacity-45 saturate-50")}
          style={{ aspectRatio: aspect }}
        >
          {state.device && state.layout ? (
            <PadBoard layout={state.layout} state={state} held={snap.held} drafts={snap.faderDrafts} client={client} live={live} />
          ) : (
            <NoDevice />
          )}
        </div>
        {faders && (
          <section aria-label={t("faders")} className={clsx("shrink-0", landscape ? "h-full max-w-[45%]" : "max-h-[38%] w-full")}>
            {faders}
          </section>
        )}
      </main>
      <Notice snap={snap} />
    </>
  );
}

function ConnectionPill({ snap }: { snap: Snap }) {
  const seconds = useCountdown(snap.status === "waiting" ? snap.retryAt : null);
  const live = snap.status === "connected";
  const label = live ? t("connected") : snap.status === "waiting" ? (seconds !== null && seconds > 0 ? t("reconnectIn", { seconds }) : t("reconnecting")) : snap.status === "signingIn" ? t("signingIn") : t("connecting");
  return (
    <span role="status" aria-live="polite" className="flex items-center gap-1.5 rounded-full bg-stage-800/80 px-2.5 py-1 text-[11px] text-stage-300">
      <span className={clsx("h-2 w-2 rounded-full", live ? "bg-ok shadow-[0_0_8px_var(--color-ok)]" : snap.status === "waiting" ? "bg-warn" : "rp-blink bg-stage-400")} />
      {label}
    </span>
  );
}

function NoDevice() {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 rounded-2xl border border-dashed border-stage-700 px-8 text-center">
      <div className="text-sm font-medium text-stage-200">{t("noDevice")}</div>
      <p className="max-w-xs text-xs leading-relaxed text-stage-400">{t("noDeviceHint")}</p>
    </div>
  );
}

/** A refused command, shown for a moment above the bottom edge. */
function Notice({ snap }: { snap: Snap }) {
  const [shown, setShown] = useState<{ code: string; at: number } | null>(null);
  useEffect(() => {
    if (!snap.notice) return;
    setShown(snap.notice);
    const id = window.setTimeout(() => setShown(null), 2600);
    return () => window.clearTimeout(id);
  }, [snap.notice]);
  if (!shown) return null;
  return (
    <div
      key={shown.at}
      role="alert"
      className="rp-toast pointer-events-none fixed left-1/2 z-20 -translate-x-1/2 max-w-[85vw] rounded-full border border-stage-600 bg-stage-800/95 px-4 py-2 text-center text-xs text-stage-100 shadow-xl"
      style={{ bottom: "calc(env(safe-area-inset-bottom) + 16px)" }}
    >
      {t(`notices.${shown.code}`)}
    </div>
  );
}
