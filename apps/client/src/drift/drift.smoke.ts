// P4-T04: runnable smoke test for the pure drift math.
//
// The repo does not yet have a Vitest test runner (P0-T04
// TODO). Node 22.6+ has stable `--experimental-strip-types`
// support for running plain `.ts` files; we use it here so
// the math is exercised end-to-end without a bundler /
// test framework dependency.
//
// Run via: `pnpm -C apps/client smoke:drift` (script defined
// in package.json). Intended for CI and local verification
// of the math invariants; the Playwright suite covers the
// UI visibility separately.

import {
    INDICATOR_THRESHOLD_MS,
    INDICATOR_THRESHOLD_HIGH_MS,
    JITTER_HIGH_MS,
    SEVERE_THRESHOLD_MS,
    SEVERE_THRESHOLD_HIGH_MS,
    SMOOTHING_ALPHA_1HZ,
    STALE_REPORT_MS,
    activeThresholds,
    applyDriftSample,
    computeDriftVsMedian,
    computeRawDrift,
    computeRoomMedian,
    computeSyncTarget,
    deriveDriftSample,
    expectedPositionMs,
    initialDriftState,
} from "./drift.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

process.stdout.write("drift math smoke\n");

// ----- expectedPositionMs -----
process.stdout.write("expectedPositionMs\n");
check("null hostCommand => null", expectedPositionMs(null, 1000, 0) === null);
check(
    "now == server_ts (no elapsed) => mediaPositionMs",
    expectedPositionMs({ mediaPositionMs: 5000, serverTsMs: 1000 }, 1000, 0) === 5000,
);
check(
    "advances by elapsed time (zero skew)",
    expectedPositionMs({ mediaPositionMs: 10_000, serverTsMs: 1_000 }, 6_000, 0) === 15_000,
);
check(
    "applies skew (server ahead) => larger expected",
    expectedPositionMs({ mediaPositionMs: 10_000, serverTsMs: 1_000 }, 6_000, 500) === 15_500,
);
check(
    "negative elapsed clamped to 0",
    expectedPositionMs({ mediaPositionMs: 10_000, serverTsMs: 5_000 }, 1_000, 0) === 10_000,
);

// ----- computeRawDrift -----
process.stdout.write("computeRawDrift\n");
check(
    "null hostCommand => null",
    computeRawDrift({ localMs: 10_000, hostCommand: null, nowMs: 5_000, skewMs: 0 }) === null,
);
check(
    "local == expected => 0",
    computeRawDrift({
        localMs: 10_000,
        hostCommand: { mediaPositionMs: 10_000, serverTsMs: 1_000 },
        nowMs: 1_000,
        skewMs: 0,
    }) === 0,
);
check(
    "local ahead => positive",
    computeRawDrift({
        localMs: 11_000,
        hostCommand: { mediaPositionMs: 10_000, serverTsMs: 1_000 },
        nowMs: 1_000,
        skewMs: 0,
    }) === 1_000,
);
check(
    "local behind => negative",
    computeRawDrift({
        localMs: 9_000,
        hostCommand: { mediaPositionMs: 10_000, serverTsMs: 1_000 },
        nowMs: 1_000,
        skewMs: 0,
    }) === -1_000,
);

// ----- EMA smoother -----
process.stdout.write("applyDriftSample (EMA)\n");
{
    const s = applyDriftSample(initialDriftState(), 3000, 1_000);
    check("seed: first sample sets smoothed directly", s.smoothedDriftMs === 3000);
    check("seed: sampleCount = 1", s.sampleCount === 1);

    const a = applyDriftSample(initialDriftState(), 1000, 1_000);
    const b = applyDriftSample(a, null, 2_000);
    check("null sample is no-op (preserves smoothed)", b.smoothedDriftMs === 1000);

    let s2 = applyDriftSample(initialDriftState(), 0, 1_000);
    s2 = applyDriftSample(s2, 5000, 2_000);
    const expectedAfterOne = SMOOTHING_ALPHA_1HZ * 5000;
    check(
        "step 0->5000 after one post-seed sample",
        s2.smoothedDriftMs !== null &&
            Math.abs(s2.smoothedDriftMs - expectedAfterOne) < 1e-6,
    );

    let s3 = applyDriftSample(initialDriftState(), 0, 1_000);
    for (let i = 0; i < 100; i++) {
        s3 = applyDriftSample(s3, 5000, (i + 2) * 1000);
    }
    check(
        "100 step samples converge to 5000",
        s3.smoothedDriftMs !== null && Math.abs(s3.smoothedDriftMs - 5000) < 1e-3,
    );
}

