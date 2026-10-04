// P5-T04: the network half of the laser pointer.
//
// `LaserSender` turns local laser activity into `laser_send`
// calls (LASER_MOVE / LASER_OFF); `acceptRemoteLaser` decides
// whether a relayed `laser://move` / `laser://off` event may
// touch the canvas. Deliberately free of React, Tauri and DOM
// imports (the transport, clock and timers are injected) so the
// Node smoke test `laserTransport.smoke.ts` drives the exact
// production logic. The React wiring is
// `hooks/useLaserTransport.ts`.
//
// Sender guarantees:
//
// 1. Rate. `move` only stores the newest position
//    (last-point-wins). Sends (moves, keepalives and offs alike)
//    go out no closer together than `MIN_LASER_SEND_INTERVAL_MS`
//    (1000 / 60), matching the server's 60 msg/s per-connection
//    laser budget, which drops the excess silently (an off it
//    dropped would leave the remote trail up until idle expiry).
// 2. One in flight. At most one `laser_send` is outstanding.
//    Positions that arrive meanwhile coalesce into the single
//    pending slot; nothing queues up behind a slow IPC call. A
//    send that never settles counts as finished after
//    `sendTimeoutMs` so it cannot wedge the laser.
// 3. Keepalive. While the laser is held still, the last
//    position is re-sent every `LASER_KEEPALIVE_MS`, so remote
//    idle expiry (`REMOTE_LASER_IDLE_MS`) never fades a
//    stationary pointer.
// 4. Off. `off` drops any pending position and sends one
//    LASER_OFF, but only if a move went out since the last off
//    (a laser nobody saw needs no release). `close` (teardown)
//    sends that final off at once, then disposes.
// 5. Best effort. Send failures are swallowed (reported to the
//    optional `onError`); nothing is thrown from `move` / `off`.

import type { LaserSendInput } from "../bindings/index";

/** Most laser messages per second the client sends. The server
 *  allows 60 per connection (burst 60) and drops the rest. */
export const MAX_LASER_HZ = 60;

/** Minimum gap between two LASER_MOVE sends, ms. */
export const MIN_LASER_SEND_INTERVAL_MS = 1000 / MAX_LASER_HZ;

/** While the laser is held still the last position is re-sent
 *  this often, ms. */
export const LASER_KEEPALIVE_MS = 1000;

/** A remote trail with no update for this long fades out, ms.
 *  Matches ARCHITECTURE section 25.5 (3 s auto-release) and is
 *  more than twice `LASER_KEEPALIVE_MS`, so one lost keepalive
 *  does not fade a stationary pointer. */
export const REMOTE_LASER_IDLE_MS = 3000;

/** Default per-send timeout, ms. */
export const DEFAULT_LASER_SEND_TIMEOUT_MS = 2000;

/** Injected transport: the production value calls the
 *  `laser_send` Tauri command. */
export type LaserSendFn = (input: LaserSendInput) => Promise<unknown>;

export interface LaserSenderOptions {
    /** Monotonic millisecond clock (default `performance.now`, so
     *  a wall-clock step cannot stall the rate gate). */
    now?: () => number;
    /** Timer scheduler (default `setTimeout` / `clearTimeout`). */
    setTimer?: (cb: () => void, ms: number) => unknown;
    clearTimer?: (handle: unknown) => void;
    /** Override `MIN_LASER_SEND_INTERVAL_MS`. */
    minIntervalMs?: number;
    /** Override `LASER_KEEPALIVE_MS`; `0` or `Infinity` disables it. */
    keepaliveMs?: number;
    /** Override `DEFAULT_LASER_SEND_TIMEOUT_MS`; `0` or `Infinity`
     *  disables it. */
    sendTimeoutMs?: number;
    /** Called for every failed send (the default ignores it). */
    onError?: (err: unknown, input: LaserSendInput) => void;
}

/** A finite number within [0, 1]. */
export function isUnit(n: unknown): n is number {
    return typeof n === "number" && Number.isFinite(n) && n >= 0 && n <= 1;
}

