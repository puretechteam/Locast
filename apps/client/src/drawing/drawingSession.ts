// P5-T02: the network half of local drawing.
//
// `DrawingSession` turns local stroke activity into the
// DRAW_BEGIN / DRAW_POINT / DRAW_END `drawing_send` calls
// (and, P5-T03, the DRAW_UNDO / DRAW_CLEAR ones).
// It is deliberately free of React, Tauri and DOM imports
// (the transport is injected) so the Node smoke test
// `drawingSession.smoke.ts` can drive the exact production
// logic. The production wrapper that injects the real Tauri
// transport is `services/drawing.ts` (`DrawingService`).
//
// Guarantees:
//
// 1. Ordering. Every `drawing_send` call is queued behind
//    the previous one (one send in flight), so the Rust side
//    sees BEGIN, then the POINTs, then END in the order the
//    user produced them. Without the queue two concurrent
//    Tauri async commands could overtake each other (the
//    BEGIN command loads the signing key from the keyring
//    first and is the slowest), and the server rejects a
//    POINT / END whose BEGIN has not arrived yet.
//
// 2. Rate. `appendPoint` only stores the newest point
//    (last-point-wins). A frame callback (rAF in the app)
//    sends at most one DRAW_POINT per frame AND never closer
//    together than 1000 / MAX_DRAW_POINT_HZ ms, so even a
//    240 Hz display or a 1000 Hz mouse cannot exceed the
//    80 Hz ceiling.
//
// 3. Closure. `endStroke` flushes the newest pending point
//    regardless of the rate gate (the trailing position is
//    never lost) and then queues DRAW_END. Failures are
//    reported through `onError` and never thrown from
//    `appendPoint`; `beginStroke` / `endStroke` reject only
//    to the caller that awaits them.
//
// 4. Failed BEGIN. If DRAW_BEGIN cannot be sent (not in a
//    room, key unavailable, IPC failure) the rest of that
//    stroke is dropped locally instead of generating
//    POINT / END envelopes the server would reject as
//    "unknown stroke".
//
// 5. Hung transport. Each send has a timeout
//    (`sendTimeoutMs`, default 3000). A send that never
//    settles is treated as a failed send (reported via
//    `onError`; a failed BEGIN drops the rest of its stroke)
//    and the queue moves on, so one stuck IPC call cannot
//    wedge every later stroke. While a send is stuck, the
//    not-yet-started DRAW_POINTs waiting behind it are
//    bounded to `MAX_QUEUED_POINTS`: when full, the OLDEST
//    queued point is dropped (last-point-wins). BEGIN and END
//    are never dropped and order is preserved.
//
// 6. Undo and clear (P5-T03). `undoStroke` / `clearAll` go
//    through the SAME ordered queue, so an undo of the stroke
//    that was just drawn is sent after that stroke's DRAW_END
//    (the server only undoes ended strokes). They change
//    nothing locally: the stroke leaves the canvas when the
//    server's DRAW_UNDO event comes back (to the actor too), so
//    a refused undo can never diverge the actor's screen.

import type { DrawingSendInput, DrawingSendResult } from "../bindings/index";
import { MAX_DRAW_POINT_HZ } from "./constants.ts";
import { newStrokeId } from "./types.ts";

/** Convert Hz to the minimum interval between
 *  DRAW_POINT sends. `1000 / 80 = 12.5 ms`. With the
 *  default integer-ms clock the effective gap is 9 ms
 *  (about 77 Hz). */
export const MIN_FLUSH_INTERVAL_MS = 1000 / MAX_DRAW_POINT_HZ;

/** Default per-send timeout, ms. */
export const DEFAULT_SEND_TIMEOUT_MS = 3000;

/** Most DRAW_POINTs that may wait (not yet started) in the
 *  send queue. About 400 ms of drawing at the 80 Hz ceiling. */
export const MAX_QUEUED_POINTS = 32;

/** Injected transport: the production value calls the
 *  `drawing_send` Tauri command. */
export type DrawingSendFn = (input: DrawingSendInput) => Promise<DrawingSendResult>;

/** A single coalesced DRAW_POINT payload. Mirrors
 *  `locast_protocol::room::StrokePointPayload` minus
 *  the stroke id (the session stamps it). */
export interface StrokePointPayload {
    x: number;
    y: number;
    pressure: number;
    tsMs: number;
}

