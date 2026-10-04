// P5-T01: drawing data model.
//
// Shapes mirror `docs/ARCHITECTURE.md` §15.4 wire format
// (normalized [0..1] float coordinates, per stroke:
// tool, color, width, point ring). They are deliberately
// kept network-friendly: the P5-T02 transport serializes
// these straight to the wire without conversion.
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

/** Fill `n` bytes from the platform CSPRNG, falling back
 *  to `Math.random` when `crypto.getRandomValues` is not
 *  available (very old webviews). Ids are not security
 *  tokens (the server binds them to the signed sender), so
 *  the fallback only needs to avoid collisions. */
function randomBytes(n: number): Uint8Array {
    const out = new Uint8Array(n);
    const c = (
        globalThis as { crypto?: { getRandomValues?: (a: Uint8Array) => Uint8Array } }
    ).crypto;
    if (c !== undefined && typeof c.getRandomValues === "function") {
        c.getRandomValues(out);
        return out;
    }
    for (let i = 0; i < n; i++) {
        out[i] = Math.floor(Math.random() * 256);
    }
    return out;
}

/** Generate a stroke id: a canonical (lowercase,
 *  hyphenated) UUID v7 per architecture section 15.4. This
 *  is the ONE id scheme for strokes: the same string is the
 *  local store/renderer id and the `stroke_id` on the wire
 *  (`drawing_send` parses it with `Uuid::parse_str` and the
 *  server deserializes it as a `Uuid`).
 *
 *  Layout: 48-bit big-endian unix-ms timestamp, version
 *  nibble 7, 12 random bits, variant 10, 62 random bits. */
export function newStrokeId(nowMs: number = Date.now()): string {
    const b = randomBytes(16);
    // Timestamps are far below 2^48, so the arithmetic
    // below is exact in a double.
    let ts = Math.max(0, Math.floor(nowMs));
    for (let i = 5; i >= 0; i--) {
        b[i] = ts % 256;
        ts = Math.floor(ts / 256);
    }
    b[6] = ((b[6] ?? 0) & 0x0f) | 0x70;
    b[8] = ((b[8] ?? 0) & 0x3f) | 0x80;
    let hex = "";
    for (const byte of b) {
        hex += byte.toString(16).padStart(2, "0");
    }
    return (
        `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-` +
        `${hex.slice(16, 20)}-${hex.slice(20, 32)}`
    );
}

const CANONICAL_UUID =
    /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/** `true` when `id` is a canonical lowercase hyphenated
 *  RFC 4122 UUID, i.e. exactly the shape the Rust
 *  `drawing_send` command accepts and echoes back. */
export function isCanonicalStrokeId(id: string): boolean {
    return CANONICAL_UUID.test(id);
}