// P5-T02: smoke test for the production drawing send path.
//
// Run via `pnpm -C apps/client smoke:drawing-session`
// (script declared in package.json). Plain Node
// `--experimental-strip-types`, no DOM, no Tauri. It drives
// the REAL `DrawingSession` and `PointerStrokePipeline`
// (the code `DrawingService` / `DrawingLayer` use in the app)
// with an injected fake transport, clock and frame scheduler,
// and proves:
//
//   (a) stroke ids are canonical UUIDs and the local id is
//       the wire id;
//   (b) DRAW_BEGIN -> DRAW_POINT* -> DRAW_END order, one
//       send in flight at a time;
//   (c) <=80 DRAW_POINT sends for a 1000-event, 1-second
//       burst, with last-point-wins semantics;
//   (d) DRAW_END is always sent (and the trailing point
//       flushed first);
//   (e) a rejecting transport never throws out of the
//       pointer handlers;
//   (f) a transport that never settles cannot wedge the
//       queue: the send times out, DRAW_END still goes out,
//       and queued DRAW_POINTs stay bounded;
//   (g) P5-T03: undo / clear go through the same ordered
//       queue (an undo follows its stroke's DRAW_END), never
//       touch the local canvas, and a failing one is
//       reported without wedging the queue.

import { DrawingSession, MAX_QUEUED_POINTS } from "./drawingSession.ts";
import type { DrawingSendFn } from "./drawingSession.ts";
import { PointerStrokePipeline } from "./pointerPipeline.ts";
import type { LocalStrokeSink } from "./pointerPipeline.ts";
import { MAX_DRAW_POINT_HZ } from "./constants.ts";
import { isCanonicalStrokeId, newStrokeId } from "./types.ts";
import type { StrokePoint } from "./types.ts";
import type { DrawingSendInput } from "../bindings/index";

let failures = 0;

