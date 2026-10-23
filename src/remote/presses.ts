/**
 * Which pads this phone holds, finger by finger. Two fingers on one pad are one
 * press for the app: the pad goes down with the first finger and up with the last,
 * so lifting one of them never ends a hold the other still makes.
 */
export class PressTracker {
  /** finger (pointer id) → pad key "x,y" */
  private fingers = new Map<number, string>();
  /** pad key → how many fingers are on it */
  private counts = new Map<string, number>();

  /** A finger lands on a pad; true when the pad has to be pressed (no other finger on it). */
  down(pointer: number, key: string): boolean {
    // A finger the page missed lifting (a lost pointerup) lets go of its old pad first.
    if (this.fingers.has(pointer)) this.up(pointer);
    this.fingers.set(pointer, key);
    const n = (this.counts.get(key) ?? 0) + 1;
    this.counts.set(key, n);
    return n === 1;
  }

  /** A finger lifts (or is cancelled); the pad to release when it was the last finger on it. */
  up(pointer: number): string | null {
    const key = this.fingers.get(pointer);
    if (key === undefined) return null;
    this.fingers.delete(pointer);
    const n = (this.counts.get(key) ?? 1) - 1;
    if (n > 0) {
      this.counts.set(key, n);
      return null;
    }
    this.counts.delete(key);
    return key;
  }

  /** Let go of everything (the page hid, the connection dropped); the pads to release. */
  releaseAll(): string[] {
    const keys = [...this.counts.keys()];
    this.fingers.clear();
    this.counts.clear();
    return keys;
  }

  /** Forget a pad the app refused, so the fingers on it no longer count as holding it. */
  forget(key: string): void {
    if (!this.counts.has(key)) return;
    this.counts.delete(key);
    for (const [pointer, k] of this.fingers) if (k === key) this.fingers.delete(pointer);
  }

  isHeld(key: string): boolean {
    return this.counts.has(key);
  }

  held(): string[] {
    return [...this.counts.keys()];
  }
}

export const padKeyOf = (x: number, y: number): string => `${x},${y}`;

export function parsePadKey(key: string): { x: number; y: number } {
  const [x, y] = key.split(",").map(Number);
  return { x, y };
}
