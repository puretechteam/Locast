/**
 * P4-T06: clock-skew + jitter state.
 *
 * The skew (server - local) and jitter (stddev of the
 * accepted NTP offsets) are derived from a burst of 4
 * RTT samples every 60 s (architecture section 13.3).
 * The transport lives in `apps/client/src-tauri/src/room/skew.rs`
 * and the React cadence lives in `useClockSkew.ts`; this
 * module is the single source of truth for the values
 * once they are computed.
 *
 * Why a store: the drift smoother (P4-T04) and the manual
 * sync hook (P4-T05) both consume the same `skewMs` so
 * the projection `expectedPositionMs` is consistent across
 * the two surfaces. A Zustand store is the smallest
 * additive surface that fits the project convention
 * (used by `usePlaybackStore`, `useRoomStore`,
 * `useViewerPositionStore`).
 *
 * Test seam: `__locastClockSkew` exposes a `setSkewJitter`
 * for Playwright. Gated on `MODE === "test"` so the
 * production bundle does NOT include it.
 */

import { create } from "zustand";

export interface ClockSkewState {
    /** Server - local wall clock, in ms. The smoother
     *  uses this to project the host's `server_ts_ms` into
     *  local time: `local_ts = server_ts - skewMs`. */
    skewMs: number | null;
    /** Standard deviation of the accepted NTP offsets,
     *  ms. Used to widen the drift thresholds (2 s -> 3 s,
     *  5 s -> 7 s) when the connection is unstable. */
    jitterMs: number | null;
    /** Monotonic ms when the values were last updated. */
    lastUpdatedMs: number | null;
    setSkewJitter: (skewMs: number | null, jitterMs: number | null) => void;
    clear: () => void;
}

export const useClockSkewStore = create<ClockSkewState>((set, get) => ({
    skewMs: null,
    jitterMs: null,
    lastUpdatedMs: null,
    setSkewJitter: (skewMs, jitterMs) => {
        set({ skewMs, jitterMs, lastUpdatedMs: Date.now() });
    },
    clear: () => {
        if (
            get().skewMs === null &&
            get().jitterMs === null &&
            get().lastUpdatedMs === null
        ) {
            return;
        }
        set({ skewMs: null, jitterMs: null, lastUpdatedMs: null });
    },
}));

/* P4-T06 test seam: in Vite's test mode, expose
 * `__locastClockSkew` on `window` so the Playwright
 * harness can drive the store directly. Gated on
 * `MODE === "test"` so it is tree-shaken from
 * production builds. */
if (import.meta.env.MODE === "test" && typeof window !== "undefined") {
    const w = window as unknown as {
        __locastClockSkew?: {
            getSkew: () => number | null;
            getJitter: () => number | null;
            setSkewJitter: (skewMs: number | null, jitterMs: number | null) => void;
            clear: () => void;
        };
    };
    w.__locastClockSkew = {
        getSkew: () => useClockSkewStore.getState().skewMs,
        getJitter: () => useClockSkewStore.getState().jitterMs,
        setSkewJitter: (skewMs, jitterMs) => {
            useClockSkewStore.getState().setSkewJitter(skewMs, jitterMs);
        },
        clear: () => {
            useClockSkewStore.getState().clear();
        },
    };
}
