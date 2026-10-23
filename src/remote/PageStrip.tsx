import { clsx } from "clsx";
import { useEffect, useRef } from "react";
import { t } from "./strings";

/** The profile's pages as a sideways strip of large chips; the one on the Launchpad is lit. */
export function PageStrip({ pages, active, pending, disabled, onSwitch }: { pages: { id: string; name: string }[]; active: string | null; pending: string | null; disabled: boolean; onSwitch: (id: string) => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = ref.current?.querySelector<HTMLElement>(`[data-page-id="${CSS.escape(active ?? "")}"]`);
    el?.scrollIntoView({ block: "nearest", inline: "center", behavior: "smooth" });
  }, [active]);
  if (pages.length === 0) return null;
  return (
    <nav aria-label={t("pages")} ref={ref} className="rp-chips flex gap-1.5 overflow-x-auto">
      {pages.map((page) => {
        const on = page.id === active;
        const waiting = page.id === pending;
        return (
          <button
            key={page.id}
            type="button"
            data-page-id={page.id}
            aria-current={on ? "page" : undefined}
            aria-busy={waiting || undefined}
            disabled={disabled}
            onClick={() => onSwitch(page.id)}
            className={clsx(
              "min-h-10 shrink-0 rounded-full px-4 text-sm font-medium whitespace-nowrap transition-colors",
              on
                ? "bg-accent-500 text-stage-950 shadow-[0_0_18px_-4px_rgba(255,122,26,0.8)]"
                : waiting
                  ? "rp-blink bg-stage-600 text-stage-100"
                  : "bg-stage-800 text-stage-300 active:bg-stage-700",
              disabled && !on && "opacity-50",
            )}
          >
            {page.name}
          </button>
        );
      })}
    </nav>
  );
}