function check(name: string, cond: boolean, detail?: string): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}${detail === undefined ? "" : ` (${detail})`}\n`);
        failures++;
    }
}

const unhandled: unknown[] = [];
process.on("unhandledRejection", (reason) => {
    unhandled.push(reason);
});

/** Let the send queue's promise continuations run (no timers),
 *  so `callTimes` reflects the simulated time of each flush. */
async function microtasks(): Promise<void> {
    for (let k = 0; k < 12; k++) await null;
}

/** Let queued promise continuations run. */
async function settle(): Promise<void> {
    for (let i = 0; i < 20; i++) {
        await new Promise<void>((r) => setImmediate(r));
    }
}

interface Rig {
    calls: DrawingSendInput[];
    callTimes: number[];
    maxInFlight: () => number;
    clock: { t: number };
    runFrame: () => void;
    hasFrame: () => boolean;
    session: DrawingSession;
    errors: unknown[];
    local: LocalStrokeSink & {
        events: string[];
        points: StrokePoint[];
        ids: string[];
    };
    pipeline: PointerStrokePipeline;
    canSend: { value: boolean };
}

function makeRig(opts: {
    send?: (input: DrawingSendInput) => Promise<void>;
    sendDelayMs?: number;
    /** Sends for which this returns true never settle. */
    hang?: (input: DrawingSendInput, nth: number) => boolean;
    sendTimeoutMs?: number;
} = {}): Rig {
    const calls: DrawingSendInput[] = [];
    const callTimes: number[] = [];
    const clock = { t: 0 };
    let inFlight = 0;
    let maxInFlight = 0;
    const send: DrawingSendFn = async (input) => {
        calls.push(input);
        if (opts.hang?.(input, calls.length) === true) {
            return new Promise<never>(() => undefined);
        }
        callTimes.push(clock.t);
        inFlight += 1;
        maxInFlight = Math.max(maxInFlight, inFlight);
        try {
            // Yield so overlapping sends would be observable.
            const delay = opts.sendDelayMs ?? 0;
            if (delay > 0) {
                await new Promise<void>((r) => setTimeout(r, delay));
            } else {
                await Promise.resolve();
            }
            if (opts.send !== undefined) await opts.send(input);
        } finally {
            inFlight -= 1;
        }
        return {
            envelope_id: `env-${calls.length}`,
            stroke_id: "stroke_id" in input ? input.stroke_id : null,
        };
    };
    let frameCb: (() => void) | null = null;
    const errors: unknown[] = [];
    const session = new DrawingSession(send, {
        ...(opts.sendTimeoutMs === undefined ? {} : { sendTimeoutMs: opts.sendTimeoutMs }),
        now: () => clock.t,
        requestFrame: (cb) => {
            frameCb = cb;
            return 1;
        },
        cancelFrame: () => {
            frameCb = null;
        },
        onError: (err) => {
            errors.push(err);
        },
    });
    const local = {
        events: [] as string[],
        points: [] as StrokePoint[],
        ids: [] as string[],
        beginStroke(): string {
            const id = newStrokeId();
            this.ids.push(id);
            this.events.push("begin");
            return id;
        },
        appendPoint(p: StrokePoint): void {
            this.points.push(p);
        },
        endStroke(): void {
            this.events.push("end");
        },
    };
    const canSend = { value: true };
    const pipeline = new PointerStrokePipeline(local, session, {
        canSend: () => canSend.value,
        onError: (err) => {
            errors.push(err);
        },
    });
    return {
        calls,
        callTimes,
        maxInFlight: () => maxInFlight,
        clock,
        runFrame: () => {
            const cb = frameCb;
            frameCb = null;
            if (cb !== null) cb();
        },
        hasFrame: () => frameCb !== null,
        session,
        errors,
        local,
        pipeline,
        canSend,
    };
}

const STYLE = { tool: "pen" as const, color: "#ff5c69", width: 3 };

async function main(): Promise<void> {
    process.stdout.write("drawing session smoke\n");

    // ---------------------------------------------------------
    // (a) ids
    // ---------------------------------------------------------
    process.stdout.write("(a) stroke ids\n");
    {
        const ids = new Set<string>();
        let allCanonical = true;
        let allV7 = true;
        for (let i = 0; i < 2000; i++) {
            const id = newStrokeId();
            ids.add(id);
            if (!isCanonicalStrokeId(id)) allCanonical = false;
            if (id[14] !== "7") allV7 = false;
        }
        check("2000 ids are canonical lowercase hyphenated UUIDs", allCanonical);
        check("ids are version 7", allV7);
        check("2000 ids are unique", ids.size === 2000);
        const fixed = newStrokeId(0x0123456789ab);
        check("id embeds the unix-ms timestamp", fixed.startsWith("01234567-89ab-7"), fixed);
        check("old malformed id (3 groups, trailing '-') is rejected", !isCanonicalStrokeId("018f3a2b4c5d-7abc-1234-"));
        check("old prefixed local id is rejected", !isCanonicalStrokeId("stroke-018f3a2b4c5d-7abc-1234"));
        check("uppercase UUID is rejected", !isCanonicalStrokeId(newStrokeId().toUpperCase()));
        check("32-digit (simple) UUID is rejected", !isCanonicalStrokeId(newStrokeId().replace(/-/g, "")));
        check("nil UUID is rejected", !isCanonicalStrokeId("00000000-0000-0000-0000-000000000000"));

        const r = makeRig();
        r.pipeline.down(STYLE, { x: 0.1, y: 0.2, pressure: 0.5, ts: 1 });
        r.pipeline.move({ x: 0.3, y: 0.4, pressure: 0.5, ts: 2 });
        r.pipeline.up({ x: 0.5, y: 0.6, pressure: 0.5, ts: 3 });
        await settle();
        const localId = r.local.ids[0];
        check("local store id is a canonical UUID", localId !== undefined && isCanonicalStrokeId(localId));
        check(
            "every wire envelope carries the local stroke id",
            r.calls.length > 0 &&
                r.calls.every((c) => "stroke_id" in c && c.stroke_id === localId),
        );
    }

    // ---------------------------------------------------------
    // (b) order
    // ---------------------------------------------------------
    process.stdout.write("(b) begin -> point -> end order\n");
    {
        // A slow transport (5 ms per send) must still be
        // observed strictly in order, one at a time.
        const r = makeRig({ sendDelayMs: 5 });
        r.clock.t = 100;
        r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0.5, ts: 100 });
        // Move and frame immediately, while BEGIN is still in flight.
        r.clock.t = 120;
        r.pipeline.move({ x: 0.2, y: 0.2, pressure: 0.5, ts: 120 });
        r.runFrame();
        r.clock.t = 140;
        r.pipeline.move({ x: 0.3, y: 0.3, pressure: 0.5, ts: 140 });
        r.runFrame();
        r.pipeline.up({ x: 0.4, y: 0.4, pressure: 0.5, ts: 150 });
        await r.session.idle();
        const kinds = r.calls.map((c) => c.action);
        check("first send is begin", kinds[0] === "begin");
        check("last send is end", kinds[kinds.length - 1] === "end");
        check(
            "begin, point, point, point, end",
            kinds.join(",") === "begin,point,point,point,end",
            kinds.join(","),
        );
        check("only one drawing_send in flight at a time", r.maxInFlight() === 1, String(r.maxInFlight()));
        const seqs = r.calls.map((c) => ("client_seq" in c ? c.client_seq : 0));
        check(
            "client_seq is strictly increasing starting at 1",
            seqs[0] === 1 && seqs.every((s, i) => i === 0 || s > (seqs[i - 1] ?? 0)),
            seqs.join(","),
        );
        const begin = r.calls[0];
        check(
            "begin carries style and first point",
            begin !== undefined &&
                begin.action === "begin" &&
                begin.tool === "pen" &&
                begin.color === "#ff5c69" &&
                begin.width === 3 &&
                begin.x === 0.1 &&
                begin.y === 0.1,
        );
        check("local stroke saw begin then end", r.local.events.join(",") === "begin,end");
    }
    {
        // Starting a second stroke while one is open closes the
        // first (END precedes the next BEGIN).
        const r = makeRig();
        r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1, pointerId: 1 });
        r.pipeline.down(STYLE, { x: 0.5, y: 0.5, pressure: 0, ts: 2, pointerId: 2 });
        r.pipeline.up({ x: 0.5, y: 0.5, pressure: 0, ts: 3, pointerId: 2 });
        await settle();
        check(
            "second down ends the first stroke before beginning the next",
            // The final `up` position is flushed as a point first.
            r.calls.map((c) => c.action).join(",") === "begin,end,begin,point,end",
            r.calls.map((c) => c.action).join(","),
        );
    }
    {
        // Events from another pointer are ignored mid-stroke.
        const r = makeRig();
        r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1, pointerId: 1 });
        r.pipeline.move({ x: 0.9, y: 0.9, pressure: 0, ts: 2, pointerId: 2 });
        r.pipeline.up({ x: 0.9, y: 0.9, pressure: 0, ts: 3, pointerId: 2 });
        check("other pointer's up does not end the stroke", r.pipeline.active);
        r.pipeline.up({ x: 0.2, y: 0.2, pressure: 0, ts: 4, pointerId: 1 });
        await settle();
        check("owning pointer's up ends it", !r.pipeline.active);
        check(
            "foreign pointer coordinates never reached the wire",
            r.calls.every((c) => !("x" in c) || c.x < 0.5),
        );
    }

    // ---------------------------------------------------------
    // (c) rate cap and last-point-wins
    // ---------------------------------------------------------
    process.stdout.write("(c) <=80 DRAW_POINT for a 1000-event 1 s burst\n");
    for (const frameEveryMs of [1, 4, 8, 16]) {
        const r = makeRig();
        r.clock.t = 0;
        r.pipeline.down(STYLE, { x: 0, y: 0, pressure: 0.5, ts: 0 });
        // 1000 pointer moves, 1 per simulated ms; the frame
        // scheduler fires every `frameEveryMs`.
        for (let i = 1; i <= 1000; i++) {
            r.clock.t = i;
            r.pipeline.move({ x: i / 1000, y: i / 2000, pressure: 0.5, ts: i });
            if (i % frameEveryMs === 0) r.runFrame();
            await microtasks();
        }
        r.clock.t = 1001;
        r.pipeline.up({ x: 1, y: 0.5, pressure: 0.5, ts: 1001 });
        await r.session.idle();
        const points = r.calls.filter((c) => c.action === "point");
        check(
            `frames every ${frameEveryMs} ms: ${points.length} DRAW_POINT <= ${MAX_DRAW_POINT_HZ}`,
            points.length <= MAX_DRAW_POINT_HZ,
            String(points.length),
        );
        check(`frames every ${frameEveryMs} ms: at least 50 points sent (not starved)`, points.length >= 50);
        // No 1 s sliding window may contain more than 80 DRAW_POINTs.
        const times: number[] = [];
        r.calls.forEach((c, i) => {
            if (c.action === "point") times.push(r.callTimes[i] ?? 0);
        });
        let worst = 0;
        for (let i = 0; i < times.length; i++) {
            let n = 0;
            for (let j = i; j < times.length && (times[j] ?? 0) - (times[i] ?? 0) < 1000; j++) n++;
            worst = Math.max(worst, n);
        }
        check(`frames every ${frameEveryMs} ms: worst 1 s window ${worst} <= ${MAX_DRAW_POINT_HZ}`, worst <= MAX_DRAW_POINT_HZ);
        // Last-point-wins: sent x values never go backwards (no
        // stale point is ever sent after a newer one) and the
        // final point on the wire is the final pointer position.
        let monotonic = true;
        let prev = -1;
        for (const c of points) {
            if (c.action !== "point") continue;
            if (c.x < prev) monotonic = false;
            prev = c.x;
        }
        check(`frames every ${frameEveryMs} ms: sent points are never older than the previous one`, monotonic);
        const lastPoint = points[points.length - 1];
        check(
            `frames every ${frameEveryMs} ms: trailing point is the pointer-up position`,
            lastPoint !== undefined && lastPoint.action === "point" && lastPoint.x === 1 && lastPoint.y === 0.5,
        );
        check(
            `frames every ${frameEveryMs} ms: local canvas still got all 1002 points`,
            r.local.points.length === 1002,
            String(r.local.points.length),
        );
    }
    {
        // Coalescing is not "drop everything between sends": a
        // flush sends the NEWEST pending point, not the oldest.
        const r = makeRig();
        r.pipeline.down(STYLE, { x: 0, y: 0, pressure: 0, ts: 0 });
        r.clock.t = 50;
        r.pipeline.move({ x: 0.1, y: 0.1, pressure: 0, ts: 50 });
        r.pipeline.move({ x: 0.2, y: 0.2, pressure: 0, ts: 51 });
        r.pipeline.move({ x: 0.3, y: 0.3, pressure: 0, ts: 52 });
        r.runFrame();
        await r.session.idle();
        const pts = r.calls.filter((c) => c.action === "point");
        check(
            "three moves before a frame -> one DRAW_POINT carrying the newest",
            pts.length === 1 && pts[0]?.action === "point" && pts[0].x === 0.3,
        );
    }

    // ---------------------------------------------------------
    // (d) DRAW_END is always sent
    // ---------------------------------------------------------
    process.stdout.write("(d) DRAW_END is always sent\n");
    {
        // The trailing point inside the rate window is flushed
        // (bypassing the gate) and END follows it.
        const r = makeRig();
        r.pipeline.down(STYLE, { x: 0, y: 0, pressure: 0, ts: 0 });
        r.clock.t = 100;
        r.pipeline.move({ x: 0.1, y: 0.1, pressure: 0, ts: 100 });
        r.runFrame(); // sends 0.1
        r.clock.t = 101; // 1 ms later: inside the 12.5 ms gate
        r.pipeline.move({ x: 0.2, y: 0.2, pressure: 0, ts: 101 });
        r.runFrame(); // gated, stays pending
        r.pipeline.up({ x: 0.3, y: 0.3, pressure: 0, ts: 102 });
        await r.session.idle();
        check(
            "point-gated-by-rate then up: begin,point,point,end",
            r.calls.map((c) => c.action).join(",") === "begin,point,point,end",
            r.calls.map((c) => c.action).join(","),
        );
        const last = r.calls[r.calls.length - 2];
        check("the forced trailing point is the newest position", last?.action === "point" && last.x === 0.3);
    }
    {
        // pointercancel / leave ends the stroke where it is.
        const r = makeRig();
        r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1, pointerId: 7 });
        r.pipeline.cancel({ x: 0, y: 0, pressure: 0, ts: 0, pointerId: 7 });
        await settle();
        check("cancel sends begin,end", r.calls.map((c) => c.action).join(",") === "begin,end");
        check("no frame left scheduled after end", !r.hasFrame());
    }
    {
        // Unmount / room change / leaving drawing mode.
        const r = makeRig();
        r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1 });
        r.clock.t = 20;
        r.pipeline.move({ x: 0.2, y: 0.2, pressure: 0, ts: 20 });
        r.pipeline.finish();
        await r.session.idle();
        check(
            "finish() mid-stroke flushes the last point and sends end",
            r.calls.map((c) => c.action).join(",") === "begin,point,end",
            r.calls.map((c) => c.action).join(","),
        );
        check("a late frame after end sends nothing", (r.runFrame(), r.calls.length === 3));
    }
    {
        // dispose() on the service closes a stroke begun without
        // the pipeline.
        const r = makeRig();
        void r.session.beginStroke({
            tool: "pen",
            color: "#000",
            width: 1,
            x: 0.5,
            y: 0.5,
            pressure: 0,
            tsMs: 1,
        });
        await r.session.dispose();
        check("dispose() ends an open stroke", r.calls.map((c) => c.action).join(",") === "begin,end");
    }
    {
        // Capability revoked mid-stroke: local stroke closes, but
        // no further envelope (the server would answer ROOM_ERROR).
        const r = makeRig();
        r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1 });
        await r.session.idle();
        const before = r.calls.length;
        r.clock.t = 50;
        r.pipeline.move({ x: 0.2, y: 0.2, pressure: 0, ts: 50 });
        r.pipeline.finish({ sendEnd: false });
        r.runFrame();
        await settle();
        check("revoked mid-stroke: no point/end after revocation", r.calls.length === before);
        check("revoked mid-stroke: local stroke was ended", r.local.events.join(",") === "begin,end");
    }
    {
        // No DRAW capability at pointer down: local-only stroke,
        // nothing on the wire.
        const r = makeRig();
        r.canSend.value = false;
        r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1 });
        r.clock.t = 30;
        r.pipeline.move({ x: 0.2, y: 0.2, pressure: 0, ts: 30 });
        r.runFrame();
        r.pipeline.up({ x: 0.3, y: 0.3, pressure: 0, ts: 31 });
        await settle();
        check("no capability: zero drawing_send calls", r.calls.length === 0);
        check("no capability: local stroke still drawn", r.local.points.length === 3 && r.local.events.join(",") === "begin,end");
    }
    {
        // Non-finite / out-of-range input never reaches the wire.
        const r = makeRig();
        r.pipeline.down(STYLE, { x: Number.NaN, y: 0.1, pressure: 0, ts: 1 });
        check("NaN pointer down is ignored", !r.pipeline.active && r.calls.length === 0);
        r.pipeline.down(STYLE, { x: 2, y: -1, pressure: 5, ts: 1 });
        await settle();
        const b = r.calls[0];
        check(
            "out-of-range coordinates are clamped to [0, 1]",
            b?.action === "begin" && b.x === 1 && b.y === 0 && b.pressure === 1,
        );
    }

    // ---------------------------------------------------------
    // (e) failures never throw out of the pointer handlers
    // ---------------------------------------------------------
    process.stdout.write("(e) rejected sends do not throw\n");
    {
        const r = makeRig({
            send: async () => {
                throw new Error("ipc down");
            },
        });
        let threw = false;
        try {
            r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1 });
            for (let i = 1; i <= 50; i++) {
                r.clock.t = i * 10;
                r.pipeline.move({ x: 0.1 + i / 100, y: 0.2, pressure: 0, ts: i * 10 });
                r.runFrame();
            }
            r.pipeline.up({ x: 0.9, y: 0.9, pressure: 0, ts: 600 });
            r.pipeline.cancel();
            r.pipeline.finish();
        } catch {
            threw = true;
        }
        await settle();
        check("handlers did not throw with a rejecting transport", !threw);
        check("no unhandled promise rejection escaped", unhandled.length === 0, String(unhandled.length));
        check("failures were reported through onError", r.errors.length > 0);
        // BEGIN failed -> the rest of the stroke is dropped, not
        // streamed as POINT/END the server would reject.
        check(
            "after a failed begin only the begin attempt was made",
            r.calls.length === 1 && r.calls[0]?.action === "begin",
            r.calls.map((c) => c.action).join(","),
        );
        check("local stroke is intact despite the failure", r.local.points.length === 52 && r.local.events.join(",") === "begin,end");
    }
    {
        // Only POINT sends fail: the stroke still ends.
        const r = makeRig({
            send: async (input) => {
                if (input.action === "point") throw new Error("flaky");
            },
        });
        let threw = false;
        try {
            r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1 });
            r.clock.t = 20;
            r.pipeline.move({ x: 0.2, y: 0.2, pressure: 0, ts: 20 });
            r.runFrame();
            r.pipeline.up({ x: 0.3, y: 0.3, pressure: 0, ts: 40 });
        } catch {
            threw = true;
        }
        await settle();
        check("failing point sends do not throw", !threw && unhandled.length === 0);
        check(
            "DRAW_END is still sent after point failures",
            r.calls[r.calls.length - 1]?.action === "end",
            r.calls.map((c) => c.action).join(","),
        );
    }
    {
        // A transport that throws synchronously (not a rejected
        // promise) is also contained.
        const calls: string[] = [];
        const session = new DrawingSession(
            (() => {
                throw new Error("sync boom");
            }) as unknown as DrawingSendFn,
            { onError: (_e, i) => calls.push(i.action), requestFrame: () => 1, cancelFrame: () => undefined },
        );
        const local = {
            beginStroke: () => newStrokeId(),
            appendPoint: () => undefined,
            endStroke: () => undefined,
        };
        const pipeline = new PointerStrokePipeline(local, session, { canSend: () => true });
        let threw = false;
        try {
            pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1 });
            pipeline.up({ x: 0.2, y: 0.2, pressure: 0, ts: 2 });
        } catch {
            threw = true;
        }
        await settle();
        check("synchronously throwing transport does not throw out of handlers", !threw && unhandled.length === 0);
    }

    // ---------------------------------------------------------
    // (f) a transport that never settles
    // ---------------------------------------------------------
    process.stdout.write("(f) hung transport\n");
    const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));
    {
        // The first DRAW_POINT hangs forever. A 1000-event burst
        // piles up behind it; the queue must stay bounded, and
        // after the timeout the stroke must still end.
        const r = makeRig({
            sendTimeoutMs: 150,
            hang: (input, nth) => input.action === "point" && nth === 2,
        });
        let threw = false;
        let maxQueued = 0;
        try {
            r.pipeline.down(STYLE, { x: 0, y: 0, pressure: 0.5, ts: 0 });
            for (let i = 1; i <= 1000; i++) {
                r.clock.t = i * 10; // always past the rate gate
                r.pipeline.move({ x: i / 1000, y: 0.5, pressure: 0.5, ts: i });
                r.runFrame();
                maxQueued = Math.max(maxQueued, r.session.queuedSendCount);
                if (i % 50 === 0) await microtasks();
            }
            r.clock.t = 20_000;
            r.pipeline.up({ x: 1, y: 0.5, pressure: 0.5, ts: 1001 });
            maxQueued = Math.max(maxQueued, r.session.queuedSendCount);
        } catch {
            threw = true;
        }
        check("burst against a hung transport does not throw", !threw);
        check(
            `queued sends stay bounded (max ${maxQueued} <= ${MAX_QUEUED_POINTS + 2})`,
            maxQueued <= MAX_QUEUED_POINTS + 2,
            String(maxQueued),
        );
        await sleep(500);
        await r.session.idle();
        const last = r.calls[r.calls.length - 1];
        check("DRAW_END is sent after the hung send times out", last?.action === "end", r.calls.map((c) => c.action).slice(-3).join(","));
        check("the timeout was reported through onError", r.errors.some((e) => String(e).includes("timed out")));
        check(
            "oldest points were dropped, not the newest (last point before end is the final position)",
            (() => {
                const pts = r.calls.filter((c) => c.action === "point");
                const lp = pts[pts.length - 1];
                return lp !== undefined && lp.action === "point" && lp.x === 1;
            })(),
        );
        check(
            "total sends are bounded far below 1000",
            r.calls.length <= MAX_QUEUED_POINTS + 6,
            String(r.calls.length),
        );
        check("no unhandled rejection after the timeout", unhandled.length === 0, String(unhandled.length));
        check("queue is empty afterwards", r.session.queuedSendCount === 0);
    }
    {
        // DRAW_BEGIN hangs: the stroke is dropped (existing
        // failed-BEGIN behaviour) but the queue is not wedged -
        // the next stroke goes out.
        const r = makeRig({
            sendTimeoutMs: 100,
            hang: (input, nth) => input.action === "begin" && nth === 1,
        });
        r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0, ts: 1 });
        r.clock.t = 50;
        r.pipeline.move({ x: 0.2, y: 0.2, pressure: 0, ts: 50 });
        r.runFrame();
        r.pipeline.up({ x: 0.3, y: 0.3, pressure: 0, ts: 60 });
        await sleep(300);
        check(
            "hung BEGIN: the rest of that stroke is dropped",
            r.calls.length === 1 && r.calls[0]?.action === "begin",
            r.calls.map((c) => c.action).join(","),
        );
        r.pipeline.down(STYLE, { x: 0.5, y: 0.5, pressure: 0, ts: 100 });
        r.pipeline.up({ x: 0.6, y: 0.6, pressure: 0, ts: 110 });
        await r.session.idle();
        check(
            "hung BEGIN: a later stroke still goes out (queue not wedged)",
            r.calls.map((c) => c.action).slice(1).join(",") === "begin,point,end",
            r.calls.map((c) => c.action).join(","),
        );
        check("hung BEGIN: no unhandled rejection", unhandled.length === 0);
    }

    // ---------------------------------------------------------
    // (g) undo + clear (P5-T03)
    // ---------------------------------------------------------
    process.stdout.write("(g) undo / clear through the ordered queue\n");
    {
        // A slow transport: the undo is requested while the stroke's
        // own sends are still in flight. It must go out after END.
        const r = makeRig({ sendDelayMs: 5 });
        r.pipeline.down(STYLE, { x: 0.1, y: 0.1, pressure: 0.5, ts: 1 });
        r.pipeline.up({ x: 0.2, y: 0.2, pressure: 0.5, ts: 2 });
        const id = r.local.ids[0];
        check("a stroke was drawn", id !== undefined);
        const localEventsBefore = r.local.events.slice();
        const undone = r.session.undoStroke(id ?? "");
        const cleared = r.session.clearAll();
        await Promise.all([undone, cleared]);
        const kinds = r.calls.map((c) => c.action);
        check(
            "begin, point, end, undo, clear in order",
            kinds.join(",") === "begin,point,end,undo,clear" ||
                kinds.join(",") === "begin,end,undo,clear",
            kinds.join(","),
        );
        const undo = r.calls.find((c) => c.action === "undo");
        check(
            "undo carries the local stroke id and nothing else",
            undo !== undefined &&
                undo.action === "undo" &&
                undo.stroke_id === id &&
                Object.keys(undo).sort().join(",") === "action,stroke_id",
        );
        const clr = r.calls.find((c) => c.action === "clear");
        check(
            "clear carries no fields",
            clr !== undefined && Object.keys(clr).join(",") === "action",
        );
        check("still one send in flight at a time", r.maxInFlight() === 1, String(r.maxInFlight()));
        check(
            "undo and clear do not touch the local canvas",
            r.local.events.join(",") === localEventsBefore.join(","),
            r.local.events.join(","),
        );
        check("no error was reported", r.errors.length === 0);
    }
    {
        // A failing undo rejects to its caller, is reported through
        // onError, and does not wedge the queue.
        let failNext = true;
        const r = makeRig({
            send: async (input) => {
                if (input.action === "undo" && failNext) {
                    failNext = false;
                    throw new Error("boom");
                }
            },
        });
        let rejected = false;
        await r.session.undoStroke(newStrokeId()).catch(() => {
            rejected = true;
        });
        check("a failed undo rejects to the caller", rejected);
        check("a failed undo is reported through onError", r.errors.length === 1);
        await r.session.clearAll();
        check(
            "a later clear still goes out",
            r.calls.map((c) => c.action).join(",") === "undo,clear",
            r.calls.map((c) => c.action).join(","),
        );
        check("no unhandled rejection from undo / clear", unhandled.length === 0);
    }
    {
        // An undo for a stroke the session never drew is just a send:
        // the SERVER decides (and answers a no-op silently).
        const r = makeRig();
        const foreign = newStrokeId();
        await r.session.undoStroke(foreign);
        const first = r.calls[0];
        check(
            "an undo of any canonical id is sent as-is",
            first !== undefined && first.action === "undo" && first.stroke_id === foreign,
        );
    }

    if (failures > 0) {
        process.stdout.write(`\n${failures} failure(s)\n`);
        process.exit(1);
    } else {
        process.stdout.write("\nall checks passed\n");
    }
}

main().catch((err: unknown) => {
    process.stdout.write(`FATAL ${String(err)}\n`);
    process.exit(1);
});
