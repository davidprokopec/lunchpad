import { clsx } from "clsx";
import { useState } from "react";
import type { ClientSnapshot, RemoteClient } from "./client";
import { cleanCode, cleanName, CODE_LENGTH, defaultPhoneName, formatCode, isFullCode } from "./pairing";
import { Mark } from "./Mark";
import { t } from "./strings";

/** Pairing: the code from the QR link (or typed), a name for the phone, and why the last try failed. */
export function PairScreen({ client, snap, linkCode }: { client: RemoteClient; snap: ClientSnapshot; linkCode: string | null }) {
  const [code, setCode] = useState(linkCode ?? "");
  const [name, setName] = useState(() => snap.deviceName ?? defaultPhoneName(navigator.userAgent, navigator.maxTouchPoints ?? 0));
  const busy = snap.status === "connecting" || snap.status === "signingIn";
  const stopped = snap.status === "stopped";
  const ready = isFullCode(code) && !busy && !stopped;
  const problem = snap.problem;
  const submit = () => {
    if (!ready) return;
    client.pair(code, cleanName(name));
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
      <form
        className="mx-auto flex w-full max-w-sm flex-1 flex-col justify-center gap-6 px-6 py-8"
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <div className="flex flex-col items-start gap-4">
          <Mark big />
          <div>
            <h1 className="text-2xl font-semibold tracking-tight">{t("pairTitle")}</h1>
            <p className="mt-2 text-sm leading-relaxed text-stage-400">{linkCode && !problem ? t("pairFromLink") : t("pairIntro")}</p>
          </div>
        </div>

        {problem && (
          <div role="alert" className={clsx("rounded-xl border px-3.5 py-3 text-sm leading-relaxed", problem === "revoked" || problem === "outdated" ? "border-danger/30 bg-danger/10 text-stage-100" : "border-warn/30 bg-warn/10 text-stage-100")}>
            {t(`problems.${problem}`)}
          </div>
        )}

        {stopped ? (
          <button type="button" onClick={() => window.location.reload()} className="min-h-[52px] rounded-2xl bg-stage-700 text-base font-semibold text-stage-100 active:bg-stage-600">
            {t("reload")}
          </button>
        ) : (
          <>
            <label className="flex flex-col gap-2">
              <span className="text-xs font-medium uppercase tracking-[0.12em] text-stage-400">{t("codeLabel")}</span>
              <input
                className="rp-field h-16 rounded-2xl border border-stage-700 bg-stage-900 text-center font-mono !text-[28px] font-semibold tracking-[0.18em] text-stage-100 outline-none placeholder:text-stage-600 focus:border-accent-500 focus:ring-2 focus:ring-accent-500/30"
                inputMode="numeric"
                autoComplete="one-time-code"
                enterKeyHint="next"
                placeholder={formatCode("0".repeat(CODE_LENGTH)).replace(/0/g, "•")}
                value={formatCode(code)}
                disabled={busy}
                onChange={(e) => setCode(cleanCode(e.target.value))}
              />
            </label>
            <label className="flex flex-col gap-2">
              <span className="text-xs font-medium uppercase tracking-[0.12em] text-stage-400">{t("nameLabel")}</span>
              <input
                className="rp-field h-12 rounded-xl border border-stage-700 bg-stage-900 px-3.5 text-stage-100 outline-none focus:border-accent-500 focus:ring-2 focus:ring-accent-500/30"
                value={name}
                maxLength={40}
                enterKeyHint="go"
                autoCapitalize="words"
                disabled={busy}
                onChange={(e) => setName(e.target.value)}
              />
              <span className="text-xs text-stage-500">{t("nameHint")}</span>
            </label>
            <button
              type="submit"
              disabled={!ready}
              className="min-h-[52px] rounded-2xl bg-accent-500 text-base font-semibold text-stage-950 shadow-[0_10px_30px_-10px_rgba(255,122,26,0.8)] transition-[opacity,transform] active:scale-[0.98] active:bg-accent-400 disabled:opacity-35 disabled:shadow-none"
            >
              {busy ? t("pairing") : t("pair")}
            </button>
          </>
        )}
      </form>
    </div>
  );
}
