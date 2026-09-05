/**
 * P4-T06: 60s NTP-style skew measurement driver.
 *
 * Architecture section 13.3 says: "Take 4 RTT samples every
 * 60 seconds." The transport (a single SKEW_PROBE round trip)
 * is the same as the P3/P4 presence-style request: a one-shot
 * envelope pair. The Tauri command and the WS exchange are
 * owned by `apps/client/src-tauri/src/room/skew.rs` and the
 * React cadence lives here.
 *
 * The hook runs a 60s `setInterval`. Each tick fires a 4-
 * sample burst, computed via a user-supplied `probeOnce`
 * function (which the parent `RoomPage` wires to the
 * `commands.clockSkewProbe` Tauri invoke). The 4 samples
 * are passed through the `compute_skew_jitter` reducer
 * (re-exposed on the JS side as a thin shim: the Rust math
 * is the source of truth, but the JS reducer is sufficient
 * for the high-level filter and is unit-testable in the
 * smoke harness).
 *
 * Test seam: in Vite's test mode the hook is replaced by
 * direct calls to `useClockSkewStore.getState().setSkewJitter`
 * (the test seam). The 60s cadence is a no-op in tests so
 * the seam is the only consumer.
 *
 * Lifecycle:
 *  - Mount once per room page (RoomPage).
 *  - On unmount: clear the timer.
 *  - On room change: clear the store so a stale skew
 *    from the previous room cannot feed into the new
 *    room's drift projection.
 */

import { useEffect, useRef } from "react";
import { useClockSkewStore } from "../stores/useClockSkewStore";

/** One NTP probe round trip. Returns a `(t0, t3, server_ts_ms)`
 *  tuple; the caller is responsible for feeding it into the
 *  Rust math (or the JS shim). The probe may fail; failures
 *  are counted as a rejected sample (architecture section
 *  13.3 explicitly says RTT > 500 ms is dropped, and a
 *  transport failure is the degenerate RTT = infinity
 *  case). */
export type SkewProbeFn = () => Promise<{
    t0_local_ms: number;
    t3_local_ms: number;
    server_ts_ms: number;
} | null>;

/** JS-side shim of `apps/client/src-tauri/src/room/skew.rs`.
 *  The Rust module is the source of truth; the JS shim
 *  exists for the rare case where the probe runs from
 *  pure-JS (the Vite test harness can call the Tauri
 *  shim directly; the React layer does not need to
 *  re-implement the NTP math). The output of the shim
 *  matches the Rust `SkewMeasurement` struct (with `null`
 *  for `None`). */
export interface JsSkewMeasurement {
    skewMs: number | null;
    jitterMs: number | null;
    samplesUsed: number;
    samplesRejected: number;
}

const JITTER_HIGH_MS = 200;
const SAMPLE_COUNT_PER_BURST = 4;

function computeJsSkewJitter(
    samples: Array<{
        t0_local_ms: number;
        t3_local_ms: number;
        server_ts_ms: number;
    }>,
): JsSkewMeasurement {
    if (samples.length === 0) {
        return { skewMs: null, jitterMs: null, samplesUsed: 0, samplesRejected: 0 };
    }
    const offsets: number[] = [];
    let rejected = 0;
    for (const s of samples) {
        const rtt = s.t3_local_ms - s.t0_local_ms;
        if (rtt < 0 || rtt > 500) {
            rejected += 1;
            continue;
        }
        const midpoint = Math.floor((s.t0_local_ms + s.t3_local_ms) / 2);
        offsets.push(s.server_ts_ms - midpoint);
    }
    if (offsets.length === 0) {
        return { skewMs: null, jitterMs: null, samplesUsed: 0, samplesRejected: rejected };
    }
    const sorted = [...offsets].sort((a, b) => a - b);
    const mid = Math.floor(sorted.length / 2);
    const skewMs =
        sorted.length % 2 === 1
            ? sorted[mid] ?? null
            : (() => {
                  const a = sorted[mid - 1];
                  const b = sorted[mid];
                  if (a === undefined || b === undefined) return null;
                  return Math.floor((a + b) / 2);
              })();
    let jitterMs: number | null = null;
    if (sorted.length >= 2 && skewMs !== null) {
        const mean = skewMs;
        let sumSq = 0;
        for (const o of sorted) {
            const d = o - mean;
            sumSq += d * d;
        }
        const variance = sumSq / sorted.length;
        jitterMs = Math.round(Math.sqrt(variance));
    }
    return {
        skewMs,
        jitterMs,
        samplesUsed: sorted.length,
        samplesRejected: rejected,
    };
}

export interface UseClockSkewOptions {
    /** One NTP probe round trip. The hook does not know
     *  how the probe is implemented (Tauri IPC,
     *  test seam, etc.); it just calls it. */
    probeOnce: SkewProbeFn;
    /** The room id; null when not in a room. When the
     *  room changes the store is cleared so a stale
     *  skew from the previous room cannot leak. */
    roomId: string | null;
    /** Disable the timer (useful for tests that drive
     *  the store directly). */
    disabled?: boolean;
}

/** P4-T06: 60s clock skew measurement driver. Mounted
 *  by `RoomPage` alongside the drift smoother. */
export function useClockSkew(opts: UseClockSkewOptions): void {
    const { probeOnce, roomId, disabled = false } = opts;
    const lastRoomIdRef = useRef<string | null>(roomId);
    // Reset the store on room change.
    useEffect(() => {
        if (lastRoomIdRef.current !== roomId) {
            useClockSkewStore.getState().clear();
            lastRoomIdRef.current = roomId;
        }
    }, [roomId]);

    useEffect(() => {
        if (disabled) return;
        if (roomId === null) return;
        let stopped = false;
        const tick = async () => {
            if (stopped) return;
            const samples = [];
            for (let i = 0; i < SAMPLE_COUNT_PER_BURST; i++) {
                if (stopped) return;
                try {
                    const s = await probeOnce();
                    if (s !== null) samples.push(s);
                } catch {
                    // Network / signaling failure counts
                    // as a rejected sample; the math
                    // function treats null as the rejected
                    // case anyway.
                }
            }
            if (stopped) return;
            const m = computeJsSkewJitter(samples);
            if (m.skewMs !== null) {
                useClockSkewStore
                    .getState()
                    .setSkewJitter(m.skewMs, m.jitterMs);
            }
        };
        // First burst fires immediately so the UI has a
        // usable value right after room entry (rather
        // than waiting 60 s for the first measurement).
        void tick();
        const id = window.setInterval(() => {
            void tick();
        }, 60_000);
        return () => {
            stopped = true;
            window.clearInterval(id);
        };
    }, [probeOnce, roomId, disabled]);
}

/** The "high jitter" threshold (architecture 13.3). */
export { JITTER_HIGH_MS };
