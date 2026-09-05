/**
 * P4-T04: drift sampler hook (architecture §12.4 + §13.3
 * + Risk 9 + roadmap P4-T04).
 *
 * Owns a 1 Hz timer that reads the local media position
 * and the host's last accepted command, computes the raw
 * drift, feeds it through an exponential moving average
 * (τ = 5 s, Risk 9), and exposes the smoothed value +
 * visibility flag for the UI.
 *
 * Lifecycle:
 *  - Mounted once per room (P4-T04 calls the hook from
 *    `RoomPage`).
 *  - Stops the timer on unmount, on room change (when
 *    `roomId` switches), and when there is no `hostCommand`
 *    yet (nothing to drift against). The smoother state
 *    is RESET on room change so old samples cannot leak
 *    from one room into another.
 *  - Skips a tick when the local media is paused or
 *    unknown (per §12.4: "Drift is computed only when the
 *    local player is `PLAYING`").
 *  - Skips a tick when the local media has no duration
 *    (no `<video>` element attached, or the source has
 *    not loaded yet). The 1 Hz cadence is preserved; we
 *    simply do not feed the EMA during those windows.
 *
 * No automatic correction: the hook NEVER mutates the
 * authoritative playback state. The output is observation
 * only.
 *
 * The hook is a thin React wrapper around the pure-math
 * module `./drift.ts`. The math is unit-tested separately
 * (see `drift.smoke.ts`); the hook's job is lifecycle +
 * gluing.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { useClockSkewStore } from "../stores/useClockSkewStore";
import { usePlaybackStore, type PlaybackStateEvent } from "../stores/usePlaybackStore";
import {
    applyDriftSample,
    computeDriftVsMedian,
    computeRawDrift,
    computeRoomMedian,
    deriveDriftSample,
    expectedPositionMs,
    initialDriftState,
    type DriftSample,
    type DriftState,
    type MedianParticipant,
} from "./drift";
import { getForcedHostCommand, readLocalSeekTick, resetLocalSeekTick, setForcedHostCommand } from "./testSeams";

/** What the hook needs to read from the local media element
 *  on every tick. The consumer provides this; the hook
 *  itself is DOM-agnostic so the math stays pure. */
export interface DriftLocalSource {
    /** Returns the local `<video>.currentTime * 1000`, or
     *  null when the local media is not ready. */
    getLocalMs: () => number | null;
    /** Returns `!video.paused` (i.e. true iff the local
     *  media is actively playing). */
    getLocalPlaying: () => boolean;
}

export interface DriftSmootherResult extends DriftSample {
    /** Current room median, or null when not enough data. */
    roomMedianMs: number | null;
    /** Drift between local and the room median (positive =
     *  local AHEAD), or null when no median is available. */
    driftVsMedianMs: number | null;
    /** True while the 1 Hz timer is active. */
    active: boolean;
    /** Number of real samples accumulated so far (debug +
     *  test visibility). */
    sampleCount: number;
}

export interface UseDriftSmootherOptions extends DriftLocalSource {
    /** Current room id; null when not in a room. The hook
     *  resets its smoother state when this changes. */
    roomId: string | null;
    /** Per-viewer latest-position snapshot, used for the
     *  room median. The hook filters out the local user. */
    remoteParticipants: readonly MedianParticipant[];
    /** The local user's id; used to exclude self-reports
     *  from the room median. */
    localUserId: string | null;
    /** Skew offset (server - local), ms. Defaults to 0 in
     *  P4-T04; P4-T06 will populate. */
    skewMs?: number;
}

/**
 * The 1 Hz drift sampler. See the module doc above for
 * the full lifecycle / responsibility story.
 */
