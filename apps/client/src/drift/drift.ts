/**
 * P4-T04: pure drift math. Separated from React so it is
 * unit-testable without a DOM and so the same math can be
 * reused by the hook, the indicator, and any future
 * dedicated tests.
 *
 * Architecture references (verbatim from `docs/ARCHITECTURE.md`):
 *
 *  - Section 12.4 (Drift Detection):
 *      - "Compares `local_video.currentTime * 1000` to the
 *         host's last received `media_position_ms + elapsed
 *         since that command's target_server_ts -
 *         local_offset_to_server`."
 *      - "Drift is computed only when the local player is
 *         `PLAYING`."
 *  - Section 13.3 (Clock-Skew Handling):
 *      - "expected_position_ms = command.media_position_ms +
 *         (t - (command.server_ts - skew_ms))"
 *      - "If jitter_ms > 200 ms, the client increases its
 *         drift-detection threshold..."
 *  - Section 25.3.2: "drift indicator ... visible only when
 *      the smoothed offset between the local playback clock
 *      and the median participant clock exceeds 2.0 seconds"
 *  - Section 25.3.4: "thin marker for the median participant
 *      position"
 *  - Risk 9: "the drift estimate is a low-pass filter
 *      (exponential moving average with a time constant of
 *      5 seconds). The indicator only appears when the
 *      smoothed offset exceeds 2.0 seconds..."
 *
 * The math below implements all of the above verbatim. The
 * P4-T04 task ships the smoother + indicator WITHOUT the
 * NTP-style clock skew measurement (P4-T06); `skewMs`
 * therefore defaults to 0 and the `jitterMs` widening is
 * not applied. P4-T06 will plug real values into the same
 * inputs without changing this module's API.
 *
 * Conventions:
 *  - `localMs` is `local_video.currentTime * 1000` (ms).
 *  - `expectedMs` is what the host's last accepted command
 *    says the room's position should be, projected forward
 *    by the elapsed time since that command was stamped by
 *    the server (with the local-to-server skew applied).
 *  - `driftMs = localMs - expectedMs` -- positive = local
 *    AHEAD of the host; negative = local BEHIND.
 *  - The EMA is a low-pass filter on `driftMs`. The smoothed
 *    value retains the sign so the UI can show "ahead"
 *    vs "behind" direction.
 *  - Visibility uses `|smoothedDriftMs| > INDICATOR_THRESHOLD_MS`
 *    (strict `>` per the roadmap's "smoothed offset > 2 s"
 *    and §25.3.2's "exceeds 2.0 seconds").
 */

/** Roadmap / §25.3.2 default indicator threshold. */
export const INDICATOR_THRESHOLD_MS = 2000;

/** Architecture §13.3: the severe band threshold.
 *  When the smoothed drift exceeds this value the user
 *  is in a "severe drift" state. The UI does not currently
 *  show a separate "severe" surface (P4-T04's DriftIndicator
 *  is the only drift UI), but the threshold is exposed
 *  for the future YouAreBehindModal (P4-T05 follow-on)
 *  and for telemetry. */
export const SEVERE_THRESHOLD_MS = 5000;

/** P4-T06 / architecture §13.3: when the stddev of the
 *  four-sample NTP offsets (jitter) exceeds 200 ms, the
 *  client is on an unstable connection; the indicator
 *  and severe thresholds are widened to avoid flicker
 *  (2 s -> 3 s; 5 s -> 7 s). */
export const JITTER_HIGH_MS = 200;
export const INDICATOR_THRESHOLD_HIGH_MS = 3000;
export const SEVERE_THRESHOLD_HIGH_MS = 7000;

/** Select the indicator + severe threshold based on
 *  the current jitter. Pure function; called by
 *  `deriveDriftSample`. */
export function activeThresholds(
    jitterMs: number | null,
): { indicator: number; severe: number } {
    if (jitterMs !== null && jitterMs > JITTER_HIGH_MS) {
        return {
            indicator: INDICATOR_THRESHOLD_HIGH_MS,
            severe: SEVERE_THRESHOLD_HIGH_MS,
        };
    }
    return {
        indicator: INDICATOR_THRESHOLD_MS,
        severe: SEVERE_THRESHOLD_MS,
    };
}