/** Options for `beginStroke`. */
export interface BeginStrokeOptions {
    tool: "pen" | "arrow" | "rect" | "circle" | "text" | "eraser";
    color: string;
    width: number;
    x: number;
    y: number;
    pressure: number;
    tsMs: number;
    /** Use this id instead of generating one. The local
     *  renderer passes its own stroke id so the id in the
     *  local store IS the id on the wire. Must be a
     *  canonical UUID (the Rust command rejects anything
     *  else). */
    strokeId?: string;
}

/** The handle returned from `beginStroke`. */
export interface StrokeHandle {
    strokeId: string;
}

export interface DrawingSessionOptions {
    /** Millisecond clock (default `Date.now`). */
    now?: () => number;
    /** Frame scheduler (default `requestAnimationFrame`,
     *  or a 16 ms timeout where rAF does not exist). */
    requestFrame?: (cb: () => void) => number;
    cancelFrame?: (handle: number) => void;
    /** Called for every failed `drawing_send`. The default
     *  logs a warning. */
    onError?: (err: unknown, input: DrawingSendInput) => void;
    /** Per-send timeout, ms (default 3000). A send still
     *  pending after this long counts as failed. `0` or
     *  `Infinity` disables the timeout. */
    sendTimeoutMs?: number;
}

interface QueuedSend {
    stroke: ActiveStroke;
    input: DrawingSendInput;
    resolve: (res: DrawingSendResult | null) => void;
    reject: (err: unknown) => void;
}

interface ActiveStroke {
    id: string;
    /** `true` once DRAW_BEGIN failed to send: the rest of
     *  the stroke is dropped. */
    failed: boolean;
}

type FrameHandle = number;

function defaultRequestFrame(cb: () => void): FrameHandle {
    const g = globalThis as { requestAnimationFrame?: (cb: () => void) => number };
    if (typeof g.requestAnimationFrame === "function") {
        return g.requestAnimationFrame(cb);
    }
    return setTimeout(cb, 16) as unknown as number;
}

function defaultCancelFrame(handle: FrameHandle): void {
    const g = globalThis as { cancelAnimationFrame?: (h: number) => void };
    if (typeof g.cancelAnimationFrame === "function") {
        g.cancelAnimationFrame(handle);
        return;
    }
    clearTimeout(handle as unknown as ReturnType<typeof setTimeout>);
}

export class DrawingSession {
    /** Id of the stroke in progress (`null` between strokes). */
    activeStrokeId: string | null = null;
    /** Last `client_seq` handed out for the active stroke. */
    activeSeq = 0;
    /** Pending network point (last-point-wins). */
    pendingPoint: StrokePointPayload | null = null;

    private active: ActiveStroke | null = null;
    private lastFlushMs = Number.NEGATIVE_INFINITY;
    private frameHandle: FrameHandle | null = null;
    private tasks: QueuedSend[] = [];
    private queuedPoints = 0;
    private pumping = false;
    private idleWaiters: Array<() => void> = [];
    private readonly sendTimeoutMs: number;

    private readonly send: DrawingSendFn;
    private readonly now: () => number;
    private readonly requestFrame: (cb: () => void) => number;
    private readonly cancelFrame: (handle: number) => void;
    private readonly onError: (err: unknown, input: DrawingSendInput) => void;

    constructor(send: DrawingSendFn, opts: DrawingSessionOptions = {}) {
        this.send = send;
        this.now = opts.now ?? Date.now;
        this.requestFrame = opts.requestFrame ?? defaultRequestFrame;
        this.cancelFrame = opts.cancelFrame ?? defaultCancelFrame;
        this.sendTimeoutMs = opts.sendTimeoutMs ?? DEFAULT_SEND_TIMEOUT_MS;
        this.onError =
            opts.onError ??
            ((err, input) => {
                console.warn(`drawing_send ${input.action} failed`, err);
            });
    }

