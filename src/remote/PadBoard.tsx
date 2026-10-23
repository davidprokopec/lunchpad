import { clsx } from "clsx";
import { ChevronDown, ChevronLeft, ChevronRight, ChevronUp, Image as ImageIcon } from "lucide-react";
import { useEffect, useMemo, useRef, type CSSProperties, type ReactNode } from "react";
import type { Layout, PadSpec, RemoteState } from "../lib/api";
import { cornerRadius, cornersOf, legendSize } from "../lib/grid";
import { useElementSize } from "../lib/useElementSize";
import type { RemoteClient } from "./client";
import { padKeyOf } from "./presses";
import { t } from "./strings";
import { boardOf, faceOf, fitBoard, indexState, levelStep, padRole, type StateIndex } from "./view";

interface Props {
  layout: Layout;
  state: RemoteState;
  held: string[];
  drafts: Record<string, number>;
  client: RemoteClient;
  /** Signed in: fingers press pads. */
  live: boolean;
}

/** A finger working a knob or strip: which fader it moves and from where. */
interface ControlDrag {
  faderId: string;
  shape: "knob" | "strip";
  startY: number;
  startFraction: number;
  cell: number;
  el: HTMLElement;
  last: number;
}

/**
 * The Launchpad as the app paints it: every pad in its live colour and caption,
 * laid out like the desktop grid. Each finger is its own press: a pad goes down
 * when a finger lands on it and up when that finger lifts or is cancelled.
 */
export function PadBoard({ layout, state, held, drafts, client, live }: Props) {
  const { ref, width, height } = useElementSize<HTMLDivElement>();
  const board = useMemo(() => boardOf(layout), [layout]);
  const index = useMemo(() => indexState(state), [state]);
  const size = fitBoard(board.aspect, width, height);
  const cell = size.width / board.columns;
  const heldSet = useMemo(() => new Set(held), [held]);
  const controls = useRef(new Map<number, ControlDrag>());

  // Lifting a finger anywhere ends its press: it may have slid off the board, or the
  // board may have been redrawn under it.
  useEffect(() => {
    const end = (e: PointerEvent) => {
      client.release(e.pointerId);
      const drag = controls.current.get(e.pointerId);
      if (drag) {
        controls.current.delete(e.pointerId);
        client.setFader(drag.faderId, drag.last, true);
      }
    };
    window.addEventListener("pointerup", end);
    window.addEventListener("pointercancel", end);
    return () => {
      window.removeEventListener("pointerup", end);
      window.removeEventListener("pointercancel", end);
      // The board can disappear under a held finger (e.g. the Launchpad was unplugged);
      // its lift would then never reach us, so let go of everything now.
      controls.current.clear();
      client.releaseAll();
    };
  }, [client]);

  const fractionOf = (faderId: string): number => drafts[faderId] ?? index.faders.get(faderId)?.fraction ?? 0;

  const controlValue = (drag: ControlDrag, e: { clientY: number }): number => {
    if (drag.shape === "strip") {
      const bar = drag.el.querySelector("[data-strip]") ?? drag.el;
      const r = bar.getBoundingClientRect();
      return Math.min(1, Math.max(0, 1 - (e.clientY - r.top) / (r.height || 1)));
    }
    return Math.min(1, Math.max(0, drag.startFraction + (drag.startY - e.clientY) / (drag.cell * 3)));
  };

  const onPointerDown = (e: React.PointerEvent) => {
    if (!live) return;
    const el = (e.target as Element).closest<HTMLElement>("[data-pad]");
    if (!el) return;
    const [x, y] = (el.dataset.pad ?? "").split(",").map(Number);
    if (el.dataset.role === "press") {
      e.preventDefault();
      if (client.press(e.pointerId, x, y)) navigator.vibrate?.(8);
      return;
    }
    const faderId = el.dataset.fader;
    if (el.dataset.role === "control" && faderId) {
      e.preventDefault();
      try {
        el.setPointerCapture(e.pointerId);
      } catch {
        // a synthetic pointer cannot be captured
      }
      const drag: ControlDrag = { faderId, shape: el.dataset.shape === "strip" ? "strip" : "knob", startY: e.clientY, startFraction: fractionOf(faderId), cell, el, last: fractionOf(faderId) };
      controls.current.set(e.pointerId, drag);
      if (drag.shape === "strip") {
        drag.last = controlValue(drag, e);
        client.setFader(faderId, drag.last);
      }
    }
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const drag = controls.current.get(e.pointerId);
    if (!drag) return;
    drag.last = controlValue(drag, e);
    client.setFader(drag.faderId, drag.last);
  };

  const renderPad = (spec: PadSpec, order: number): ReactNode => (
    <Cell key={padKeyOf(spec.x, spec.y)} spec={spec} cell={cell} index={index} held={heldSet} fractionOf={fractionOf} order={order} topRow={spec.y === layout.height - 1} />
  );

  return (
    <div ref={ref} className="flex h-full w-full items-center justify-center">
      {size.width > 0 && (
        <div
          className="rp-board grid select-none bg-stage-900 shadow-[inset_0_1px_0_rgba(255,255,255,0.05),0_18px_40px_-18px_rgba(0,0,0,0.9)]"
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          style={{
            width: size.width,
            height: size.height,
            borderRadius: Math.min(size.width, size.height) * 0.035,
            padding: cell * 0.1,
            gap: cell * 0.08,
            gridTemplateColumns: `repeat(${board.columns}, minmax(0, 1fr))`,
            gridTemplateRows: board.rowsTopDown,
          }}
        >
          {board.cells.map((spec, i) => (
            <div key={padKeyOf(spec.x, spec.y)} className="min-h-0 min-w-0" style={{ gridColumn: `${spec.x + 1} / span ${Math.max(1, spec.cols ?? 1)}`, gridRow: `${layout.height - spec.y} / span ${Math.max(1, spec.rows)}` }}>
              {renderPad(spec, i)}
            </div>
          ))}
          {board.keys.length > 0 && <Keyboard keys={board.keys} height={layout.height} columns={board.columns} cell={cell} renderPad={renderPad} />}
        </div>
      )}
    </div>
  );
}