export class LaserSender {
    private readonly send: LaserSendFn;
    private readonly now: () => number;
    private readonly setTimer: (cb: () => void, ms: number) => unknown;
    private readonly clearTimer: (handle: unknown) => void;
    private readonly minIntervalMs: number;
    private readonly keepaliveMs: number;
    private readonly sendTimeoutMs: number;
    private readonly onError: ((err: unknown, input: LaserSendInput) => void) | undefined;

    /** Newest position not yet sent (last-point-wins). */
    private pending: { x: number; y: number } | null = null;
    /** A LASER_OFF waits for the in-flight send. */
    private offPending = false;
    /** The laser is held: keepalives run. */
    private held = false;
    /** A move went out since the last off. */
    private movedSinceOff = false;
    /** Last position sent (for keepalives). */
    private lastPos: { x: number; y: number } | null = null;
    /** When the last move started, by `now()` (keepalive base). */
    private lastMoveAt = Number.NEGATIVE_INFINITY;
    /** When the last send of any kind started (rate gate). */
    private lastSendAt = Number.NEGATIVE_INFINITY;
    private inFlight = false;
    /** Settles when the latest send does (never rejects). */
    private lastSettled: Promise<void> = Promise.resolve();
    private timer: unknown = null;
    /** What the armed timer is for. */
    private timerKind: "gate" | "keepalive" | null = null;
    private disposed = false;

    constructor(send: LaserSendFn, opts: LaserSenderOptions = {}) {
        this.send = send;
        this.now =
            opts.now ??
            (typeof performance !== "undefined"
                ? () => performance.now()
                : () => Date.now());
        this.setTimer = opts.setTimer ?? ((cb, ms) => setTimeout(cb, ms));
        this.clearTimer =
            opts.clearTimer ??
            ((h) => clearTimeout(h as ReturnType<typeof setTimeout>));
        this.minIntervalMs = opts.minIntervalMs ?? MIN_LASER_SEND_INTERVAL_MS;
        this.keepaliveMs = opts.keepaliveMs ?? LASER_KEEPALIVE_MS;
        this.sendTimeoutMs = opts.sendTimeoutMs ?? DEFAULT_LASER_SEND_TIMEOUT_MS;
        this.onError = opts.onError;
    }

    /** The local laser is at (x, y), normalized to the video frame.
     *  Values outside [0, 1] or non-finite are ignored (the caller
     *  clamps; the Rust command would reject them). */
    move(x: number, y: number): void {
        if (this.disposed) return;
        if (!isUnit(x) || !isUnit(y)) return;
        this.pending = { x, y };
        this.held = true;
        // A newer position supersedes a release that has not gone
        // out yet: the remote trail simply continues.
        this.offPending = false;
        this.pump();
    }

    /** The local user released the laser. */
    off(): void {
        if (this.disposed) return;
        this.pending = null;
        this.held = false;
        this.cancelTimer();
        if (!this.movedSinceOff) return;
        this.offPending = true;
        this.pump();
    }

    /** Teardown (unmount, room change): if the remote side may be
     *  showing this laser, send the final LASER_OFF (best effort,
     *  not rate gated: it is the last message) once any in-flight
     *  send has settled, so it cannot overtake a move; then
     *  dispose. */
    close(): void {
        if (this.disposed) return;
        const release = this.movedSinceOff;
        this.dispose();
        if (!release) return;
        const off: LaserSendInput = { action: "off" };
        void this.lastSettled.then(() => {
            try {
                Promise.resolve(this.send(off)).catch((err: unknown) =>
                    this.onError?.(err, off),
                );
            } catch (err) {
                this.onError?.(err, off);
            }
        });
    }

    /** Stop all timers and drop pending work. Sends nothing; use
     *  `close` when the remote side should see a release. */
    dispose(): void {
        this.disposed = true;
        this.pending = null;
        this.offPending = false;
        this.held = false;
        this.cancelTimer();
    }