    /**
     * Emit DRAW_BEGIN. The stroke becomes active
     * synchronously (so `appendPoint` right after the call
     * is accepted); the returned promise settles when the
     * BEGIN send completes. If a previous stroke is still
     * open it is closed first (its END is queued ahead of
     * this BEGIN).
     */
    public beginStroke(opts: BeginStrokeOptions): Promise<StrokeHandle> {
        if (this.active !== null) {
            // Defensive: a previous stroke was not ended.
            // Close it so the server's pending-strokes map
            // does not accumulate orphan bindings.
            this.endStroke().catch(() => undefined);
        }
        const stroke: ActiveStroke = {
            id: opts.strokeId ?? newStrokeId(),
            failed: false,
        };
        this.active = stroke;
        this.activeStrokeId = stroke.id;
        this.activeSeq = 1;
        this.pendingPoint = null;
        const input: DrawingSendInput = {
            action: "begin",
            stroke_id: stroke.id,
            tool: opts.tool,
            color: opts.color,
            width: opts.width,
            x: opts.x,
            y: opts.y,
            pressure: opts.pressure,
            ts_ms: opts.tsMs,
            client_seq: this.activeSeq,
        };
        return this.enqueue(stroke, input).then((res) => ({
            strokeId: res?.stroke_id ?? stroke.id,
        }));
    }

    /**
     * Record the newest pointer position for the active
     * stroke. Never sends directly and never throws: it
     * overwrites `pendingPoint` and schedules a frame.
     * The local canvas is updated separately by the caller.
     */
    public appendPoint(point: StrokePointPayload): void {
        if (this.active === null) return;
        this.pendingPoint = point;
        this.scheduleFlush();
    }

    /**
     * Emit DRAW_END. The newest pending point is flushed
     * first (bypassing the rate gate). The stroke is closed
     * synchronously; the returned promise settles when the
     * END send completes and resolves with the stroke id
     * (`null` if no stroke was in progress).
     */
    public endStroke(): Promise<string | null> {
        const stroke = this.active;
        if (stroke === null) return Promise.resolve(null);
        this.flushPending(true);
        this.cancelScheduledFrame();
        this.active = null;
        this.activeStrokeId = null;
        this.pendingPoint = null;
        this.activeSeq += 1;
        const input: DrawingSendInput = {
            action: "end",
            stroke_id: stroke.id,
            ts_ms: this.now(),
            client_seq: this.activeSeq,
        };
        return this.enqueue(stroke, input).then(() => stroke.id);
    }

    /**
     * Close the active stroke (if any) and swallow the
     * outcome. Used on unmount / room change / capability
     * loss, where nobody can handle a rejection.
     */
    public async dispose(): Promise<void> {
        try {
            await this.endStroke();
        } catch {
            // Already reported through `onError`.
        }
    }

    /**
     * Abandon the active stroke locally WITHOUT emitting
     * DRAW_END. The server keeps the stroke open until the
     * room ends, so the app does not use this: pointer
     * cancel / leave go through `endStroke`. Kept for tests
     * that assert no phantom END is produced.
     */
    public cancelStroke(): void {
        this.active = null;
        this.activeStrokeId = null;
        this.pendingPoint = null;
        this.cancelScheduledFrame();
    }

    /**
     * Ask the server to remove one committed stroke (DRAW_UNDO).
     * Queued behind everything already sent, so it follows the
     * stroke's own DRAW_END. Resolves when the send completes;
     * rejects (after `onError` ran) if it fails or times out. The
     * canvas is NOT touched here: the stroke is removed when the
     * server's undo event arrives.
     */
    public undoStroke(strokeId: string): Promise<void> {
        return this.enqueue(
            { id: strokeId, failed: false },
            { action: "undo", stroke_id: strokeId },
        ).then(() => undefined);
    }

    /**
     * Ask the server to wipe every stroke in the room
     * (DRAW_CLEAR). Same queueing and "canvas changes only on the
     * server's event" rules as `undoStroke`.
     */
    public clearAll(): Promise<void> {
        return this.enqueue({ id: "clear", failed: false }, { action: "clear" }).then(
            () => undefined,
        );
    }

    /** Resolves once every send queued so far has settled. */
    public idle(): Promise<void> {
        if (!this.pumping && this.tasks.length === 0) return Promise.resolve();
        return new Promise<void>((resolve) => {
            this.idleWaiters.push(resolve);
        });
    }

    /** Number of sends waiting behind the one in flight
     *  (exposed for tests: it must stay bounded). */
    public get queuedSendCount(): number {
        return this.tasks.length;
    }