/** The piano keys of a keyboard model as one row, as the desktop draws them. */
function Keyboard({ keys, height, columns, cell, renderPad }: { keys: PadSpec[]; height: number; columns: number; cell: number; renderPad: (pad: PadSpec, order: number) => ReactNode }) {
  const whites = keys.filter((k) => k.shape === "keyWhite").sort((a, b) => a.x - b.x);
  const blacks = keys.filter((k) => k.shape === "keyBlack");
  const top = Math.max(...keys.map((k) => k.y));
  const bottom = Math.min(...keys.map((k) => k.y));
  const slot = 100 / Math.max(1, whites.length);
  const gap = cell * 0.04;
  return (
    <div className="min-h-0 min-w-0" style={{ gridColumn: `1 / ${columns + 1}`, gridRow: `${height - top} / span ${top - bottom + 1}`, paddingTop: cell * 0.22, paddingBottom: cell * 0.06 }}>
      <div className="relative h-full w-full">
        {whites.map((k, i) => (
          <div key={padKeyOf(k.x, k.y)} className="absolute inset-y-0" style={{ left: `${i * slot}%`, width: `${slot}%`, paddingLeft: gap / 2, paddingRight: gap / 2 }}>
            {renderPad(k, i)}
          </div>
        ))}
        {blacks.map((k) => {
          const i = whites.findIndex((w) => w.x === k.x);
          if (i < 0) return null;
          return (
            <div key={padKeyOf(k.x, k.y)} className="absolute top-0 z-10" style={{ left: `${(i + 1) * slot - slot * 0.31}%`, width: `${slot * 0.62}%`, height: "60%" }}>
              {renderPad(k, i)}
            </div>
          );
        })}
      </div>
    </div>
  );
}

