// P5-T01: pure coordinate-transform math for the
// drawing canvas.
//
// Two pure functions:
//
// - `clientToCanvasRect`: maps a viewport `PointerEvent`
//   to a position INSIDE the canvas's bounding rect
//   (CSS pixels). This is the input to a downstream
//   `normalize` step that converts CSS pixels to the
//   [0..1] wire coordinate convention from §15.4.
//
// - `normalizeCanvasPoint`: maps a CSS-pixel position
//   inside the canvas to the [0..1] wire coordinate,
//   clamped so an out-of-bounds pointer cannot produce
//   an off-canvas stroke point.
//
// All functions are pure: no DOM, no React, no Date.
// The smoke test exercises every branch.
//
// Design notes:
// - Pointer events report `clientX/clientY` in viewport
//   pixels (CSS px). `getBoundingClientRect()` reports
//   the canvas element's position in viewport pixels.
//   Subtraction yields the position INSIDE the canvas.
// - `width` / `height` here are the canvas's CSS-pixel
//   display dimensions (NOT the canvas backing-store
//   pixel count, which may differ if `devicePixelRatio`
//   scaling is in play). The renderer multiplies by
//   `intrinsicWidth / width` to convert the normalized
//   position back to backing-store pixels before drawing.
// - The contract is robust against degenerate inputs:
//   zero / negative canvas dimensions, zero-area rects,
//   and out-of-bounds pointers all produce sane output
//   (0 or 1 in the normalized case).

/** Convert viewport `clientX/clientY` to a CSS-pixel
 *  position inside `rect`. `rect` is what
 *  `canvas.getBoundingClientRect()` would return.
 *
 *  Returns the X/Y pair as a `Point`. The result is
 *  NOT clamped: a pointer outside the canvas returns
 *  a negative number (or a number greater than the
 *  canvas CSS width). Callers that need clamped
 *  values should pipe the result through
 *  `normalizeCanvasPoint`.
 */
export interface Point {
    x: number;
    y: number;
}

export interface RectLike {
    left: number;
    top: number;
    width: number;
    height: number;
}

export function clientToCanvasRect(
    clientX: number,
    clientY: number,
    rect: RectLike,
): Point {
    return {
        x: clientX - rect.left,
        y: clientY - rect.top,
    };
}

/** Convert a CSS-pixel position INSIDE a canvas of the
 *  given CSS dimensions to the [0..1] normalized
 *  coordinate convention from §15.4.
 *
 *  The output is clamped to [0..1] on both axes so a
 *  stray pointer event at the edge of the canvas (or
 *  just outside it) cannot pollute the stroke history
 *  with off-canvas points.
 *
 *  Degenerate inputs (zero width or height) are
 *  treated as "no canvas" and produce 0. Negative
 *  CSS dimensions are coerced to 0 first.
 */
export function normalizeCanvasPoint(
    canvasX: number,
    canvasY: number,
    canvasCssWidth: number,
    canvasCssHeight: number,
): Point {
    const w = canvasCssWidth > 0 ? canvasCssWidth : 0;
    const h = canvasCssHeight > 0 ? canvasCssHeight : 0;
    return {
        x: w === 0 ? 0 : clamp01(canvasX / w),
        y: h === 0 ? 0 : clamp01(canvasY / h),
    };
}

/** Convenience: viewport `clientX/clientY` -> normalized
 *  [0..1] in a single call. Returns null when the
 *  canvas has no visible area (degenerate dimensions).
 *  The return is NOT clamped above; pass through
 *  `normalizeCanvasPoint` directly if the caller
 *  already has CSS-pixel coordinates. */
export function clientToNormalized(
    clientX: number,
    clientY: number,
    rect: RectLike,
): Point {
    const p = clientToCanvasRect(clientX, clientY, rect);
    return normalizeCanvasPoint(p.x, p.y, rect.width, rect.height);
}

/** Internal: clamp a finite number to [0, 1]. NaN
 *  collapses to 0 (a sensible "no coordinate" default);
 *  +/-Infinity clamp to the finite endpoints (1 and 0
 *  respectively) so a stray calculation cannot crash
 *  the renderer. */
export function clamp01(n: number): number {
    if (Number.isNaN(n)) return 0;
    if (n <= 0) return 0;
    if (n >= 1) return 1;
    return n;
}

/** Compute the canvas backing-store pixel coordinates
 *  for a normalized [0..1] position, given the canvas
 *  intrinsic dimensions. This is the inverse of the
 *  `clientToNormalized` chain and is what the renderer
 *  calls before issuing a `ctx.lineTo`.
 *
 *  The renderer multiplies a normalized `x` by
 *  `intrinsicWidth - 1` (not `intrinsicWidth`) so a
 *  point at x=1 lands on the last backing-store pixel
 *  (and never past the end, which would silently clip
 *  in some Canvas2D implementations). */
export function normalizedToBacking(
    norm: Point,
    intrinsicWidth: number,
    intrinsicHeight: number,
): Point {
    const w = intrinsicWidth > 0 ? intrinsicWidth : 0;
    const h = intrinsicHeight > 0 ? intrinsicHeight : 0;
    return {
        x: w === 0 ? 0 : norm.x * (w - 1),
        y: h === 0 ? 0 : norm.y * (h - 1),
    };
}