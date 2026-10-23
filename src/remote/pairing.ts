/** Pairing helpers for the phone page: the code in the QR link, typed codes and a default name. */

/** Digits in a pairing code (the app makes 8). */
export const CODE_LENGTH = 8;

/** Longest phone name the app keeps. */
export const MAX_NAME = 40;

/**
 * The pairing code in a link fragment such as `#pair=12345678` (other `&key=value`
 * parts are ignored). Null when there is none or it is not a full code.
 */
export function readPairCode(hash: string): string | null {
  const fragment = hash.startsWith("#") ? hash.slice(1) : hash;
  for (const part of fragment.split("&")) {
    const [key, value = ""] = part.split("=");
    if (key !== "pair") continue;
    let decoded = value;
    try {
      decoded = decodeURIComponent(value);
    } catch {
      return null;
    }
    const code = decoded.replace(/[\s-]/g, "");
    return new RegExp(`^\\d{${CODE_LENGTH}}$`).test(code) ? code : null;
  }
  return null;
}

/** What the code field keeps of typed or pasted text: digits only, at most a full code. */
export function cleanCode(input: string): string {
  return input.replace(/\D/g, "").slice(0, CODE_LENGTH);
}

export const isFullCode = (code: string): boolean => code.length === CODE_LENGTH && /^\d+$/.test(code);

/** A code as people read it: two groups of four. */
export function formatCode(code: string): string {
  return code.length > 4 ? `${code.slice(0, 4)} ${code.slice(4)}` : code;
}

/** A sensible first name for this phone from its user agent; the user can change it. */
export function defaultPhoneName(userAgent: string, touchPoints = 0): string {
  const ua = userAgent.toLowerCase();
  if (ua.includes("ipad")) return "iPad";
  if (ua.includes("iphone")) return "iPhone";
  if (ua.includes("android")) return ua.includes("mobile") ? "Android phone" : "Android tablet";
  // iPadOS asks for desktop pages: a Mac user agent on a touch screen.
  if (ua.includes("macintosh") && touchPoints > 1) return "iPad";
  return "Phone";
}

/** The name sent with `pair`: trimmed, single-spaced, not too long; empty means "let the app choose". */
export function cleanName(name: string): string {
  return name.replace(/\s+/g, " ").trim().slice(0, MAX_NAME);
}