const CHEVRONS: Record<string, typeof ChevronUp> = { "∧": ChevronUp, "∨": ChevronDown, ">": ChevronRight, "<": ChevronLeft };

/** A printed legend (Arp, Scale, a chevron) in a given size. */
function Legend({ label, size }: { label: string | null; size: number }) {
  if (!label) return null;
  const Icon = CHEVRONS[label];
  return Icon ? <Icon aria-hidden style={{ width: size * 1.3, height: size * 1.3 }} /> : <>{label}</>;
}

/** Box of a control that is smaller than its cell, as on the desktop. */
function sizeOf(spec: PadSpec, cell: number): { className: string; style?: CSSProperties } {
  switch (spec.shape) {
    case "small":
      return { className: "h-1/2 w-1/2" };
    case "rect":
      return { className: "shrink-0", style: { width: cell * 0.9, height: cell * 0.57 } };
    case "smallRect":
      return { className: "shrink-0", style: { width: cell * 0.54, height: cell * 0.46 } };
    case "halfPad":
      return { className: "shrink-0", style: { width: cell * 0.9, height: cell * 0.42 } };
    case "tallRect":
      return { className: "h-full shrink-0", style: { width: cell * 0.54 } };
    default:
      return { className: "h-full w-full" };
  }
}

interface CellProps {
  spec: PadSpec;
  cell: number;
  index: StateIndex;
  held: Set<string>;
  fractionOf: (faderId: string) => number;
  order: number;
  topRow: boolean;
}