    private scheduleFlush(): void {
        if (this.frameHandle !== null) return;
        this.frameHandle = this.requestFrame(() => {
            this.frameHandle = null;
            this.flushPending(false);
            if (this.pendingPoint !== null && this.active !== null) {
                this.scheduleFlush();
            }
        });
    }

    private cancelScheduledFrame(): void {
        if (this.frameHandle !== null) {
            this.cancelFrame(this.frameHandle);
            this.frameHandle = null;
        }
    }

    /**
     * Queue one DRAW_POINT for the pending point. Unless
     * `force` is set, nothing is sent while the minimum
     * interval since the previous DRAW_POINT has not
     * elapsed (the pending point stays and a later frame
     * retries).
     */
    private flushPending(force: boolean): void {
        const stroke = this.active;
        const point = this.pendingPoint;
        if (stroke === null || point === null) return;
        const t = this.now();
        if (!force && t - this.lastFlushMs < MIN_FLUSH_INTERVAL_MS) return;
        this.pendingPoint = null;
        this.lastFlushMs = t;
        this.activeSeq += 1;
        this.enqueue(stroke, {
            action: "point",
            stroke_id: stroke.id,
            x: point.x,
            y: point.y,
            pressure: point.pressure,
            ts_ms: point.tsMs,
            client_seq: this.activeSeq,
        }).catch(() => undefined);
    }

    /**
     * Append one send to the ordered queue. Resolves with
     * the command result, or `null` when the send was
     * skipped (DRAW_BEGIN for the stroke failed) or dropped
     * (queue full of points). Rejects when the send fails or
     * times out (after `onError` ran).
     */
    private enqueue(
        stroke: ActiveStroke,
        input: DrawingSendInput,
    ): Promise<DrawingSendResult | null> {
        return new Promise((resolve, reject) => {
            if (input.action === "point") {
                if (this.queuedPoints >= MAX_QUEUED_POINTS) {
                    const idx = this.tasks.findIndex((t) => t.input.action === "point");
                    if (idx >= 0) {
                        const [dropped] = this.tasks.splice(idx, 1);
                        dropped?.resolve(null);
                        this.queuedPoints -= 1;
                    }
                }
                this.queuedPoints += 1;
            }
            this.tasks.push({ stroke, input, resolve, reject });
            void this.pump();
        });
    }

    /** Run queued sends one at a time, in order. */
    private async pump(): Promise<void> {
        if (this.pumping) return;
        this.pumping = true;
        try {
            for (let t = this.tasks.shift(); t !== undefined; t = this.tasks.shift()) {
                if (t.input.action === "point") this.queuedPoints -= 1;
                try {
                    t.resolve(await this.run(t));
                } catch (err) {
                    t.reject(err);
                }
            }
        } finally {
            this.pumping = false;
            const waiters = this.idleWaiters;
            this.idleWaiters = [];
            for (const w of waiters) w();
        }
    }

    private async run(t: QueuedSend): Promise<DrawingSendResult | null> {
        if (t.stroke.failed && t.input.action !== "begin") return null;
        try {
            return await this.sendWithTimeout(t.input);
        } catch (err) {
            if (t.input.action === "begin") t.stroke.failed = true;
            this.onError(err, t.input);
            throw err;
        }
    }

    /** `send` raced against the per-send timeout. A send that
     *  settles after the timeout is ignored. */
    private sendWithTimeout(input: DrawingSendInput): Promise<DrawingSendResult> {
        return new Promise((resolve, reject) => {
            let done = false;
            const limited = Number.isFinite(this.sendTimeoutMs) && this.sendTimeoutMs > 0;
            const timer = limited
                ? setTimeout(() => {
                      if (done) return;
                      done = true;
                      reject(
                          new Error(
                              `drawing_send ${input.action} timed out after ${this.sendTimeoutMs} ms`,
                          ),
                      );
                  }, this.sendTimeoutMs)
                : undefined;
            const settle = (fn: () => void): void => {
                if (done) return;
                done = true;
                if (timer !== undefined) clearTimeout(timer);
                fn();
            };
            let pending: Promise<DrawingSendResult>;
            try {
                pending = this.send(input);
            } catch (err) {
                settle(() => reject(err));
                return;
            }
            pending.then(
                (res) => settle(() => resolve(res)),
                (err: unknown) => settle(() => reject(err)),
            );
        });
    }
}
