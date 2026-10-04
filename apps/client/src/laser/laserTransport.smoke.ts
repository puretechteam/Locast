// P5-T04: smoke test for the laser transport.
//
// Run via `pnpm -C apps/client smoke:laser` (script declared in
// package.json). Plain Node `--experimental-strip-types`, no DOM,
// no Tauri. It drives the REAL `LaserSender` and
// `acceptRemoteLaser` (the code `useLaserTransport` uses in the
// app) with an injected fake transport, clock and timers, and
// proves:
//
//   (a) a 1000 Hz pointer for one second produces <= 60
//       LASER_MOVE sends, never two closer than 1000 / 60 ms,
//       and the last position always goes out (last-point-wins);
//   (b) one send in flight at a time; a slow transport coalesces
//       instead of queueing;
//   (c) a still laser re-sends its position about every 1000 ms
//       (keepalive), and stops after `off`;
//   (d) `off` drops the pending position and sends exactly one
//       LASER_OFF, only when a move went out since the last off;
//   (e) a rejecting / throwing / hanging transport never throws
//       out of `move` / `off` and never wedges the sender;
//   (f) `dispose` stops everything;
//   (g) `acceptRemoteLaser` rejects wrong-room, own-user, empty
//       or nil sender, and non-finite / out-of-range coordinates.

import {
    LaserSender,
    LASER_KEEPALIVE_MS,
    MAX_LASER_HZ,
    MIN_LASER_SEND_INTERVAL_MS,
    REMOTE_LASER_IDLE_MS,
    acceptRemoteLaser,
} from "./laserTransport.ts";
import type { LaserSendFn } from "./laserTransport.ts";
import type { LaserSendInput } from "../bindings/index";

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

/** Let queued promise continuations run. */
async function microtasks(): Promise<void> {
    for (let k = 0; k < 12; k++) await null;
}

/** A fake clock with a timer queue. */
class FakeTime {
    t = 0;
    private seq = 0;
    private timers = new Map<number, { at: number; cb: () => void }>();

    setTimer = (cb: () => void, ms: number): unknown => {
        const id = ++this.seq;
        this.timers.set(id, { at: this.t + Math.max(0, ms), cb });
        return id;
    };

    clearTimer = (h: unknown): void => {
        this.timers.delete(h as number);
    };

    pending(): number {
        return this.timers.size;
    }

    /** Advance to `to`, firing due timers in order. */
    async advanceTo(to: number): Promise<void> {
        for (;;) {
            // Settle sends that already finished before picking the
            // next timer (as a real event loop would).
            await microtasks();
            let next: [number, { at: number; cb: () => void }] | null = null;
            for (const e of this.timers) {
                if (e[1].at <= to && (next === null || e[1].at < next[1].at)) next = e;
            }
            if (next === null) break;
            this.timers.delete(next[0]);
            this.t = Math.max(this.t, next[1].at);
            next[1].cb();
            await microtasks();
        }
        this.t = to;
        await microtasks();
    }
}

interface Rig {
    time: FakeTime;
    calls: LaserSendInput[];
    callTimes: number[];
    maxInFlight: () => number;
    sender: LaserSender;
    errors: unknown[];
    /** Resolve every send that is waiting (for `manual` rigs). */
    release: () => void;
}