function Cell({ spec, cell, index, held, fractionOf, order, topRow }: CellProps) {
  const key = padKeyOf(spec.x, spec.y);
  const role = padRole(spec);
  const pad = index.pads.get(key);
  const radius = cornerRadius(cornersOf(spec.shape), cell);
  const rise: CSSProperties = { animationDelay: `${Math.min(order, 80) * 9}ms` };

  if (spec.shape === "logo") {
    return (
      <div aria-hidden className="flex h-full w-full items-center justify-center">
        <span className="rounded-full border border-stage-600" style={{ width: cell * 0.36, height: cell * 0.36, backgroundColor: faceOf(pad?.color).bg || undefined }} />
      </div>
    );
  }

  if (role === "control") {
    const faderId = pad?.fader ?? index.faderAt.get(key)?.fader.id;
    const fader = faderId ? index.faders.get(faderId) : undefined;
    const fraction = faderId ? fractionOf(faderId) : null;
    return (
      <div
        data-pad={key}
        data-role="control"
        data-shape={spec.shape === "strip" ? "strip" : "knob"}
        data-fader={faderId}
        role={fader ? "slider" : undefined}
        aria-label={fader?.name || spec.label || undefined}
        aria-valuemin={fader ? 0 : undefined}
        aria-valuemax={fader ? 100 : undefined}
        aria-valuenow={fraction !== null ? Math.round(fraction * 100) : undefined}
        aria-valuetext={fader?.display}
        className={clsx("rp-rise flex h-full w-full flex-col items-center gap-0.5", spec.shape === "knob" && spec.rows > 1 ? "justify-center" : "justify-start", !fader && "opacity-50")}
        style={{ ...rise, paddingTop: spec.shape === "knob" && spec.rows > 1 ? 0 : cell * 0.06 }}
      >
        {spec.shape === "strip" ? <StripFace fraction={fraction} cell={cell} /> : <KnobFace fraction={fraction} cell={cell} />}
        <span className="max-w-full truncate leading-none text-stage-400" style={{ fontSize: Math.max(8, cell * 0.13) }}>
          {fader ? fader.display : spec.label}
        </span>
      </div>
    );
  }

  const box = sizeOf(spec, cell);
  const hangTop = topRow && spec.shape === "rect";
  const align = hangTop || spec.edge === "top" ? "items-start" : spec.edge === "bottom" ? "items-end" : "items-center";
  // The entrance animation sits on the wrapper: on the face it would pin the face's transform
  // and hide the pressed look.
  const outer = clsx("rp-rise flex h-full w-full justify-center", align);
  const outerStyle: CSSProperties = { ...rise, ...(spec.shape === "round" ? { padding: cell * 0.08 } : {}), ...(hangTop ? { paddingTop: cell * 0.06 } : {}) };

  if (role === "none") {
    return (
      <div className={outer} style={outerStyle} aria-hidden>
        <span className={clsx("flex items-center justify-center border border-dashed border-stage-600/70 bg-stage-900/40 px-1 text-center leading-tight text-stage-600", box.className)} style={{ ...box.style, borderRadius: radius, fontSize: legendSize(cell) }}>
          <Legend label={spec.label} size={legendSize(cell)} />
        </span>
      </div>
    );
  }

  const face = faceOf(pad?.color);
  const hit = index.faderAt.get(key);
  const pressed = !!pad?.pressed || held.has(key);
  const white = spec.shape === "keyWhite";
  const black = spec.shape === "keyBlack";
  const mask = spec.mask;
  const lit = !face.off;
  const legend = legendSize(cell);
  const captionSize = Math.max(9, Math.min(20, cell * 0.2));

  // What the face shows: a fader's level and name, the button's caption or picture, or the printed legend.
  let content: ReactNode = null;
  let name = spec.label ?? t("pad", { column: spec.x + 1, row: spec.y + 1 });
  let isLevel = false;
  if (hit) {
    const step = levelStep(hit.fader, fractionOf(hit.fader.id));
    isLevel = step === hit.step;
    name = hit.fader.name || name;
    // The level text shrinks to fit a small pad ("-42 dB" on a phone held sideways).
    if (isLevel) content = <span className="whitespace-nowrap font-semibold leading-none" style={{ fontSize: Math.max(7, Math.min(cell * 0.22, (cell * 1.45) / Math.max(4, hit.fader.display.length))), color: face.text }}>{hit.fader.display}</span>;
    else if (hit.step === 0 && hit.fader.name) content = <span className="absolute inset-x-0 bottom-[8%] truncate px-1 text-center" style={{ fontSize: Math.max(8, cell * 0.14), color: face.text, opacity: 0.75 }}>{hit.fader.name}</span>;
  } else if (pad?.label?.type === "text") {
    name = pad.label.caption;
    content = (
      <span
        className="line-clamp-3 max-w-full px-[7%] text-center leading-[1.1] font-medium break-words"
        style={{ fontSize: captionSize, color: mask ? (lit ? face.bg : "var(--color-stage-300)") : pad.label.color || face.text, textShadow: lit && !mask ? "0 1px 2px rgba(0,0,0,0.45)" : mask && lit ? `0 0 ${Math.max(3, cell * 0.08)}px ${face.bg}` : undefined }}
      >
        {pad.label.caption}
      </span>
    );
  } else if (pad?.label?.type === "image") {
    content = <ImageIcon aria-label={t("picture")} style={{ width: cell * 0.36, height: cell * 0.36, color: mask ? face.bg || "var(--color-stage-400)" : face.text, opacity: 0.7 }} />;
  } else if (spec.label) {
    content = (
      <span className="px-1 font-medium leading-none" style={{ fontSize: legend, color: mask ? (lit ? face.bg : "var(--color-stage-400)") : lit ? face.text : undefined, textShadow: mask && lit ? `0 0 ${Math.max(3, cell * 0.08)}px ${face.bg}` : undefined }}>
        <Legend label={spec.label} size={legend} />
      </span>
    );
  }

  const unlitBg = mask ? "var(--color-ink)" : white ? "var(--color-stage-200)" : black ? "var(--color-stage-950)" : spec.shape === "round" || spec.shape === "rect" || spec.shape === "smallRect" || spec.shape === "halfPad" || spec.shape === "tallRect" ? "var(--color-stage-800)" : "var(--color-stage-700)";
  const bg = mask ? "var(--color-ink)" : lit ? face.bg : unlitBg;
  const style: CSSProperties & Record<string, string | number | undefined> = {
    ...box.style,
    borderRadius: radius,
    backgroundColor: bg,
    "--pad-a": bg,
    "--pad-b": mask ? bg : lit ? face.alt : bg,
    boxShadow: isLevel ? "inset 0 0 0 2px rgba(255,255,255,0.85)" : mask ? "inset 0 1px 0 rgba(255,255,255,0.05), inset 0 -3px 0 rgba(0,0,0,0.7)" : "inset 0 1px 0 rgba(255,255,255,0.06), inset 0 -3px 0 rgba(0,0,0,0.38)",
    color: lit ? face.text : white ? "var(--color-stage-600)" : "var(--color-stage-500)",
  };

  return (
    <div className={outer} style={outerStyle}>
      <div
        data-pad={key}
        data-role="press"
        data-pressed={pressed}
        role="button"
        aria-label={name}
        aria-pressed={pressed}
        className={clsx(
          "rp-face relative flex items-center justify-center overflow-hidden text-center",
          box.className,
          (white || black) && "items-end pb-[12%]",
          lit && face.flashing && !mask && "pad-flashing",
          lit && face.flashing && mask && "pad-flashing-symbol",
          lit && face.pulsing && "pad-pulsing",
        )}
        style={style}
      >
        {content}
      </div>
    </div>
  );
}

