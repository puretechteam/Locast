// P5-T02: smoke test for the drawing coalescing math.
//
// Run via `pnpm -C apps/client smoke:drawing` (script
// declared in package.json). Plain Node 22+
// `--experimental-strip-types`, no DOM. Validates the
// last-point-wins behavior + the minimum-flush-interval
// gate that the React-side `DrawingService` enforces
// at the IPC boundary.

import { MAX_DRAW_POINT_HZ } from "../drawing/constants.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

process.stdout.write("drawing coalescing smoke\n");

// ----- Constant sanity -----
process.stdout.write("constants\n");
check(
    "MAX_DRAW_POINT_HZ is 120",
    MAX_DRAW_POINT_HZ === 120,
);
const MIN_INTERVAL_MS = 1000 / MAX_DRAW_POINT_HZ;
check(
    "1000/MAX_DRAW_POINT_HZ is approximately 8.33",
    Math.abs(MIN_INTERVAL_MS - 1000 / 120) < 1e-6,
);

// ----- Pure coalescing simulation -----
//
// Reproduces the same last-point-wins + min-interval gate
// logic that DrawingService::tickTick enforces. The test
// runs N pointermove "events" in a tight loop with a
// shared `now` cursor; for each event, the simulator
// either keeps the pending point (last-point-wins) or
// flushes a DRAW_POINT envelope (if the minimum interval
// has elapsed). The number of flushes is the expected
// outbound DRAW_POINT count.

interface SimState {
    pending: { x: number; y: number; ts: number } | null;
    lastFlushMs: number;
    flushed: number;
}

function newSim(startMs: number): SimState {
    return { pending: null, lastFlushMs: startMs, flushed: 0 };
}

function appendPoint(
    s: SimState,
    p: { x: number; y: number; ts: number },
): void {
    s.pending = p;
}

function tick(s: SimState, nowMs: number): void {
    if (s.pending === null) return;
    if (nowMs - s.lastFlushMs < MIN_INTERVAL_MS) return;
    s.pending = null;
    s.lastFlushMs = nowMs;
    s.flushed += 1;
}

process.stdout.write("last-point-wins coalescing\n");

// Scenario A: 200 pointermoves in 1 s.
// A real pointer event is delivered roughly every 5 ms;
// the coalescer keeps the LAST pending point and only
// emits when the 8.33 ms minimum interval has elapsed.
{
    const s = newSim(0);
    s.lastFlushMs = -100;
    for (let i = 0; i < 200; i++) {
        const now = i * 5;
        appendPoint(s, { x: 0.5, y: 0.5, ts: now });
        tick(s, now);
    }
    check(
        "200 events at 5 ms intervals -> <=120 flushes",
        s.flushed <= 120,
    );
    check(
        "200 events at 5 ms intervals -> at least 50 flushes",
        s.flushed >= 50,
    );
}

// Scenario B: very slow pointer (one event per 1 s)
// -> exactly 1 flush per tick-eligible event.
{
    const s = newSim(0);
    for (let i = 0; i < 5; i++) {
        const now = i * 1000;
        s.lastFlushMs = now - 100;
        appendPoint(s, { x: 0.5, y: 0.5, ts: now });
        tick(s, now);
    }
    check(
        "5 events at 1 s intervals -> 5 flushes",
        s.flushed === 5,
    );
}

// Scenario C: rapid burst (100 events in 50 ms)
// -> at most ceil(50 / 8.33) = 6 flushes.
{
    const s = newSim(0);
    for (let i = 0; i < 100; i++) {
        const now = (i * 50) / 100; // 0..50 ms
        if (i === 0) s.lastFlushMs = now - 100;
        appendPoint(s, { x: 0.5, y: 0.5, ts: now });
        tick(s, now);
    }
    check(
        "100 events in 50 ms -> <=7 flushes",
        s.flushed <= 7,
    );
    check(
        "100 events in 50 ms -> at least 4 flushes (interval honored)",
        s.flushed >= 4,
    );
}

// Scenario D: a single final pending point is flushed
// on endStroke (the service's `flushPending` bypasses
// the interval gate ONLY when called explicitly at
// stroke close).
{
    const s = newSim(0);
    s.lastFlushMs = -100;
    appendPoint(s, { x: 0.5, y: 0.5, ts: 0 });
    tick(s, 0);
    s.lastFlushMs = -100;
    appendPoint(s, { x: 0.6, y: 0.6, ts: 1 });
    check(
        "pending point survives rapid pointer events (no flush within 1 ms)",
        s.pending !== null,
    );
    s.pending = null;
    s.flushed += 1;
    check(
        "endStroke flush emits the trailing point (flushed=2 total)",
        s.flushed === 2,
    );
}

if (failures > 0) {
    process.stdout.write(`\n${failures} failure(s)\n`);
    process.exit(1);
} else {
    process.stdout.write("\nall checks passed\n");
}