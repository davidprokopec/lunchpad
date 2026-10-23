import { clsx } from "clsx";
import { useEffect, useMemo, useRef } from "react";
import type { RemoteFader, RemoteState } from "../lib/api";
import type { RemoteClient } from "./client";
import { padKeyOf } from "./presses";
import { faceOf, stripFraction } from "./view";

/** Travel before a touch on a strip counts as a drag rather than the start of a scroll. */
const SLOP = 6;

interface Drag {
  pointerId: number;
  faderId: string;
  startX: number;
  startY: number;
  moving: boolean;
  last: number;
  el: HTMLElement;
}

/**
 * The page's faders as large strips: lying down under the pads when the phone is upright,
 * standing beside them when it is sideways. A finger sets the level where it is; a short
 * tap sets it there too, and a swipe along the list scrolls it instead.
 */
export function FaderStrips({ state, drafts, disabled, vertical, client }: { state: RemoteState; drafts: Record<string, number>; disabled: boolean; vertical: boolean; client: RemoteClient }) {
  const drag = useRef<Drag | null>(null);
  const colours = useMemo(() => {
    const byKey = new Map(state.pads.map((p) => [padKeyOf(p.x, p.y), p]));
    return new Map(
      state.faders.map((f) => {
        const first = f.pads[0];
        const last = f.pads[f.pads.length - 1];
        const a = first ? faceOf(byKey.get(padKeyOf(first[0], first[1]))?.color) : null;
        const b = last ? faceOf(byKey.get(padKeyOf(last[0], last[1]))?.color) : null;
        return [f.id, { a: a && !a.off ? a.bg : null, b: b && !b.off ? b.bg : null }];
      }),
    );
  }, [state]);

  useEffect(() => {
    // A finger released outside the strip still ends the drag.
    const end = (e: PointerEvent) => finish(e, e.type === "pointercancel");
    window.addEventListener("pointerup", end);
    window.addEventListener("pointercancel", end);
    return () => {
      window.removeEventListener("pointerup", end);
      window.removeEventListener("pointercancel", end);
    };
  });

  const valueAt = (d: Drag, e: { clientX: number; clientY: number }) => {
    const track = d.el.querySelector("[data-track]") ?? d.el;
    const r = track.getBoundingClientRect();
    return stripFraction({ x: e.clientX, y: e.clientY }, r, vertical ? "vertical" : "horizontal");
  };

  function finish(e: { pointerId: number; clientX: number; clientY: number }, cancelled: boolean) {
    const d = drag.current;
    if (!d || d.pointerId !== e.pointerId) return;
    drag.current = null;
    if (d.moving) client.setFader(d.faderId, d.last, true);
    // A tap (no travel) puts the level where the finger was; a cancelled touch was a scroll.
    else if (!cancelled) client.setFader(d.faderId, valueAt(d, e), true);
  }

  return (
    <div className={clsx("flex h-full gap-2", vertical ? "flex-row overflow-x-auto" : "flex-col overflow-y-auto")} style={{ touchAction: vertical ? "pan-x" : "pan-y" }}>
      {state.faders.map((fader) => (
        <Strip
          key={fader.id}
          fader={fader}
          fraction={drafts[fader.id] ?? fader.fraction}
          colours={colours.get(fader.id) ?? { a: null, b: null }}
          vertical={vertical}
          disabled={disabled}
          onPointerDown={(e) => {
            if (disabled || drag.current) return;
            const el = e.currentTarget;
            drag.current = { pointerId: e.pointerId, faderId: fader.id, startX: e.clientX, startY: e.clientY, moving: false, last: drafts[fader.id] ?? fader.fraction, el };
          }}
          onPointerMove={(e) => {
            const d = drag.current;
            if (!d || d.pointerId !== e.pointerId) return;
            if (!d.moving) {
              const along = vertical ? Math.abs(e.clientY - d.startY) : Math.abs(e.clientX - d.startX);
              if (along < SLOP) return;
              d.moving = true;
              try {
                d.el.setPointerCapture(e.pointerId);
              } catch {
                // a synthetic pointer cannot be captured
              }
            }
            d.last = valueAt(d, e);
            client.setFader(d.faderId, d.last);
          }}
        />
      ))}
    </div>
  );
}

function Strip({ fader, fraction, colours, vertical, disabled, onPointerDown, onPointerMove }: { fader: RemoteFader; fraction: number; colours: { a: string | null; b: string | null }; vertical: boolean; disabled: boolean; onPointerDown: (e: React.PointerEvent<HTMLDivElement>) => void; onPointerMove: (e: React.PointerEvent<HTMLDivElement>) => void }) {
  const pct = Math.round(Math.min(1, Math.max(0, fraction)) * 1000) / 10;
  const a = colours.a ?? colours.b ?? "var(--color-accent-500)";
  const b = colours.b ?? colours.a ?? "var(--color-accent-400)";
  const fill = `linear-gradient(${vertical ? "to top" : "to right"}, ${a}, ${b})`;
  return (
    <div
      data-fader-strip={fader.id}
      role="slider"
      aria-label={fader.name || fader.id}
      aria-orientation={vertical ? "vertical" : "horizontal"}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(pct)}
      aria-valuetext={fader.display}
      aria-disabled={disabled || undefined}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      className={clsx("relative shrink-0 select-none rounded-2xl border border-stage-700 bg-stage-900/90 p-1.5", vertical ? "flex h-full w-[68px] flex-col gap-1.5" : "flex h-[60px] w-full items-center gap-3 pr-3", disabled && "opacity-50")}
    >
      <div data-track className={clsx("relative overflow-hidden rounded-xl bg-stage-800 shadow-[inset_0_1px_3px_rgba(0,0,0,0.6)]", vertical ? "min-h-0 w-full flex-1" : "h-full min-w-0 flex-1")}>
        <div
          className="absolute"
          style={{
            background: fill,
            ...(vertical ? { left: 0, right: 0, bottom: 0, height: `${pct}%` } : { top: 0, bottom: 0, left: 0, width: `${pct}%` }),
            boxShadow: "0 0 18px -2px rgba(255,255,255,0.18)",
          }}
        />
        {/* the grip: a bright line where the level is */}
        <div className="absolute rounded-full bg-white/90 shadow-[0_0_8px_rgba(255,255,255,0.6)]" style={vertical ? { left: 6, right: 6, height: 3, bottom: `calc(${pct}% - 1.5px)` } : { top: 6, bottom: 6, width: 3, left: `calc(${pct}% - 1.5px)` }} />
        {!vertical && <span className="pointer-events-none absolute inset-y-0 left-3 flex items-center truncate text-sm font-medium text-white [text-shadow:0_1px_3px_rgba(0,0,0,0.8)]" style={{ maxWidth: "70%" }}>{fader.name}</span>}
      </div>
      <div className={clsx("pointer-events-none text-center leading-tight", vertical ? "w-full" : "w-16 shrink-0 text-right")}>
        {vertical && <div className="truncate text-[11px] text-stage-300">{fader.name}</div>}
        <div className="truncate text-xs font-medium text-stage-100 tabular-nums">{fader.display}</div>
      </div>
    </div>
  );
}