function makeRig(opts: {
    manual?: boolean;
    behave?: (input: LaserSendInput, nth: number) => "ok" | "reject" | "throw" | "hang";
    keepaliveMs?: number;
    sendTimeoutMs?: number;
} = {}): Rig {
    const time = new FakeTime();
    const calls: LaserSendInput[] = [];
    const callTimes: number[] = [];
    let inFlight = 0;
    let maxInFlight = 0;
    const waiting: Array<() => void> = [];
    const send: LaserSendFn = (input) => {
        calls.push(input);
        callTimes.push(time.t);
        const how = opts.behave?.(input, calls.length) ?? "ok";
        if (how === "throw") throw new Error("ipc exploded");
        if (how === "hang") return new Promise<never>(() => undefined);
        inFlight += 1;
        maxInFlight = Math.max(maxInFlight, inFlight);
        return new Promise<unknown>((resolve, reject) => {
            const finish = (): void => {
                inFlight -= 1;
                if (how === "reject") reject(new Error("not in a room"));
                else resolve({ envelope_id: `env-${calls.length}` });
            };
            if (opts.manual === true) waiting.push(finish);
            else queueMicrotask(finish);
        });
    };
    const errors: unknown[] = [];
    const sender = new LaserSender(send, {
        now: () => time.t,
        setTimer: time.setTimer,
        clearTimer: time.clearTimer,
        ...(opts.keepaliveMs === undefined ? {} : { keepaliveMs: opts.keepaliveMs }),
        ...(opts.sendTimeoutMs === undefined ? {} : { sendTimeoutMs: opts.sendTimeoutMs }),
        onError: (err) => {
            errors.push(err);
        },
    });
    return {
        time,
        calls,
        callTimes,
        maxInFlight: () => maxInFlight,
        sender,
        errors,
        release: () => {
            while (waiting.length > 0) waiting.shift()!();
        },
    };
}

const moves = (r: Rig): Array<{ x: number; y: number }> =>
    r.calls.flatMap((c) => (c.action === "move" ? [{ x: c.x, y: c.y }] : []));
const offs = (r: Rig): number => r.calls.filter((c) => c.action === "off").length;

async function testRate(): Promise<void> {
    process.stdout.write("(a) <= 60 Hz, last point wins\n");
    // Keepalive off so only pointer-driven sends are counted.
    const r = makeRig({ keepaliveMs: 0 });
    for (let ms = 0; ms < 1000; ms++) {
        await r.time.advanceTo(ms);
        r.sender.move((ms % 1000) / 1000, 0.5);
    }
    await r.time.advanceTo(1100);
    const m = moves(r);
    check(`<= ${MAX_LASER_HZ} sends for a 1000 Hz second`, m.length <= MAX_LASER_HZ + 1, `${m.length}`);
    check("a meaningful number of sends", m.length >= 50, `${m.length}`);
    let minGap = Infinity;
    for (let i = 1; i < r.callTimes.length; i++) {
        minGap = Math.min(minGap, r.callTimes[i]! - r.callTimes[i - 1]!);
    }
    check(
        `no two sends closer than ${MIN_LASER_SEND_INTERVAL_MS.toFixed(2)} ms`,
        minGap >= MIN_LASER_SEND_INTERVAL_MS - 1e-9,
        `${minGap}`,
    );
    check("the last position went out", m.at(-1)?.x === 0.999, JSON.stringify(m.at(-1)));
    check("never two in flight", r.maxInFlight() === 1, `${r.maxInFlight()}`);
    check("no off without an off()", offs(r) === 0);
    r.sender.dispose();
}

async function testCoalesce(): Promise<void> {
    process.stdout.write("(b) slow transport coalesces\n");
    const r = makeRig({ manual: true, keepaliveMs: 0 });
    r.sender.move(0.1, 0.1);
    await microtasks();
    check("first move sent immediately", r.calls.length === 1);
    for (let i = 0; i < 100; i++) {
        await r.time.advanceTo(r.time.t + 5);
        r.sender.move(0.2 + i / 1000, 0.2);
    }
    check("nothing else sent while one is in flight", r.calls.length === 1, `${r.calls.length}`);
    r.release();
    await microtasks();
    await r.time.advanceTo(r.time.t + 20);
    const m = moves(r);
    check("after release exactly one more (the newest)", m.length === 2, `${m.length}`);
    check("the newest position won", Math.abs(m[1]!.x - 0.299) < 1e-9, JSON.stringify(m[1]));
    check("never two in flight", r.maxInFlight() === 1);
    r.release();
    r.sender.dispose();
}

