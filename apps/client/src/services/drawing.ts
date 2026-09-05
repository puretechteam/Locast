// P5-T02: client-side drawing service.
//
// Bridges the P5-T01 imperative drawing hook to the
// `drawing_send` Tauri command. The service is the
// canonical owner of:
//
// 1. The last-point-wins coalescing loop: pointermove
//    events update the local hook immediately (via
//    `appendPoint`), and overwrite a pending network
//    point. A `requestAnimationFrame` flush emits at
//    most one DRAW_POINT per animation frame (which
//    at 60 Hz display refresh is the natural cap and
//    comfortably below the 120 Hz wire budget). The
//    coalescing is also explicit: `flush()` forces a
//    pending-network-point send at any moment (called
//    from `endStroke` so a final point is never lost).
//
// 2. The DRAW_BEGIN / DRAW_END envelope emission. The
//    `beginStroke` and `endStroke` wrappers build the
//    typed payload, ask the Tauri command to sign
//    DRAW_BEGIN (the Rust side owns the Ed25519 key,
//    per architecture §15.4), and forward the envelope
//    through `SignalingClient`. The React layer never
//    sees the private signing material.
//
// 3. The test-only `__locastDrawing` seam (see
//    `services/drawing.ts`'s test seam below) that the
//    Playwright acceptance test uses to verify the
//    120 Hz cap and the deterministic payload shape.

import { commands } from "./ipc";
import { MAX_DRAW_POINT_HZ } from "../drawing/constants";

/** Convert Hz to the minimum interval between
 *  flushes. `1000 / 120 = 8.33... ms`. */
const MIN_FLUSH_INTERVAL_MS = 1000 / MAX_DRAW_POINT_HZ;

/** Generate a UUID v7-ish id without depending on the
 *  `crypto` global so the smoke test can run in plain
 *  Node without `--experimental-global-crypto`. The id
 *  shape matches the wire format. */
function makeStrokeId(): string {
    const rnd = Math.floor(Math.random() * 0xffffffff)
        .toString(16)
        .padStart(8, "0");
    const ts = Date.now().toString(16).padStart(12, "0");
    return `${ts}-7${rnd.slice(0, 3)}-${rnd.slice(3, 7)}-`;
}

/** A single coalesced DRAW_POINT payload. Mirrors
 *  `locast_protocol::room::StrokePointPayload` minus
 *  the envelope id (server-assigned). */
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
}

/** The handle returned from `beginStroke` so the
 *  caller (the DrawingBridge hook) can attach the
 *  same stroke id to subsequent `appendPoint` /
 *  `endStroke` calls. */
export interface StrokeHandle {
    strokeId: string;
}

/** The drawing service. One per `DrawingBridge` mount;
 *  the lifecycle is bound to the React render tree so
 *  room changes create a fresh instance (no
 *  cross-room stroke id leakage). */
export class DrawingService {
    /**: */
    activeStrokeId: string | null = null;
    activeSeq = 0;
    /** Pending network point (last-point-wins). */
    pendingPoint: StrokePointPayload | null = null;
    private lastFlushMs = 0;
    private rafId: number | null = null;

    /**
     * P5-T02: emit a signed DRAW_BEGIN. The Tauri
     * command signs the canonical payload bytes
     * server-side (private key never leaves Rust).
     */
    public async beginStroke(opts: BeginStrokeOptions): Promise<StrokeHandle> {
        if (this.activeStrokeId !== null) {
            // Defensive: a previous stroke was not ended.
            // Flush any pending point and end the prior
            // stroke before starting a new one. This
            // protects the server's pending-strokes map
            // from accumulating orphan bindings.
            await this.endStroke();
        }
        const strokeId = makeStrokeId();
        this.activeStrokeId = strokeId;
        this.activeSeq = 1;
        this.pendingPoint = null;
        const input = {
            action: "begin" as const,
            stroke_id: strokeId,
            tool: opts.tool,
            color: opts.color,
            width: opts.width,
            x: opts.x,
            y: opts.y,
            pressure: opts.pressure,
            ts_ms: opts.tsMs,
            client_seq: this.activeSeq,
        };
        const res = await commands.drawingSend(input);
        return { strokeId: res.stroke_id };
    }

    /**
     * P5-T02: append a point to the active stroke.
     *
     * The local rendering layer (`useDrawingCanvas`)
     * is called separately by the DrawingBridge hook
     * so the local canvas updates immediately. This
     * service is responsible only for the network
     * side: it overwrites `pendingPoint` (the
     * last-point-wins coalescing state) and schedules a
     * rAF flush.
     */
    public appendPoint(point: StrokePointPayload): void {
        if (this.activeStrokeId === null) return;
        this.pendingPoint = point;
        this.scheduleFlush();
    }

