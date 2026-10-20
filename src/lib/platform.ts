export function isMacOS(platform = typeof navigator === "undefined" ? "" : navigator.platform): boolean {
  return platform.toLowerCase().includes("mac");
}

/** Filter creation/navigation choices only; never filter persisted profile data. */
export function platformOptionAvailable(value: string, platform?: string): boolean {
  return !value.startsWith("fineTune") || isMacOS(platform);
}
