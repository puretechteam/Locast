// P5-T01: pure stroke renderer.
//
// The renderer takes the full stroke history and paints
// it onto a `CanvasRenderingContext2D`-like context. It
// is intentionally pure: the only side effect is the
// mutation of the provided `ctx`. The smoke test passes a
// mock context that records every method call so the
// test can assert the correct drawing sequence without
// needing a real `<canvas>`.
//
// Coordinate convention (mirrors §15.4 + §15.2):
// - The canvas backing store is sized to the video's
//   intrinsic dimensions (`intrinsicSize.width` x
//   `intrinsicSize.height`).
// - The stroke history's points are in [0..1] normalized
//   coordinates; the renderer multiplies by the
//   intrinsic dimensions before issuing `lineTo`.
// - The stroke's `color` and `width` come from the
//   stroke record; the renderer scales `width` by
//   `(intrinsicWidth / canvasCssWidth)` so a stroke
//   rendered at the canonical CSS size looks the same
//   at any display size. (The CSS size is needed only
//   because the `width` field is recorded in CSS
//   pixels at capture time; a future revision may store
//   the width in normalized units instead.)
//
// Eraser semantics (§15.5) are NOT implemented here:
// that requires the canvas to be split into a
// "background strokes" layer + an "eraser" layer with
// `globalCompositeOperation = 'destination-out'`. P5-T01
// establishes the pipeline + the pen renderer; the
// eraser is out of scope.

import type { Stroke, StrokePoint } from "./types.ts";
import { normalizedToBacking } from "./geometry.ts";

/** The minimal slice of `CanvasRenderingContext2D` the
 *  renderer needs. Defined here so the smoke test can
 *  supply a mock without depending on DOM types.
 *
 *  `strokeStyle` accepts the full DOM union
 *  (`string | CanvasGradient | CanvasPattern`) so a real
 *  `CanvasRenderingContext2D` can be passed through
 *  unchanged; the renderer only ever assigns a `string`
 *  color from a stroke record, but the property is
 *  read by external code that may have set it to a
 *  gradient earlier. */
export interface Canvas2DLike {
    /** Reset every pixel to transparent black. */
    clearRect: (x: number, y: number, w: number, h: number) => void;
    /** Begin a new sub-path. */
    beginPath: () => void;
    /** Add a line segment to the current sub-path. */
    moveTo: (x: number, y: number) => void;
    lineTo: (x: number, y: number) => void;
    /** Stroke the current sub-path with the current
     *  `strokeStyle` / `lineWidth` / `lineCap` /
     *  `lineJoin` settings. */
    stroke: () => void;
    /** Set stroke style. */
    readonly strokeStyle: string | CanvasGradient | CanvasPattern;
    readonly lineWidth: number;
    readonly lineCap: CanvasLineCap;
    readonly lineJoin: CanvasLineJoin;
}

/** The "set" operations the renderer needs on top of
 *  `Canvas2DLike`. `CanvasRenderingContext2D` exposes
 *  these as settable properties; we type them
 *  explicitly here so the smoke test can mutate a
 *  mock and so the renderer can request a particular
 *  style without coupling to the full DOM type. */
export interface Canvas2DStateful extends Canvas2DLike {
    strokeStyle: string | CanvasGradient | CanvasPattern;
    lineWidth: number;
    lineCap: CanvasLineCap;
    lineJoin: CanvasLineJoin;
}

/** P5-T01 only paints pen strokes. Other tools are
 *  future work; the renderer ignores them silently so
 *  the foundation accepts the v1 wire format without
 *  crashing. */
function isPen(s: Stroke): boolean {
    return s.tool === "pen";
}

/** P5-T01 only paints a stroke once it has at least two
 *  points (a `stroke_end` arriving with a single point
 *  renders as a dot per §15.7; that's a future revision).
 *  Live strokes (`endedAt === 0`) are painted with
 *  their current point count so the user sees their
 *  pen trail in real time. */
function isRenderable(s: Stroke): boolean {
    return isPen(s) && s.points.length >= 1;
}

/** Paint the full stroke history onto the provided
 *  context. Clears the canvas first so a removed /
 *  undone stroke disappears immediately. Pure with
 *  respect to the stroke history; no I/O.
 *
 *  `intrinsicSize` is the canvas's backing-store pixel
 *  dimensions (NOT the CSS display size). The renderer
 *  always paints in backing-store units.
 *
 *  `canvasCssWidth` is the CSS-pixel width used to
 *  scale the stroke width. P5-T01 records width in CSS
 *  pixels at capture time so the value is comparable
 *  across displays.
 *
 *  Returns the number of `lineTo` calls issued (the
 *  smoke test uses this to assert "stroke X produced
 *  exactly N segments"). */
export function renderStrokes(
    ctx: Canvas2DStateful,
    strokes: readonly Stroke[],
    intrinsicSize: { width: number; height: number },
    canvasCssWidth: number,
): number {
    // 1. Clear the canvas (the rendering surface is
    //    re-drawn from scratch on every relevant state
    //    change per §15.2). Clearing is in backing-store
    //    pixels.
    ctx.clearRect(0, 0, intrinsicSize.width, intrinsicSize.height);
    if (strokes.length === 0) {
        return 0;
    }
    if (intrinsicSize.width <= 0 || intrinsicSize.height <= 0) {
        return 0;
    }

    // 2. Stroke width scale: a width recorded at CSS
    //    size X must produce a backing-store width of
    //    (width * intrinsicWidth / canvasCssWidth).
    //    Guarded against a degenerate CSS size. Only the
    //    width axis is needed for the stroke-width scale;
    //    the height axis is used implicitly by the
    //    `normalizedToBacking` call below.
    const cssW = canvasCssWidth > 0 ? canvasCssWidth : intrinsicSize.width;
    const widthScale = intrinsicSize.width / cssW;

    let lineToCount = 0;
    for (const stroke of strokes) {
        if (!isRenderable(stroke)) continue;
        ctx.strokeStyle = stroke.color;
        ctx.lineWidth = stroke.width * widthScale;
        ctx.lineJoin = "round";
        ctx.lineCap = "round";
        ctx.beginPath();
        for (let i = 0; i < stroke.points.length; i++) {
            const p: StrokePoint = stroke.points[i] as StrokePoint;
            const backing = normalizedToBacking(
                { x: p.x, y: p.y },
                intrinsicSize.width,
                intrinsicSize.height,
            );
            if (i === 0) {
                ctx.moveTo(backing.x, backing.y);
            } else {
                ctx.lineTo(backing.x, backing.y);
                lineToCount++;
            }
        }
        ctx.stroke();
    }
    return lineToCount;
}