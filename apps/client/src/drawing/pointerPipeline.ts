// P5-T02: pointer events -> local stroke + network stroke.
//
// `PointerStrokePipeline` is the single place that couples
// the local drawing hook (`useDrawingCanvas`: store +
// renderer) with the network service (`DrawingService`):
//
//   pointer down  -> local beginStroke (returns the id)
//                    + DRAW_BEGIN with THAT id
//   pointer move  -> local appendPoint + coalesced DRAW_POINT
//   pointer up /  -> local endStroke + DRAW_END
//   cancel / leave
//   finish()      -> same, for unmount / room change /
//                    capability loss
//
// It is free of React and DOM imports so the Node smoke
// test (`drawingSession.smoke.ts`) drives the same code the
// canvas handlers call.
//
// Capability handling: `canSend()` is consulted once per
// stroke at pointer down. When it is false the stroke is
// drawn locally exactly as before but nothing is sent (the
// server would refuse every envelope with ROOM_ERROR). A
// stroke that is already networked keeps going until it is
// ended; the owner calls `finish()` when DRAW is revoked.
//
// Errors: nothing in this class throws into a pointer
// handler. Send failures are reported through `onError`.

import type {
    BeginStrokeOptions,
    StrokeHandle,
    StrokePointPayload,
} from "./drawingSession.ts";
import type { StrokePoint, StrokeTool } from "./types.ts";

/** The local half: `DrawingCanvasHandle` satisfies this. */
export interface LocalStrokeSink {
    beginStroke: (opts?: { tool?: StrokeTool; color?: string; width?: number }) => string;
    appendPoint: (point: StrokePoint) => void;
    endStroke: (endedAt?: number) => void;
}

/** The network half: `DrawingService` satisfies this. */
export interface NetworkStrokeSink {
    beginStroke: (opts: BeginStrokeOptions) => Promise<StrokeHandle>;
    appendPoint: (point: StrokePointPayload) => void;
    endStroke: () => Promise<string | null>;
    /** Stop sending for the active stroke without DRAW_END. */
    cancelStroke: () => void;
}

export interface PointerSample {
    /** Normalized [0..1] canvas coordinates. */
    x: number;
    y: number;
    /** [0..1]; 0 means "not reported". */
    pressure: number;
    /** Wall-clock ms. */
    ts: number;
    pointerId?: number;
}

export interface StrokeStyle {
    tool: StrokeTool;
    color: string;
    width: number;
}

export interface PointerPipelineOptions {
    /** Whether the local user may currently draw on the
     *  wire (the DRAW capability). Read at pointer down. */
    canSend: () => boolean;
    onError?: (err: unknown) => void;
}

function clamp01(v: number): number {
    return v < 0 ? 0 : v > 1 ? 1 : v;
}

function isFiniteSample(s: PointerSample): boolean {
    return (
        Number.isFinite(s.x) &&
        Number.isFinite(s.y) &&
        Number.isFinite(s.pressure) &&
        Number.isFinite(s.ts)
    );
}

export class PointerStrokePipeline {
    private readonly local: LocalStrokeSink;
    private readonly network: NetworkStrokeSink;
    private readonly canSend: () => boolean;
    private readonly onError: (err: unknown) => void;

    private activeId: string | null = null;
    private activePointer: number | undefined;
    private networked = false;

    constructor(local: LocalStrokeSink, network: NetworkStrokeSink, opts: PointerPipelineOptions) {
        this.local = local;
        this.network = network;
        this.canSend = opts.canSend;
        this.onError =
            opts.onError ??
            ((err) => {
                // eslint-disable-next-line no-console
                console.warn("drawing pipeline error", err);
            });
    }

    /** `true` while a stroke is in progress. */
    get active(): boolean {
        return this.activeId !== null;
    }

    /** Id of the stroke in progress (local id == wire id). */
    get strokeId(): string | null {
        return this.activeId;
    }

    /** Pointer down: start a local stroke and DRAW_BEGIN. */
    down(style: StrokeStyle, sample: PointerSample): void {
        if (!isFiniteSample(sample)) return;
        if (this.activeId !== null) {
            // A second pointer went down mid-stroke: close
            // the first one cleanly before starting again.
            this.finish();
        }
        const x = clamp01(sample.x);
        const y = clamp01(sample.y);
        const pressure = clamp01(sample.pressure);
        const id = this.local.beginStroke({
            tool: style.tool,
            color: style.color,
            width: style.width,
        });
        this.activeId = id;
        this.activePointer = sample.pointerId;
        this.local.appendPoint({ x, y, pressure, ts: sample.ts });
        this.networked = false;
        let allowed = false;
        try {
            allowed = this.canSend();
        } catch (err) {
            this.onError(err);
        }
        if (!allowed) return;
        this.networked = true;
        try {
            this.network
                .beginStroke({
                    strokeId: id,
                    tool: style.tool,
                    color: style.color,
                    width: style.width,
                    x,
                    y,
                    pressure,
                    tsMs: sample.ts,
                })
                .catch(() => {
                    // Already reported by the session's onError.
                });
        } catch (err) {
            this.onError(err);
        }
    }

    /** Pointer move: extend the local stroke, coalesce a
     *  DRAW_POINT. Ignored when no stroke is active or the
     *  event belongs to a different pointer. */
    move(sample: PointerSample): void {
        if (this.activeId === null) return;
        if (!this.isActivePointer(sample)) return;
        if (!isFiniteSample(sample)) return;
        const x = clamp01(sample.x);
        const y = clamp01(sample.y);
        const pressure = clamp01(sample.pressure);
        this.local.appendPoint({ x, y, pressure, ts: sample.ts });
        if (this.networked) {
            try {
                this.network.appendPoint({ x, y, pressure, tsMs: sample.ts });
            } catch (err) {
                this.onError(err);
            }
        }
    }

    /** Pointer up: record the final position and end. */
    up(sample: PointerSample): void {
        if (this.activeId === null) return;
        if (!this.isActivePointer(sample)) return;
        this.move(sample);
        this.finish();
    }

    /** Pointer cancel / leave: end the stroke where it is. */
    cancel(sample?: PointerSample): void {
        if (this.activeId === null) return;
        if (sample !== undefined && !this.isActivePointer(sample)) return;
        this.finish();
    }

    /** End the active stroke unconditionally (unmount, room
     *  change, drawing mode left). Safe to call at any time.
     *
     *  With `{ sendEnd: false }` (the DRAW capability was
     *  revoked) the local stroke is closed but no DRAW_END
     *  is sent: the server would refuse it with ROOM_ERROR,
     *  which the room client treats as the end of the room. */
    finish(opts: { sendEnd?: boolean } = {}): void {
        if (this.activeId === null) return;
        const sendEnd = opts.sendEnd ?? true;
        const networked = this.networked;
        this.activeId = null;
        this.activePointer = undefined;
        this.networked = false;
        try {
            this.local.endStroke();
        } catch (err) {
            this.onError(err);
        }
        if (networked) {
            try {
                if (sendEnd) {
                    this.network.endStroke().catch(() => {
                        // Already reported by the session's onError.
                    });
                } else {
                    this.network.cancelStroke();
                }
            } catch (err) {
                this.onError(err);
            }
        }
    }

    private isActivePointer(sample: PointerSample): boolean {
        return (
            this.activePointer === undefined ||
            sample.pointerId === undefined ||
            sample.pointerId === this.activePointer
        );
    }
}