// ----- deriveDriftSample -----
process.stdout.write("deriveDriftSample (visibility)\n");
{
    const fresh = (v: number) => ({
        ...initialDriftState(),
        smoothedDriftMs: v,
        rawDriftMs: v,
        lastSampleAtMs: 1000,
        sampleCount: 1,
    });
    check(
        "no smoothed value => not visible",
        deriveDriftSample(initialDriftState()).indicatorVisible === false,
    );
    check(
        `${INDICATOR_THRESHOLD_MS - 500}ms hidden (strict >)`,
        deriveDriftSample(fresh(INDICATOR_THRESHOLD_MS - 500)).indicatorVisible === false,
    );
    check(
        `${INDICATOR_THRESHOLD_MS + 1}ms visible`,
        deriveDriftSample(fresh(INDICATOR_THRESHOLD_MS + 1)).indicatorVisible === true,
    );
    check(
        `-${INDICATOR_THRESHOLD_MS + 500}ms visible AND 'behind'`,
        deriveDriftSample(fresh(-(INDICATOR_THRESHOLD_MS + 500))).direction === "behind",
    );
    check(
        `+${INDICATOR_THRESHOLD_MS + 500}ms visible AND 'ahead'`,
        deriveDriftSample(fresh(INDICATOR_THRESHOLD_MS + 500)).direction === "ahead",
    );
    check(
        `exactly at threshold (${INDICATOR_THRESHOLD_MS}ms) NOT visible (strict >)`,
        deriveDriftSample(fresh(INDICATOR_THRESHOLD_MS)).indicatorVisible === false,
    );
}

// ----- computeRoomMedian -----
process.stdout.write("computeRoomMedian\n");
{
    const now = 10_000;
    const f = (over: Partial<{ userId: string; mediaPositionMs: number; playing: boolean; receivedAtMs: number }>) => ({
        userId: "u",
        mediaPositionMs: 0,
        playing: true,
        receivedAtMs: now,
        ...over,
    });
    check("empty => null", computeRoomMedian([], now) === null);

    const one = f({ userId: "v1", mediaPositionMs: 5_000, receivedAtMs: 7_000 });
    check(
        "single viewer => 5000 + 3000 = 8000",
        computeRoomMedian([one], now) === 8000,
    );

    const aa = f({ userId: "v1", mediaPositionMs: 5_000, receivedAtMs: 9_000 });
    const bb = f({ userId: "v2", mediaPositionMs: 9_000, receivedAtMs: 9_500 });
    check(
        "two viewers: mid pair avg = (6000+9500)/2 = 7750",
        computeRoomMedian([aa, bb], now) === 7750,
    );

    const t1 = f({ userId: "v1", mediaPositionMs: 0, receivedAtMs: 10_000 });
    const t2 = f({ userId: "v2", mediaPositionMs: 5_000, receivedAtMs: 10_000 });
    const t3 = f({ userId: "v3", mediaPositionMs: 10_000, receivedAtMs: 10_000 });
    check(
        "three viewers: middle value 5000",
        computeRoomMedian([t1, t2, t3], now) === 5000,
    );

    const playing = f({ userId: "v1", mediaPositionMs: 5_000, receivedAtMs: 10_000 });
    const paused = f({
        userId: "v2",
        mediaPositionMs: 0,
        playing: false,
        receivedAtMs: 10_000,
    });
    check(
        "paused excluded => 5000",
        computeRoomMedian([playing, paused], now) === 5000,
    );

    const fresher = f({ userId: "v1", mediaPositionMs: 5_000, receivedAtMs: 9_000 });
    const stale = f({ userId: "v2", mediaPositionMs: 7_000, receivedAtMs: -1_000 });
    check(
        "stale excluded => fresher's predicted 6000",
        computeRoomMedian([fresher, stale], now) === 6000,
    );

    const boundary = f({
        userId: "v2",
        mediaPositionMs: 7_000,
        receivedAtMs: now - STALE_REPORT_MS,
    });
    check(
        "at STALE_REPORT_MS boundary INCLUDED (age == limit, not > limit) => (6000+17000)/2 = 11500",
        computeRoomMedian([fresher, boundary], now) === 11_500,
    );

    const allPaused = f({
        userId: "v1",
        mediaPositionMs: 5_000,
        playing: false,
        receivedAtMs: 9_000,
    });
    check("all paused => null", computeRoomMedian([allPaused], now) === null);

    const future = f({ userId: "v1", mediaPositionMs: 5_000, receivedAtMs: 20_000 });
    check(
        "future receivedAtMs excluded (clock skew guard) => null",
        computeRoomMedian([future], now) === null,
    );
}

