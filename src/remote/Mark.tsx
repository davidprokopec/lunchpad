import { clsx } from "clsx";

/** Two-by-two lit pads: the page's mark. */
export function Mark({ big = false }: { big?: boolean }) {
  const s = big ? "h-5 w-5 rounded-[5px]" : "h-2 w-2 rounded-[2px]";
  return (
    <span aria-hidden className={clsx("grid grid-cols-2", big ? "gap-1.5" : "gap-[3px]")}>
      <span className={clsx(s, "bg-accent-500 shadow-[0_0_14px_rgba(255,122,26,0.7)]", big && "rp-blink")} />
      <span className={clsx(s, "bg-stage-600")} />
      <span className={clsx(s, "bg-stage-600")} />
      <span className={clsx(s, "bg-accent-400/80 shadow-[0_0_10px_rgba(255,148,71,0.5)]", big && "rp-blink [animation-delay:550ms]")} />
    </span>
  );
}