async function testKeepalive(): Promise<void> {
    process.stdout.write("(c) keepalive while still\n");
    const r = makeRig();
    r.sender.move(0.4, 0.6);
    await r.time.advanceTo(5_500);
    const m = moves(r);
    check("initial + ~5 keepalives in 5.5 s", m.length >= 5 && m.length <= 7, `${m.length}`);
    check("keepalives repeat the last position", m.every((p) => p.x === 0.4 && p.y === 0.6));
    for (let i = 1; i < r.callTimes.length; i++) {
        const gap = r.callTimes[i]! - r.callTimes[i - 1]!;
        if (Math.abs(gap - LASER_KEEPALIVE_MS) > 1) {
            check("keepalive interval", false, `gap ${gap}`);
            break;
        }
    }
    check("keepalive interval is ~1000 ms", true);
    // Moving resets the keepalive clock to the last real send.
    await r.time.advanceTo(5_700);
    r.sender.move(0.5, 0.5);
    await r.time.advanceTo(5_701);
    const before = r.calls.length;
    await r.time.advanceTo(6_600);
    check("no keepalive sooner than 1000 ms after a move", r.calls.length === before);
    await r.time.advanceTo(6_750);
    check("keepalive after 1000 ms", r.calls.length === before + 1);
    r.sender.off();
    await microtasks();
    const afterOff = r.calls.length;
    await r.time.advanceTo(12_000);
    check("off stops the keepalive", r.calls.length === afterOff, `${r.calls.length - afterOff}`);
    check("keepalive did not leak timers", r.time.pending() === 0, `${r.time.pending()}`);
    check(
        "remote idle expiry is more than twice the keepalive",
        REMOTE_LASER_IDLE_MS > 2 * LASER_KEEPALIVE_MS,
    );
}

async function testOff(): Promise<void> {
    process.stdout.write("(d) off semantics\n");
    {
        const r = makeRig({ keepaliveMs: 0 });
        r.sender.off();
        await r.time.advanceTo(100);
        check("off before any move sends nothing", r.calls.length === 0);
        r.sender.move(0.1, 0.1);
        await microtasks();
        r.sender.move(0.2, 0.2); // pending behind the rate gate
        r.sender.off();
        await r.time.advanceTo(200);
        check("off drops the pending position", moves(r).length === 1, `${moves(r).length}`);
        check("exactly one off", offs(r) === 1);
        check("off is last", r.calls.at(-1)?.action === "off");
        r.sender.off();
        await r.time.advanceTo(300);
        check("a second off without a move sends nothing", offs(r) === 1);
        r.sender.move(0.3, 0.3);
        await r.time.advanceTo(400);
        r.sender.off();
        await r.time.advanceTo(500);
        check("move then off again sends one more off", offs(r) === 2);
        r.sender.dispose();
    }
    {
        // An off while a move is in flight waits for it.
        const r = makeRig({ manual: true, keepaliveMs: 0 });
        r.sender.move(0.1, 0.1);
        await microtasks();
        r.sender.off();
        await microtasks();
        check("off waits for the in-flight move", r.calls.length === 1);
        r.release();
        await microtasks();
        // ... and then for the rate gate (the off is a send too).
        await r.time.advanceTo(r.time.t + MIN_LASER_SEND_INTERVAL_MS + 1);
        check("off follows the move", r.calls.at(-1)?.action === "off" && r.calls.length === 2);
        check("never two in flight", r.maxInFlight() === 1);
        r.release();
        r.sender.dispose();
    }
    {
        // A queued off is superseded by a newer move.
        const r = makeRig({ manual: true, keepaliveMs: 0 });
        r.sender.move(0.1, 0.1);
        await microtasks();
        r.sender.off();
        r.sender.move(0.9, 0.9);
        r.release();
        await microtasks();
        await r.time.advanceTo(50);
        r.release();
        await microtasks();
        check("a move after a queued off cancels the off", offs(r) === 0, JSON.stringify(r.calls));
        check("and the new position goes out", moves(r).at(-1)?.x === 0.9);
        r.release();
        r.sender.dispose();
    }
}

