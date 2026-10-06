/**
 * Drives `tickDedup` on a timer. A parked playback event is only
 * force-applied after `BUFFER_TIMEOUT_MS` when something ticks the dedup
 * state, and nothing did in production: a viewer who joined mid-session (the
 * host's first event they see has seq > 1), or whose room page was unmounted
 * while events arrived, parked that event and never applied it.
 *
 * `timers` is injectable so the behaviour can be tested without real time.
 */
export const DEDUP_TICK_INTERVAL_MS = 1000;

export interface TickerTimers {
    setInterval: (fn: () => void, ms: number) => unknown;
    clearInterval: (handle: unknown) => void;
    now: () => number;
}

const realTimers: TickerTimers = {
    setInterval: (fn, ms) => globalThis.setInterval(fn, ms),
    clearInterval: (handle) => globalThis.clearInterval(handle as number),
    now: () => Date.now(),
};

/** Start ticking; returns a function that stops it. */
export function startDedupTicker(
    tick: (nowMs: number) => void,
    intervalMs: number = DEDUP_TICK_INTERVAL_MS,
    timers: TickerTimers = realTimers,
): () => void {
    const handle = timers.setInterval(() => tick(timers.now()), intervalMs);
    return () => timers.clearInterval(handle);
}