    /** Send the next due message, or arm the timer for it. */
    private pump(): void {
        if (this.disposed || this.inFlight) return;
        if (this.offPending) {
            const wait = this.lastSendAt + this.minIntervalMs - this.now();
            if (wait > 0) {
                if (this.timerKind !== "gate") this.armTimer(wait, "gate");
                return;
            }
            this.offPending = false;
            this.movedSinceOff = false;
            this.lastPos = null;
            this.cancelTimer();
            this.dispatch({ action: "off" });
            return;
        }
        if (this.pending !== null) {
            const wait = this.lastSendAt + this.minIntervalMs - this.now();
            if (wait > 0) {
                // Already waiting for the rate gate: the newer
                // position just replaced the pending one.
                if (this.timerKind !== "gate") this.armTimer(wait, "gate");
                return;
            }
            const pos = this.pending;
            this.pending = null;
            this.lastPos = pos;
            this.lastMoveAt = this.now();
            this.movedSinceOff = true;
            this.cancelTimer();
            this.dispatch({ action: "move", x: pos.x, y: pos.y });
            return;
        }
        // Idle while held: schedule the keepalive.
        if (
            this.held &&
            this.lastPos !== null &&
            this.keepaliveMs > 0 &&
            Number.isFinite(this.keepaliveMs)
        ) {
            const wait = Math.max(0, this.lastMoveAt + this.keepaliveMs - this.now());
            this.armTimer(wait, "keepalive");
        }
    }

    private armTimer(ms: number, kind: "gate" | "keepalive"): void {
        this.cancelTimer();
        this.timerKind = kind;
        this.timer = this.setTimer(() => {
            this.timer = null;
            this.timerKind = null;
            if (this.disposed) return;
            if (kind === "keepalive" && this.held && this.pending === null && this.lastPos !== null) {
                this.pending = this.lastPos;
            }
            this.pump();
        }, ms);
    }

    private cancelTimer(): void {
        if (this.timer !== null) {
            this.clearTimer(this.timer);
            this.timer = null;
        }
        this.timerKind = null;
    }

    private dispatch(input: LaserSendInput): void {
        this.inFlight = true;
        this.lastSendAt = this.now();
        let settled = false;
        let timeout: unknown = null;
        const done = (): void => {
            if (settled) return;
            settled = true;
            if (timeout !== null) this.clearTimer(timeout);
            this.inFlight = false;
            this.pump();
        };
        if (this.sendTimeoutMs > 0 && Number.isFinite(this.sendTimeoutMs)) {
            timeout = this.setTimer(() => {
                timeout = null;
                if (!settled) this.onError?.(new Error("laser_send timed out"), input);
                done();
            }, this.sendTimeoutMs);
        }
        let p: Promise<unknown>;
        try {
            p = Promise.resolve(this.send(input));
        } catch (err) {
            p = Promise.reject(err);
        }
        this.lastSettled = p.then(
            () => undefined,
            () => undefined,
        );
        p.then(
            () => done(),
            (err: unknown) => {
                if (!settled) this.onError?.(err, input);
                done();
            },
        );
    }
}

/** What a remote laser event is checked against. */
export interface LaserFilterContext {
    /** The room this client is in; `null` rejects everything. */
    roomId: string | null;
    /** The local user's server-assigned id, if known. Events from
     *  it are the local user's own and are rejected. */
    localUserId: string | null;
}

const NIL_UUID = "00000000-0000-0000-0000-000000000000";

/**
 * Whether a relayed `laser://move` (`kind` "move", the default)
 * or `laser://off` event may be applied: it is for the current
 * room, names a real sender that is not the local user, and (for
 * a move) carries finite coordinates within [0, 1]. The Rust room
 * client already checks all of this; the webview re-checks
 * because the event bus is the trust boundary of this layer.
 */
export function acceptRemoteLaser(
    event: unknown,
    ctx: LaserFilterContext,
    kind: "move" | "off" = "move",
): boolean {
    if (typeof event !== "object" || event === null) return false;
    const ev = event as { room_id?: unknown; sender_id?: unknown; x?: unknown; y?: unknown };
    if (ctx.roomId === null || ev.room_id !== ctx.roomId) return false;
    if (typeof ev.sender_id !== "string" || ev.sender_id.length === 0) return false;
    if (ev.sender_id === NIL_UUID) return false;
    if (ctx.localUserId !== null && ev.sender_id === ctx.localUserId) return false;
    if (kind === "move" && (!isUnit(ev.x) || !isUnit(ev.y))) return false;
    return true;
}