async function testFailures(): Promise<void> {
    process.stdout.write("(e) failures are swallowed\n");
    for (const how of ["reject", "throw"] as const) {
        const r = makeRig({ behave: () => how, keepaliveMs: 0 });
        let threw = false;
        try {
            for (let i = 0; i < 10; i++) {
                r.sender.move(i / 10, 0.5);
                await r.time.advanceTo(r.time.t + 20);
            }
            r.sender.off();
            await r.time.advanceTo(r.time.t + 20);
        } catch {
            threw = true;
        }
        check(`${how}: nothing thrown`, !threw);
        check(`${how}: kept sending`, moves(r).length === 10, `${moves(r).length}`);
        check(`${how}: off still sent`, offs(r) === 1);
        check(`${how}: errors reported`, r.errors.length === 11, `${r.errors.length}`);
        r.sender.dispose();
    }
    {
        // A send that never settles is abandoned after the timeout.
        const r = makeRig({
            behave: (_input, nth) => (nth === 1 ? "hang" : "ok"),
            keepaliveMs: 0,
            sendTimeoutMs: 500,
        });
        r.sender.move(0.1, 0.1);
        await r.time.advanceTo(100);
        r.sender.move(0.2, 0.2);
        await r.time.advanceTo(400);
        check("hung send blocks only until the timeout", r.calls.length === 1);
        await r.time.advanceTo(600);
        check("after the timeout the newest position goes out", moves(r).at(-1)?.x === 0.2 && r.calls.length === 2);
        check("timeout reported", r.errors.length === 1);
        r.sender.dispose();
    }
}

async function testDispose(): Promise<void> {
    process.stdout.write("(f) dispose\n");
    const r = makeRig();
    r.sender.move(0.1, 0.1);
    await microtasks();
    r.sender.move(0.2, 0.2);
    r.sender.dispose();
    await r.time.advanceTo(10_000);
    check("nothing after dispose", r.calls.length === 1, `${r.calls.length}`);
    r.sender.move(0.3, 0.3);
    r.sender.off();
    await r.time.advanceTo(20_000);
    check("move / off after dispose are no-ops", r.calls.length === 1);
    check("no timers left", r.time.pending() === 0, `${r.time.pending()}`);
    // Bad coordinates never reach the transport.
    const r2 = makeRig({ keepaliveMs: 0 });
    for (const [x, y] of [[NaN, 0.5], [0.5, Infinity], [-0.1, 0.5], [0.5, 1.5]] as const) {
        r2.sender.move(x, y);
        await r2.time.advanceTo(r2.time.t + 20);
    }
    check("invalid coordinates are ignored", r2.calls.length === 0);
    r2.sender.dispose();
}

async function testOffGateAndClose(): Promise<void> {
    process.stdout.write("(f2) off is rate gated; close releases\n");
    // A release right after a move waits for the 60 Hz gate, so a
    // quick press / release can never exceed the server's budget.
    const r = makeRig({ keepaliveMs: 0 });
    r.sender.move(0.4, 0.4);
    await microtasks();
    r.sender.off();
    await r.time.advanceTo(100);
    check(
        "move then off",
        r.calls.length === 2 && r.calls[1]?.action === "off",
        JSON.stringify(r.calls),
    );
    check(
        "off waited for the gate",
        (r.callTimes[1] ?? 0) - (r.callTimes[0] ?? 0) >= MIN_LASER_SEND_INTERVAL_MS - 1e-9,
        `${r.callTimes}`,
    );
    // Rapid press / release cycles stay within 60 Hz overall.
    for (let k = 0; k < 30; k++) {
        r.sender.move(0.5, 0.5);
        r.sender.off();
        await r.time.advanceTo(r.time.t + 2);
    }
    await r.time.advanceTo(r.time.t + 1_000);
    let minGap = Infinity;
    for (let k = 1; k < r.callTimes.length; k++) {
        minGap = Math.min(minGap, (r.callTimes[k] ?? 0) - (r.callTimes[k - 1] ?? 0));
    }
    check(
        "no two sends closer than the gate",
        minGap >= MIN_LASER_SEND_INTERVAL_MS - 1e-9,
        `${minGap}`,
    );
    check("ends released", r.calls.at(-1)?.action === "off");
    r.sender.dispose();

    // close() on a held laser sends the final off at once.
    const c = makeRig();
    c.sender.move(0.2, 0.2);
    await microtasks();
    c.sender.close();
    await c.time.advanceTo(5_000);
    check(
        "close releases a shown laser",
        c.calls.length === 2 && c.calls[1]?.action === "off",
        JSON.stringify(c.calls),
    );
    check("close leaves no timers", c.time.pending() === 0, `${c.time.pending()}`);
    // close() during an in-flight move sends the off only after it.
    const f = makeRig({ manual: true });
    f.sender.move(0.6, 0.6);
    await microtasks();
    f.sender.close();
    await microtasks();
    check("close waits for the in-flight move", f.calls.length === 1, `${f.calls.length}`);
    f.release();
    await microtasks();
    check(
        "then releases",
        f.calls.length === 2 && f.calls[1]?.action === "off",
        JSON.stringify(f.calls),
    );
    f.release();
    // close() with nothing shown sends nothing.
    const q = makeRig();
    q.sender.close();
    await q.time.advanceTo(5_000);
    check("close of an unused laser sends nothing", q.calls.length === 0);
}