/** Risk 9 smoothing time constant (seconds). */
export const SMOOTHING_TAU_SECONDS = 5;

/** A POSITION_REPORT older than this is treated as stale
 *  for the purposes of median computation (§12.4 expects
 *  freshness; P4-T08's 15s heartbeat is not shipped yet, so
 *  we choose a value just below it that excludes obviously
 *  stale data while still tolerating a 1 Hz cadence with
 *  a few seconds of jitter). */
export const STALE_REPORT_MS = 10_000;

/** Smoothing EMA factor for a 1 Hz sampler: alpha = 1 -
 *  exp(-1/τ) with τ=5s. Exported for tests. */
export const SMOOTHING_ALPHA_1HZ =
    1 - Math.exp(-1 / SMOOTHING_TAU_SECONDS);

/** A snapshot of every input the drift math needs. The
 *  hook owns the timers and produces one of these per
 *  tick; the math is pure. */
export interface DriftSampleInput {
    /** The local `<video>` position, integer ms. */
    localMs: number;
    /** The host's last accepted command, or null when the
     *  room has no playback yet (or the local user is the
     *  host and has not issued any command). */
    hostCommand: {
        mediaPositionMs: number;
        serverTsMs: number;
    } | null;
    /** Local wall clock at sample time, integer ms. */
    nowMs: number;
    /** Local-to-server skew (server - local), per §13.3.
     *  Defaults to 0 in P4-T04; P4-T06 will populate. */
    skewMs: number;
}

/** Output of one drift sample. `smoothedDriftMs` is null
 *  until the EMA has accumulated at least one real
 *  sample (a sample where `hostCommand` was present and
 *  the local media was PLAYING at the time of the tick). */
export interface DriftSample {
    /** Raw drift in this sample (local - expected), ms. */
    rawDriftMs: number | null;
    /** EMA-smoothed drift, ms. */
    smoothedDriftMs: number | null;
    /** Expected room position at `nowMs`, ms. */
    expectedMs: number | null;
    /** True iff the indicator should be visible
     *  (`|smoothedDriftMs| > active.indicator`). */
    indicatorVisible: boolean;
    /** True iff the smoothed drift has crossed the
     *  severe band (`|smoothedDriftMs| >= active.severe`).
     *  Used by the future YouAreBehindModal (P4-T05
     *  follow-on). The current DriftIndicator does not
     *  surface this directly. */
    severeVisible: boolean;
    /** Direction of the smoothed drift: "ahead", "behind",
     *  or "none" when no smoothed value is available. */
    direction: "ahead" | "behind" | "none";
}

/** Per-tick result of the smoother; pairs the previous
 *  state with the new sample so the hook can drive a
 *  React update deterministically. */
export interface DriftState {
    smoothedDriftMs: number | null;
    rawDriftMs: number | null;
    lastSampleAtMs: number | null;
    /** Number of real samples accumulated so far. Used to
     *  decide when the EMA is "warmed up" enough to be
     *  exposed to the UI. */
    sampleCount: number;
}

/** Build the initial drift state. */
export function initialDriftState(): DriftState {
    return {
        smoothedDriftMs: null,
        rawDriftMs: null,
        lastSampleAtMs: null,
        sampleCount: 0,
    };
}

/** Compute the expected room position at `nowMs` from the
 *  host's last accepted command. Mirrors §13.3's formula
 *  verbatim. */
export function expectedPositionMs(
    hostCommand: DriftSampleInput["hostCommand"],
    nowMs: number,
    skewMs: number,
): number | null {
    if (hostCommand === null) return null;
    // Server-stamped time at command acceptance, projected
    // into local time by subtracting the local-to-server
    // skew.
    const localTs = hostCommand.serverTsMs - skewMs;
    const elapsed = Math.max(0, nowMs - localTs);
    return hostCommand.mediaPositionMs + elapsed;
}