// ----- computeDriftVsMedian -----
process.stdout.write("computeDriftVsMedian\n");
check("null median => null", computeDriftVsMedian(5000, null) === null);
check("local > median => positive (ahead)", computeDriftVsMedian(7000, 5000) === 2000);
check("local < median => negative (behind)", computeDriftVsMedian(3000, 5000) === -2000);
check("local == median => 0", computeDriftVsMedian(5000, 5000) === 0);

// ----- computeSyncTarget (P4-T05) -----
process.stdout.write("computeSyncTarget (P4-T05)\n");
{
    // Not in a room: hostTargetMs null, canSync false.
    const t1 = computeSyncTarget({
        roomId: null,
        isHost: false,
        lastApplied: null,
        mediaReady: true,
        nowMs: 1000,
        skewMs: 0,
    });
    check("not in room => hostTargetMs null", t1.hostTargetMs === null);
    check("not in room => canSync false", t1.canSync === false);
    check("not in room => isHost preserved", t1.isHost === false);

    // In a room but no host command yet: canSync false.
    const t2 = computeSyncTarget({
        roomId: "r-1",
        isHost: false,
        lastApplied: null,
        mediaReady: true,
        nowMs: 1000,
        skewMs: 0,
    });
    check("no host command => canSync false", t2.canSync === false);
    check("no host command => hostTargetMs null", t2.hostTargetMs === null);

    // In a room with a host command for a DIFFERENT room:
    // canSync false (defense in depth).
    const t3 = computeSyncTarget({
        roomId: "r-1",
        isHost: false,
        lastApplied: { room_id: "r-2", media_position_ms: 1000, server_ts_ms: 1000 },
        mediaReady: true,
        nowMs: 1000,
        skewMs: 0,
    });
    check("cross-room host command => canSync false", t3.canSync === false);

    // In a room with a matching host command but media
    // not ready: canSync false.
    const t4 = computeSyncTarget({
        roomId: "r-1",
        isHost: false,
        lastApplied: { room_id: "r-1", media_position_ms: 1000, server_ts_ms: 1000 },
        mediaReady: false,
        nowMs: 1000,
        skewMs: 0,
    });
    check("media not ready => canSync false", t4.canSync === false);

    // Happy path: in a room, matching host command,
    // media ready. hostTargetMs projects forward by
    // (nowMs - serverTsMs).
    const t5 = computeSyncTarget({
        roomId: "r-1",
        isHost: false,
        lastApplied: { room_id: "r-1", media_position_ms: 10_000, server_ts_ms: 1_000 },
        mediaReady: true,
        nowMs: 6_000,
        skewMs: 0,
    });
    check("happy path => canSync true", t5.canSync === true);
    check(
        "happy path => hostTargetMs = 10000 + (6000-1000) = 15000",
        t5.hostTargetMs === 15_000,
    );

    // isHost is preserved (the hook exposes it; the UI
    // uses it to choose the branch).
    const t6 = computeSyncTarget({
        roomId: "r-1",
        isHost: true,
        lastApplied: { room_id: "r-1", media_position_ms: 10_000, server_ts_ms: 1_000 },
        mediaReady: true,
        nowMs: 1_000,
        skewMs: 0,
    });
    check("isHost preserved (host branch)", t6.isHost === true);
}

