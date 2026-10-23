/**
 * The phone page's words, from the `remotePad` part of en.json only (a named JSON
 * import, so the rest of the app's strings stay out of the phone's download).
 */
import { remotePad } from "../i18n/en.json";

type Tree = { [key: string]: string | Tree };

/** `t("problems.lost")`, `t("reconnectIn", { seconds: 3 })`; the key itself when missing. */
export function t(key: string, vars: Record<string, string | number> = {}): string {
  let node: string | Tree | undefined = remotePad as Tree;
  for (const part of key.split(".")) node = typeof node === "object" ? node[part] : undefined;
  if (typeof node !== "string") return key;
  return node.replace(/\{\{(\w+)\}\}/g, (_, name: string) => (name in vars ? String(vars[name]) : ""));
}
