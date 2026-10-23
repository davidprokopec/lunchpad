/** Waiting times between reconnect attempts. */

/** Steps in milliseconds; the last one repeats. */
export const RECONNECT_STEPS = [500, 1000, 2000, 4000, 8000, 15000] as const;

/**
 * How long to wait before attempt `attempt` (0 = the first retry). `random` (0..1)
 * spreads phones that lost the same computer by ±20% so they do not knock at once.
 */
export function reconnectDelay(attempt: number, random: number = Math.random()): number {
  const step = RECONNECT_STEPS[Math.min(Math.max(0, Math.floor(attempt)), RECONNECT_STEPS.length - 1)];
  const spread = 0.8 + 0.4 * Math.min(1, Math.max(0, random));
  return Math.round(step * spread);
}

/** After a refused sign-in for too many attempts the app locks this phone out for about a minute. */
export const RATE_LIMITED_DELAY = 60_000;