export function useDriftSmoother(opts: UseDriftSmootherOptions): DriftSmootherResult {
    const {
        roomId,
        remoteParticipants,
        localUserId,
        getLocalMs,
        getLocalPlaying,
        skewMs = 0,
    } = opts;

    // The smoother's internal state (not React-reactive;
    // we only push to React state on the 1 Hz tick so the
    // hook does not over-render).
    const stateRef = useRef<DriftState>(initialDriftState());
    const [tick, setTick] = useState(0);

    // P4-T05 (test-mode only): the host target + the
    // local-seek counter are module-level helpers in
    // `./testSeams.ts` so the smoother's seam AND the
    // `useManualSync` hook share the same values. The
    // tick body's 1 Hz sample still reads the playback
    // store directly (the override is honored only by
    // the seam's `hostTargetMs` and by `useManualSync`).

    // Reset on room change so samples from a previous room
    // cannot leak into the new room's drift EMA.
    const lastRoomIdRef = useRef<string | null>(roomId);
    useEffect(() => {
        if (lastRoomIdRef.current !== roomId) {
            stateRef.current = initialDriftState();
            lastRoomIdRef.current = roomId;
            // Clear any test override so it does not
            // bleed into the new room.
            setForcedHostCommand(null);
            setTick((t) => t + 1);
        }
    }, [roomId]);

    useEffect(() => {
        if (roomId === null) return;
        let stopped = false;
        const id = window.setInterval(() => {
            if (stopped) return;
            // Read live local + host command every tick.
            // The store is the source of truth for the
            // host's last accepted PLAYBACK_CMD (per
            // P4-T01/P4-T02).
            const localMs = getLocalMs();
            const playing = getLocalPlaying();
            const hostEvent: PlaybackStateEvent | null =
                usePlaybackStore.getState().lastApplied;
            const hostCommand =
                hostEvent !== null && hostEvent.room_id === roomId
                    ? {
                          mediaPositionMs: hostEvent.media_position_ms,
                          serverTsMs: hostEvent.server_ts_ms,
                      }
                    : null;
            // P4-T06: the drift projection's
            // `skewMs` comes from the measured
            // server-local clock offset (architecture
            // section 13.3). The store is the single
            // source of truth; until the first NTP
            // measurement is available, `skewMs` is
            // null and the projection is equivalent
            // to the v1 (skew = 0) behavior so existing
            // tests / the P4-T04 default continue to
            // pass.
            const storedSkewMs =
                useClockSkewStore.getState().skewMs;
            const effectiveSkewMs = storedSkewMs ?? skewMs;
            // §12.4: drift is computed only when the local
            // player is PLAYING. A paused local player
            // produces no raw drift, so the EMA is not
            // fed (the apply function preserves state on
            // null input).
            const raw =
                localMs === null || !playing
                    ? null
                    : computeRawDrift({
                          localMs,
                          hostCommand,
                          nowMs: Date.now(),
                          skewMs: effectiveSkewMs,
                      });
            stateRef.current = applyDriftSample(
                stateRef.current,
                raw,
                Date.now(),
            );
            setTick((t) => (t + 1) % 1_000_000);
        }, 1000);
        return () => {
            stopped = true;
            window.clearInterval(id);
        };
    }, [roomId, getLocalMs, getLocalPlaying, skewMs]);

    // Compute the room median (and the local-vs-median
    // drift) from the current remote participants + local
    // position. This re-derives on every render of the
    // consumer; the median math is O(N log N) where N is
    // the participant count (≤ 8 in v1, see
    // `WelcomeConfig::max_room_size`), so this is cheap.
    // We use `useMemo` to skip the work when the inputs
    // are referentially identical.
    const roomMedianMs = useMemo<number | null>(() => {
        if (roomId === null) return null;
        if (localUserId === null) {
            return computeRoomMedian(remoteParticipants, Date.now());
        }
        const filtered = remoteParticipants.filter(
            (p) => p.userId !== localUserId,
        );
        return computeRoomMedian(filtered, Date.now());
        // tick is in the dep list so re-renders triggered
        // by the smoother 1 Hz tick also refresh the
        // median label.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [roomId, localUserId, remoteParticipants, tick]);

    const localMsNow = getLocalMs();
    const driftVsMedianMs =
        localMsNow === null
            ? null
            : computeDriftVsMedian(localMsNow, roomMedianMs);

    // P4-T06: read the current jitter from the clock
    // skew store. `deriveDriftSample` widens the
    // indicator + severe thresholds when jitter is high
    // (architecture section 13.3). Until the first NTP
    // measurement is available, jitter is null and the
    // defaults apply.
    const jitterMs = useClockSkewStore.getState().jitterMs;
    const sample = deriveDriftSample(stateRef.current, jitterMs);

    // P4-T04 test seam (extended in P4-T05): in Vite's
    // test mode, expose the current smoother state on
    // `window.__locastDrift` so Playwright can drive
    // deterministic scenarios (e.g. "set the smoother
    // to +3000 ms" without waiting for the 1 Hz timer +
    // a real `<video>` element). P4-T05 adds:
    //   - `hostTargetMs`: the host's expected position
    //     at the moment of capture, so tests can assert
    //     "after click, the local <video>.currentTime is
    //     hostTargetMs / 1000".
    //   - `forceHostCommandForTest`: replaces the
    //     internal `hostCommand` derivation for the
    //     duration of one test (does not persist across
    //     re-renders) so the Sync button's `canSync`
    //     gate is testable without emitting a real
    //     `playback://state` event.
    //
    // The seam is gated on `MODE === "test"` so it is
    // tree-shaken from production builds.
    useEffect(() => {
        if (import.meta.env.MODE !== "test") return;
        const w = window as unknown as {
            __locastDrift?: {
                getSample: () => DriftSmootherResult;
                setSmoothed: (v: number | null) => void;
                hostTargetMs: () => number | null;
                forceHostCommandForTest: (p: {
                    room_id: string;
                    media_position_ms: number;
                    server_ts_ms: number;
                } | null) => void;
                readLocalSeekTick: () => number;
                resetLocalSeekTick: () => void;
                resetForcedHostCommand: () => void;
            };
        };
        const live: DriftSmootherResult = {
            ...sample,
            roomMedianMs,
            driftVsMedianMs,
            active: roomId !== null,
            sampleCount: stateRef.current.sampleCount,
        };
        // P4-T05: the host target as of "now" is
        // `expectedPositionMs(hostCommand, Date.now(), 0)`.
        // The host command is read from the playback
        // store (same source the smoother's tick body
        // uses). In test mode we honor the seam's
        // module-level override first so tests can
        // inject a synthetic command without going
        // through the playback event bridge.
        const computeHostTarget = (): number | null => {
            const override = getForcedHostCommand();
            const hc =
                override !== null
                    ? override
                    : (() => {
                          const ev = usePlaybackStore.getState().lastApplied;
                          if (ev === null) return null;
                          if (roomId !== null && ev.room_id !== roomId) return null;
                          return {
                              mediaPositionMs: ev.media_position_ms,
                              serverTsMs: ev.server_ts_ms,
                          };
                      })();
            if (hc === null) return null;
            return expectedPositionMs(hc, Date.now(), skewMs);
        };
        w.__locastDrift = {
            getSample: () => live,
            setSmoothed: (v) => {
                stateRef.current = {
                    ...stateRef.current,
                    smoothedDriftMs: v,
                    rawDriftMs: v,
                    lastSampleAtMs: Date.now(),
                };
                setTick((t) => (t + 1) % 1_000_000);
            },
            hostTargetMs: () => computeHostTarget(),
            forceHostCommandForTest: (payload) => {
                setForcedHostCommand(
                    payload === null
                        ? null
                        : {
                              mediaPositionMs: payload.media_position_ms,
                              serverTsMs: payload.server_ts_ms,
                          },
                );
                setTick((t) => (t + 1) % 1_000_000);
            },
            // P4-T05: the `useManualSync` hook bumps a
            // module-level counter every time the local
            // DOM seek completes (test mode only). The
            // Chromium Vite harness cannot observe the
            // actual `<video>.currentTime` write, so the
            // counter is the test's only signal that the
            // local seek ran.
            readLocalSeekTick: () => readLocalSeekTick(),
            resetLocalSeekTick: () => resetLocalSeekTick(),
            // P4-T05: clear the forced host command
            // override (e.g. when a test changes rooms
            // between scenarios).
            resetForcedHostCommand: () => setForcedHostCommand(null),
        };
        return () => {
            if (w.__locastDrift) delete w.__locastDrift;
        };
    }, [sample, roomMedianMs, driftVsMedianMs, roomId, skewMs]);

    return {
        ...sample,
        roomMedianMs,
        driftVsMedianMs,
        active: roomId !== null,
        sampleCount: stateRef.current.sampleCount,
    };
}
