// P5-T01: drawing data model.
//
// Shapes mirror `docs/ARCHITECTURE.md` §15.4 wire format
// (normalized [0..1] float coordinates, per stroke:
// tool, color, width, point ring). They are deliberately
// kept network-friendly: a future P5-T02 transport can
// serialize these straight to JSON without conversion.
//
// This module is pure (no React, no DOM); it can be
// imported by the smoke test, the renderer, the hook,
// and (eventually) the transport layer.

/** Drawing tools recognized by the canvas. Mirrors
 *  `docs/ARCHITECTURE.md` §15.3. */
export type StrokeTool =
    | "pen"
    | "arrow"
    | "rect"
    | "circle"
    | "text"
    | "eraser";

/** A single point on a stroke, normalized to the
 *  canvas dimensions ([0..1] per axis). Matches the
 *  `stroke_point` payload in §15.4. */
export interface StrokePoint {
    /** Normalized x coordinate, [0..1]. */
    x: number;
    /** Normalized y coordinate, [0..1]. */
    y: number;
    /** Optional pressure, [0..1]. `0` means "no pressure
     *  reported" (the renderer should treat it as a
     *  uniform width stroke). */
    pressure: number;
    /** Local wall-clock timestamp, ms. */
    ts: number;
}

/** A single stroke. `id` matches the §15.4 `stroke_begin`
 *  payload's `id` so future transport code can correlate
 *  events without changing the client-side shape. */
export interface Stroke {
    /** UUID v7 (per §15.4). */
    id: string;
    /** Originating user_id. Used by future renderer code
     *  to color local vs remote strokes differently
     *  (§15.2 "The canvas has pointer-events: auto
     *  for the local user and pointer-events: none for
     *  remote strokes"). The local renderer can also use
     *  this to deduplicate its own network echo. */
    userId: string;
    /** Tool that produced this stroke. */
    tool: StrokeTool;
    /** CSS color string (e.g. "#ff5c69"). Default:
     *  "#e6e6e6" (matches the room.css body color). */
    color: string;
    /** Stroke width in CSS pixels. The renderer scales
     *  by intrinsic/display ratio so a stroke at
     *  intrinsic resolution has the same visual width on
     *  any display size. */
    width: number;
    /** The stroke's points, in arrival order. v1 stores
     *  every captured pointer event; future
     *  optimizations (ring buffer + LTTB downsampling)
     *  belong to a later task. */
    points: StrokePoint[];
    /** Local wall-clock ms when `pointerdown` fired. */
    startedAt: number;
    /** Local wall-clock ms when `pointerup` fired. `0`
     *  while the stroke is still in progress (the
     *  renderer should treat `endedAt === 0` as "live
     *  stroke, draw as you go"). */
    endedAt: number;
}

/** Helper: produce a fresh `Stroke` with sane defaults
 *  for the local user. `points` starts empty and the
 *  caller appends via the hook's `appendPoint`. */
export function newStroke(opts: {
    id: string;
    userId: string;
    tool: StrokeTool;
    color: string;
    width: number;
    startedAt: number;
}): Stroke {
    return {
        id: opts.id,
        userId: opts.userId,
        tool: opts.tool,
        color: opts.color,
        width: opts.width,
        points: [],
        startedAt: opts.startedAt,
        endedAt: 0,
    };
}

/** Generate a UUID v7-ish id without depending on the
 *  `crypto` global so the smoke test can run in plain
 *  Node without `--experimental-global-crypto`. The id
 *  shape matches the wire format (`xxxxxxxx-xxxx-7xxx
 *  -xxxx-xxxxxxxxxxxx`) but the random suffix is a
 *  simple `Math.random` fallback; the transport layer
 *  will replace this with a real UUID v7 when P5-T02
 *  ships. Deterministic-where-possible for the smoke
 *  test means the canvas accepts any non-empty unique
 *  string. */
export function makeStrokeId(prefix: string): string {
    const rnd = Math.floor(Math.random() * 0xffffffff)
        .toString(16)
        .padStart(8, "0");
    const ts = Date.now().toString(16).padStart(12, "0");
    return `${prefix}-${ts}-7${rnd.slice(0, 3)}-${rnd.slice(3, 7)}`;
}