// ----- P4-T06: jitter threshold widening -----
process.stdout.write("activeThresholds (P4-T06)\n");
{
    const low = activeThresholds(0);
    check(`jitter=0 => indicator ${INDICATOR_THRESHOLD_MS}`, low.indicator === INDICATOR_THRESHOLD_MS);
    check(`jitter=0 => severe ${SEVERE_THRESHOLD_MS}`, low.severe === SEVERE_THRESHOLD_MS);

    const mid = activeThresholds(150);
    check(
        `jitter=150 (< ${JITTER_HIGH_MS}) => indicator stays ${INDICATOR_THRESHOLD_MS}`,
        mid.indicator === INDICATOR_THRESHOLD_MS,
    );
    check(
        `jitter=150 => severe stays ${SEVERE_THRESHOLD_MS}`,
        mid.severe === SEVERE_THRESHOLD_MS,
    );

    const high = activeThresholds(250);
    check(
        `jitter=250 (> ${JITTER_HIGH_MS}) => indicator widens to ${INDICATOR_THRESHOLD_HIGH_MS}`,
        high.indicator === INDICATOR_THRESHOLD_HIGH_MS,
    );
    check(
        `jitter=250 => severe widens to ${SEVERE_THRESHOLD_HIGH_MS}`,
        high.severe === SEVERE_THRESHOLD_HIGH_MS,
    );

    const noJitter = activeThresholds(null);
    check(
        "null jitter => normal indicator",
        noJitter.indicator === INDICATOR_THRESHOLD_MS,
    );

    const boundary = activeThresholds(JITTER_HIGH_MS);
    check(
        `jitter=${JITTER_HIGH_MS} (boundary, strict >) => normal indicator`,
        boundary.indicator === INDICATOR_THRESHOLD_MS,
    );
}

process.stdout.write("deriveDriftSample + jitter (P4-T06)\n");
{
    function stateWithSmoothed(v: number | null) {
        return {
            ...initialDriftState(),
            smoothedDriftMs: v,
            rawDriftMs: v,
            lastSampleAtMs: 1000,
            sampleCount: 1,
        };
    }
    check(
        "1500ms smoothed, low jitter => hidden",
        deriveDriftSample(stateWithSmoothed(1500), 50).indicatorVisible === false,
    );
    check(
        "2500ms smoothed, low jitter => visible",
        deriveDriftSample(stateWithSmoothed(2500), 50).indicatorVisible === true,
    );
    check(
        "2500ms smoothed, high jitter (250) => hidden (widened to 3s)",
        deriveDriftSample(stateWithSmoothed(2500), 250).indicatorVisible === false,
    );
    check(
        "3500ms smoothed, high jitter (250) => visible (widened threshold)",
        deriveDriftSample(stateWithSmoothed(3500), 250).indicatorVisible === true,
    );
    check(
        "5000ms smoothed, low jitter => severe (>= 5s threshold)",
        deriveDriftSample(stateWithSmoothed(5000), 50).severeVisible === true,
    );
    check(
        "5000ms smoothed, high jitter (250) => not severe (widened to 7s)",
        deriveDriftSample(stateWithSmoothed(5000), 250).severeVisible === false,
    );
    check(
        "7500ms smoothed, high jitter (250) => severe (widened 7s)",
        deriveDriftSample(stateWithSmoothed(7500), 250).severeVisible === true,
    );
}

if (failures > 0) {
    process.stdout.write(`\n${failures} failure(s)\n`);
    process.exit(1);
} else {
    process.stdout.write("\nall checks passed\n");
}