function testFilter(): void {
    process.stdout.write("(g) acceptRemoteLaser\n");
    const ctx = { roomId: "room-1", localUserId: "me" };
    const ok = { room_id: "room-1", sender_id: "them", x: 0.5, y: 0.5 };
    check("a valid move is accepted", acceptRemoteLaser(ok, ctx));
    check("edges are valid", acceptRemoteLaser({ ...ok, x: 0, y: 1 }, ctx));
    check("an off is accepted", acceptRemoteLaser({ room_id: "room-1", sender_id: "them" }, ctx, "off"));
    check("wrong room", !acceptRemoteLaser({ ...ok, room_id: "room-2" }, ctx));
    check("wrong room (off)", !acceptRemoteLaser({ room_id: "room-2", sender_id: "them" }, ctx, "off"));
    check("no room", !acceptRemoteLaser(ok, { ...ctx, roomId: null }));
    check("own user", !acceptRemoteLaser({ ...ok, sender_id: "me" }, ctx));
    check("own user (off)", !acceptRemoteLaser({ room_id: "room-1", sender_id: "me" }, ctx, "off"));
    check("unknown local id still filters room", acceptRemoteLaser(ok, { ...ctx, localUserId: null }));
    check("empty sender", !acceptRemoteLaser({ ...ok, sender_id: "" }, ctx));
    check("nil sender", !acceptRemoteLaser({ ...ok, sender_id: "00000000-0000-0000-0000-000000000000" }, ctx));
    check("missing sender", !acceptRemoteLaser({ room_id: "room-1", x: 0.5, y: 0.5 }, ctx));
    check("non-string sender", !acceptRemoteLaser({ ...ok, sender_id: 7 }, ctx));
    for (const [x, y] of [[NaN, 0.5], [0.5, Infinity], [-0.01, 0.5], [0.5, 1.01], ["0.5", 0.5], [undefined, 0.5]] as const) {
        check(`bad coordinates (${String(x)}, ${String(y)})`, !acceptRemoteLaser({ ...ok, x, y }, ctx));
    }
    check("a move without coordinates", !acceptRemoteLaser({ room_id: "room-1", sender_id: "them" }, ctx));
    check("null event", !acceptRemoteLaser(null, ctx));
    check("non-object event", !acceptRemoteLaser("laser", ctx));
}

async function main(): Promise<void> {
    await testRate();
    await testCoalesce();
    await testKeepalive();
    await testOff();
    await testFailures();
    await testDispose();
    await testOffGateAndClose();
    testFilter();
    await microtasks();
    check("no unhandled rejections", unhandled.length === 0, `${unhandled.length}`);
    if (failures > 0) {
        process.stdout.write(`\n${failures} check(s) FAILED\n`);
        // `exitCode` rather than `exit()`: exiting while a handle
        // is closing trips a libuv assertion on Windows.
        process.exitCode = 1;
        return;
    }
    process.stdout.write("\nall laser transport checks passed\n");
}

main().catch((err: unknown) => {
    process.stdout.write(`FATAL ${String(err)}\n`);
    process.exitCode = 1;
});