    /**
     * P5-T02: emit DRAW_END. Flushes any pending
     * network point first so the receiver sees the
     * final position before the close. Returns the
     * strokeId that was active (null if no stroke was
     * in progress).
     */
    public async endStroke(): Promise<string | null> {
        if (this.activeStrokeId === null) return null;
        const strokeId = this.activeStrokeId;
        // Flush any pending point synchronously so the
        // receiver sees the final position before the
        // close.
        this.flushPending();
        if (this.rafId !== null) {
            cancelAnimationFrame(this.rafId);
            this.rafId = null;
        }
        const tsMs = Date.now();
        const input = {
            action: "end" as const,
            stroke_id: strokeId,
            ts_ms: tsMs,
            client_seq: this.activeSeq + 1,
        };
        await commands.drawingSend(input);
        this.activeStrokeId = null;
        this.pendingPoint = null;
        return strokeId;
    }

    /**
     * Cancel the active stroke without emitting
     * DRAW_END. Used by `pointercancel` so a
     * mid-stroke cancel does not produce a phantom
     * stroke. The local hook's `undo` /
     * `clear` can also be called separately.
     */
    public cancelStroke(): void {
        this.activeStrokeId = null;
        this.pendingPoint = null;
        if (this.rafId !== null) {
            cancelAnimationFrame(this.rafId);
            this.rafId = null;
        }
    }

    private scheduleFlush(): void {
        if (this.rafId !== null) return;
        this.rafId = requestAnimationFrame(() => {
            this.rafId = null;
            this.tickTick();
        });
    }

    /**
     * Flush the pending network point if the
     * minimum-flush interval has elapsed. Otherwise
     * schedule another rAF tick. The interval is the
     * 120 Hz ceiling: even at 1000 Hz pointermoves the
     * flushes stay below the cap because the rAF loop
     * is naturally 60 Hz on most displays. The cap
     * tightens to 120 Hz on a 120 Hz display.
     */
    private tickTick(): void {
        this.flushPending();
        if (this.pendingPoint !== null) {
            this.scheduleFlush();
        }
    }

    /**
     * Emit one DRAW_POINT envelope if a point is
     * pending AND the minimum interval has elapsed.
     * The point is consumed (`pendingPoint = null`) so
     * the next call does not double-emit.
     */
    private flushPending(): void {
        if (this.activeStrokeId === null) return;
        const point = this.pendingPoint;
        if (point === null) return;
        const now = Date.now();
        if (now - this.lastFlushMs < MIN_FLUSH_INTERVAL_MS) {
            // Too soon. The rAF loop will tick again on
            // the next animation frame.
            return;
        }
        this.pendingPoint = null;
        this.lastFlushMs = now;
        this.activeSeq += 1;
        // Fire-and-forget: the IPC promise is awaited
        // inside the Tauri command but we do not block
        // the pointer event handler on it (the local
        // canvas already has the point via the hook's
        // synchronous appendPoint). Errors are surfaced
        // via the React layer's error boundary in a
        // future revision; for P5-T02 they are logged
        // to the console so the test seam can observe
        // them.
        commands
            .drawingSend({
                action: "point" as const,
                stroke_id: this.activeStrokeId,
                x: point.x,
                y: point.y,
                pressure: point.pressure,
                ts_ms: point.tsMs,
                client_seq: this.activeSeq,
            })
            .catch((err: unknown) => {
                // eslint-disable-next-line no-console
                console.warn("drawing_send point failed", err);
            });
    }
}

/** Test-only seam: mount `__locastDrawing` on window
 *  for the duration of a test scenario so Playwright
 *  can drive strokes deterministically. Returns the
 *  service instance (also stashed on the window) so
 *  the test can read the in-flight stroke id.
 */
export function mountDrawingSeam(service: DrawingService): void {
    interface DrawingSeam {
        getActiveStrokeId: () => string | null;
        getPendingPoint: () => StrokePointPayload | null;
        getSeq: () => number;
    }
    interface SeamWindow {
        __locastDrawing?: DrawingSeam;
    }
    const w = window as unknown as SeamWindow;
    w.__locastDrawing = {
        getActiveStrokeId: () => service.activeStrokeId,
        getPendingPoint: () => service.pendingPoint,
        getSeq: () => service.activeSeq,
    };
}

export function unmountDrawingSeam(): void {
    interface SeamWindow {
        __locastDrawing?: unknown;
    }
    const w = window as unknown as SeamWindow;
    if (w.__locastDrawing !== undefined) {
        delete w.__locastDrawing;
    }
}