/** A touch strip: the level as a fill from the bottom. */
function StripFace({ fraction, cell }: { fraction: number | null; cell: number }) {
  const pct = Math.round(Math.max(0, Math.min(1, fraction ?? 0)) * 100);
  return (
    <div data-strip className="relative min-h-0 w-full flex-1 overflow-hidden rounded-md bg-stage-800 shadow-[inset_0_1px_0_rgba(255,255,255,0.06)]" style={{ maxWidth: Math.max(14, cell * 0.68) }}>
      {fraction !== null && <div className="absolute inset-x-0 bottom-0 bg-accent-500/75" style={{ height: `${pct}%` }} />}
    </div>
  );
}

/** A knob: a 270° arc with the level. */
function KnobFace({ fraction, cell }: { fraction: number | null; cell: number }) {
  const size = Math.max(20, cell * 0.64);
  const r = 40;
  const start = 135;
  const sweep = 270;
  const point = (deg: number): [number, number] => [50 + r * Math.cos((deg * Math.PI) / 180), 50 + r * Math.sin((deg * Math.PI) / 180)];
  const arc = (from: number, to: number) => {
    const [x1, y1] = point(from);
    const [x2, y2] = point(to);
    return `M ${x1} ${y1} A ${r} ${r} 0 ${to - from > 180 ? 1 : 0} 1 ${x2} ${y2}`;
  };
  const f = Math.max(0, Math.min(1, fraction ?? 0));
  const end = start + sweep * f;
  const [px, py] = [50 + 24 * Math.cos((end * Math.PI) / 180), 50 + 24 * Math.sin((end * Math.PI) / 180)];
  return (
    <svg viewBox="5 5 90 90" width={size} height={size} aria-hidden>
      <circle cx="50" cy="50" r="33" className="fill-stage-800" />
      <path d={arc(start, start + sweep)} className="stroke-stage-600" strokeWidth="8" fill="none" strokeLinecap="round" />
      {fraction !== null && f > 0.004 && <path d={arc(start, end)} className="stroke-accent-400" strokeWidth="8" fill="none" strokeLinecap="round" />}
      <line x1="50" y1="50" x2={px} y2={py} className={fraction !== null ? "stroke-stage-100" : "stroke-stage-500"} strokeWidth="6" strokeLinecap="round" />
    </svg>
  );
}
