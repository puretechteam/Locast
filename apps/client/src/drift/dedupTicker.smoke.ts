// Unit test for the dedup ticker and its effect on a parked playback event.

import { evaluateDedup, initialDedupState, tickDedup, BUFFER_TIMEOUT_MS } from "./dedup.ts";
import { startDedupTicker, type TickerTimers } from "./dedupTicker.ts";

let failures = 0;
function check(name: string, cond: boolean): void {
    process.stdout.write(`  ${cond ? "ok" : "FAIL"} ${name}\n`);
    if (!cond) failures++;
}

interface Ev {
    sender_id: string;
    monotonic_seq: number;
}

process.stdout.write("startDedupTicker\n");
{
    // A late joiner's first event from the host is seq 4: it parks.
    let state = initialDedupState<Ev>();
    let clock = 10_000;
    const parked = evaluateDedup(state, { sender_id: "host", monotonic_seq: 4 }, clock);
    state = parked.next;
    check("late joiner's first event parks", parked.decision.kind === "buffer");

    let fn: (() => void) | null = null;
    let cleared = false;
    const timers: TickerTimers = {
        setInterval: (f) => {
            fn = f;
            return 1;
        },
        clearInterval: () => {
            cleared = true;
        },
        now: () => clock,
    };
    const applied: number[] = [];
    const stop = startDedupTicker(
        (now) => {
            const r = tickDedup(state, now);
            state = r.next;
            for (const e of r.expired) applied.push(e.seq);
        },
        1000,
        timers,
    );
    check("a timer was registered", fn !== null);

    clock += BUFFER_TIMEOUT_MS - 1;
    fn!();
    check("not applied before the grace window", applied.length === 0);
    clock += 1;
    fn!();
    check("applied once the grace window has passed", applied.length === 1 && applied[0] === 4);
    stop();
    check("stop clears the timer", cleared);
}

if (failures > 0) {
    process.stdout.write(`\n${failures} failure(s)\n`);
    process.exit(1);
}
process.stdout.write("\nall checks passed\n");
