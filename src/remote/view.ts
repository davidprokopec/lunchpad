/** How the phone page draws what the app sends: board geometry, pad faces and fader levels. */
import type { Layout, PadSpec, RemoteColor, RemoteFader, RemotePad, RemoteState } from "../lib/api";
import { contrastText, hexToRgb, padCss, type Rgb } from "../lib/colors";
import { isControl, isKey } from "../lib/grid";
import { padKeyOf } from "./presses";

export interface Board {
  columns: number;
  rowWeights: number[];
  /** columns / total row weight */
  aspect: number;
  /** CSS grid rows, top row first */
  rowsTopDown: string;
  /** Pads drawn cell by cell, top row first (piano keys are drawn as one keyboard row). */
  cells: PadSpec[];
  keys: PadSpec[];
}

/** The board of a layout, as the desktop grid lays it out. */
export function boardOf(layout: Layout): Board {
  const columns = Math.max(1, ...layout.pads.filter((p) => !isKey(p.shape) && p.shape !== "empty").map((p) => p.x + Math.max(1, p.cols ?? 1)));
  const total = layout.rowWeights.reduce((a, b) => a + b, 0) || 1;
  return {
    columns,
    rowWeights: layout.rowWeights,
    aspect: columns / total,
    rowsTopDown: [...layout.rowWeights].reverse().map((w) => `${w}fr`).join(" "),
    cells: layout.pads.filter((p) => !isKey(p.shape) && p.shape !== "empty").sort((a, b) => b.y - a.y || a.x - b.x),
    keys: layout.pads.filter((p) => isKey(p.shape)),
  };
}

/** Largest board of the given aspect that fits the box. */
export function fitBoard(aspect: number, width: number, height: number): { width: number; height: number } {
  if (width <= 0 || height <= 0 || aspect <= 0) return { width: 0, height: 0 };
  const w = Math.min(width, height * aspect);
  return { width: w, height: w / aspect };
}

/** What a finger on this control does: press it, drag a fader, or nothing. */
export type PadRole = "press" | "control" | "none";

export function padRole(spec: PadSpec): PadRole {
  if (spec.shape === "empty" || spec.shape === "logo") return "none";
  if (isControl(spec.shape)) return "control";
  // A printed button that sends nothing the app can use (as on the desktop).
  if (spec.note === null) return "none";
  return "press";
}

export interface Face {
  off: boolean;
  /** CSS colour of the face (dark LEDs lifted so the pad still reads). */
  bg: string;
  /** Second colour of a flashing pad (= bg otherwise). */
  alt: string;
  flashing: boolean;
  pulsing: boolean;
  /** Readable text on the face. */
  text: string;
  rgb: Rgb;
}

const BLACK: Rgb = { r: 0, g: 0, b: 0 };

/** The face of a pad in the colour its LED shows. */
export function faceOf(color: RemoteColor | undefined): Face {
  const rgb = !color || color.mode === "off" ? BLACK : hexToRgb(color.rgb);
  const off = rgb.r === 0 && rgb.g === 0 && rgb.b === 0;
  const alt = color?.mode === "flashing" ? hexToRgb(color.altRgb) : rgb;
  return {
    off,
    bg: off ? "" : padCss(rgb),
    alt: off ? "" : padCss(alt),
    flashing: !off && color?.mode === "flashing",
    pulsing: !off && color?.mode === "pulsing",
    text: off ? "#c9c9d3" : contrastText(rgb),
    rgb,
  };
}

/** The pad of a fader that shows its level. */
export function levelStep(fader: Pick<RemoteFader, "pads" | "fraction">, fraction = fader.fraction): number {
  const steps = fader.pads.length;
  if (steps <= 1) return 0;
  return Math.round(Math.min(1, Math.max(0, fraction)) * (steps - 1));
}

export interface StateIndex {
  pads: Map<string, RemotePad>;
  faders: Map<string, RemoteFader>;
  /** pad key → fader and the pad's step in it */
  faderAt: Map<string, { fader: RemoteFader; step: number }>;
}

export function indexState(state: RemoteState | null): StateIndex {
  const pads = new Map<string, RemotePad>();
  const faders = new Map<string, RemoteFader>();
  const faderAt = new Map<string, { fader: RemoteFader; step: number }>();
  for (const pad of state?.pads ?? []) pads.set(padKeyOf(pad.x, pad.y), pad);
  for (const fader of state?.faders ?? []) {
    faders.set(fader.id, fader);
    fader.pads.forEach(([x, y], step) => faderAt.set(padKeyOf(x, y), { fader, step }));
  }
  return { pads, faders, faderAt };
}

/**
 * Fraction under the finger on a fader strip (0 = the fader's minimum). Strips always fill
 * from the bottom (upright) or from the left (lying), whichever way the pads run.
 */
export function stripFraction(pointer: { x: number; y: number }, box: { left: number; top: number; width: number; height: number }, axis: "vertical" | "horizontal"): number {
  const raw = axis === "vertical" ? 1 - (pointer.y - box.top) / (box.height || 1) : (pointer.x - box.left) / (box.width || 1);
  return Math.min(1, Math.max(0, raw));
}