/** Compute the raw drift for one sample, or null when the
 *  sample cannot produce a meaningful drift (no host
 *  command yet, or local time is before the command was
 *  stamped -- defensive). */
export function computeRawDrift(input: DriftSampleInput): number | null {
    const expected = expectedPositionMs(
        input.hostCommand,
        input.nowMs,
        input.skewMs,
    );
    if (expected === null) return null;
    return input.localMs - expected;
}

/** Update the EMA state with a new raw sample. Returns a
 *  fresh state object (no mutation). When `raw` is null
 *  the state is preserved unchanged so a missing host
 *  command does not feed noise into the smoother. */
export function applyDriftSample(
    state: DriftState,
    raw: number | null,
    nowMs: number,
): DriftState {
    if (raw === null) {
        return {
            ...state,
            lastSampleAtMs: nowMs,
        };
    }
    if (state.smoothedDriftMs === null) {
        // Seed: the first real sample sets the smoothed
        // value directly. Subsequent samples use the EMA
        // update rule. Seeding avoids an initial lag where
        // the indicator's first frame is 0 even though the
        // user may have been behind for seconds.
        return {
            smoothedDriftMs: raw,
            rawDriftMs: raw,
            lastSampleAtMs: nowMs,
            sampleCount: state.sampleCount + 1,
        };
    }
    const next =
        state.smoothedDriftMs + SMOOTHING_ALPHA_1HZ * (raw - state.smoothedDriftMs);
    return {
        smoothedDriftMs: next,
        rawDriftMs: raw,
        lastSampleAtMs: nowMs,
        sampleCount: state.sampleCount + 1,
    };
}

/** Derive a UI-ready DriftSample from the current state. */
export function deriveDriftSample(
    state: DriftState,
    jitterMs: number | null = null,
): DriftSample {
    const expected: number | null = null; // expected is recomputed by callers per-tick
    if (state.smoothedDriftMs === null) {
        return {
            rawDriftMs: state.rawDriftMs,
            smoothedDriftMs: null,
            expectedMs: expected,
            indicatorVisible: false,
            severeVisible: false,
            direction: "none",
        };
    }
    const abs = Math.abs(state.smoothedDriftMs);
    // P4-T06: pick the threshold based on jitter. High
    // jitter (>= 200 ms) widens the visibility threshold
    // (2 s -> 3 s) and the severe band (5 s -> 7 s) per
    // architecture §13.3.
    const t = activeThresholds(jitterMs);
    const direction: DriftSample["direction"] =
        state.smoothedDriftMs > 0
            ? "ahead"
            : state.smoothedDriftMs < 0
              ? "behind"
              : "none";
    return {
        rawDriftMs: state.rawDriftMs,
        smoothedDriftMs: state.smoothedDriftMs,
        expectedMs: expected,
        indicatorVisible: abs > t.indicator,
        severeVisible: abs >= t.severe,
        direction,
    };
}

/** A single participant's contribution to the room median.
 *  `predictedMs` is the participant's position projected
 *  forward by `(nowMs - receivedAtMs)` so a 1 Hz report
 *  from someone whose media is still PLAYING does not
 *  become instantly stale. */
export interface MedianParticipant {
    userId: string;
    /** Last reported position, integer ms. */
    mediaPositionMs: number;
    /** True iff the report says the participant is currently
     *  playing (vs. paused / buffering). */
    playing: boolean;
    /** Local wall clock at arrival, integer ms. */
    receivedAtMs: number;
}

/** Compute the room median from a set of fresh, playing
 *  participants' predicted positions.
 *
 *  Inclusion rules (per the recon + architecture intent):
 *   - exclude the local user (callers filter before passing)
 *   - exclude stale reports (> STALE_REPORT_MS old)
 *   - exclude paused participants (drift is only meaningful
 *     for media that is actively advancing; a paused viewer
 *     should not pull the median)
 *   - if the input is empty, return null (no median, no
 *     marker on the seek bar, no drift UI)
 *
 *  Math:
 *   - Predict each participant's position at `nowMs` by
 *     advancing by `(nowMs - receivedAtMs)`. This is the
 *     "where would they be if they kept playing" number.
 *   - Sort the predicted positions and return the middle
 *     value (for odd N) or the average of the two middles
 *     (for even N). This is the canonical definition of
 *     median; the architecture does not specify a tie-break
 *     for even N so the implementation defaults to the
 *     arithmetic mean.
 */
export function computeRoomMedian(
    participants: readonly MedianParticipant[],
    nowMs: number,
): number | null {
    const predicted: number[] = [];
    for (const p of participants) {
        if (!p.playing) continue;
        const age = nowMs - p.receivedAtMs;
        if (age < 0 || age > STALE_REPORT_MS) continue;
        // Predict the participant's current position by
        // advancing their last reported position by the
        // elapsed time. This is the same "expected position"
        // math the smoother uses, applied to remote
        // telemetry.
        predicted.push(p.mediaPositionMs + age);
    }
    if (predicted.length === 0) return null;
    predicted.sort((a, b) => a - b);
    const mid = Math.floor(predicted.length / 2);
    if (predicted.length % 2 === 1) {
        return predicted[mid] ?? null;
    }
    // Even N: average the two middle values.
    const a = predicted[mid - 1];
    const b = predicted[mid];
    if (a === undefined || b === undefined) return null;
    return (a + b) / 2;
}

/** Compute a signed drift between the local position and
 *  the room median, in the same sign convention as the
 *  per-host smoother (`local - median`; positive = local
 *  AHEAD). Returns null when there is no median. */
export function computeDriftVsMedian(
    localMs: number,
    medianMs: number | null,
): number | null {
    if (medianMs === null) return null;
    return localMs - medianMs;
}

/* -----------------------------------------------------------------------
 * P4-T05: manual sync ("Sync to Host") target calculation.
 *
 * The host target is the host's `lastApplied.media_position_ms`
 * projected forward by `(nowMs - server_ts_ms)`. This is the
 * same `expectedPositionMs` formula the drift smoother uses;
 * colocated here so the sync target lives next to the drift
 * math (single source of truth) and is unit-testable without
 * React.
 *
 * The `isHost` flag determines which branch the UI takes
 * (viewer -> local-only DOM seek; host -> local + PLAYBACK_CMD).
 * The `canSync` gate disables the button until the user is in
 * a room with a matching host command and ready media.
 * --------------------------------------------------------------------- */

export interface ManualSyncTarget {
    /** The host's expected media position in integer ms.
     *  Null when no host command has been received yet,
     *  when the local user is not in a room, or when the
     *  cached host command is from a different room. */
    hostTargetMs: number | null;
    /** True when the local user is the room's current
     *  host. Controls which branch the Sync button's
     *  onClick takes. */
    isHost: boolean;
    /** True when the Sync button should be enabled:
     *  in room + media ready + matching host command
     *  present + hostTargetMs computable. */
    canSync: boolean;
}

/** Pure target computation. Extracted from `useManualSync`
 *  so the same math is testable without React + a DOM. */
export function computeSyncTarget(args: {
    roomId: string | null;
    isHost: boolean;
    lastApplied:
        | { room_id: string; media_position_ms: number; server_ts_ms: number }
        | null;
    mediaReady: boolean;
    nowMs: number;
    skewMs: number;
}): ManualSyncTarget {
    const inRoom = args.roomId !== null;
    const sameRoom =
        args.lastApplied !== null && args.lastApplied.room_id === args.roomId;
    const hasHostCommand = inRoom && sameRoom;
    // Pull the host command into a local const so the
    // strict-mode null-narrowing carries into the
    // `expectedPositionMs` call below.
    const last = args.lastApplied;
    const hostTargetMs =
        hasHostCommand && last !== null
            ? expectedPositionMs(
                  {
                      mediaPositionMs: last.media_position_ms,
                      serverTsMs: last.server_ts_ms,
                  },
                  args.nowMs,
                  args.skewMs,
              )
            : null;
    return {
        hostTargetMs,
        isHost: args.isHost,
        canSync:
            inRoom && args.mediaReady && hasHostCommand && hostTargetMs !== null,
    };
}
