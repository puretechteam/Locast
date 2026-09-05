// P5-T01: pure smoke test for the stroke renderer.
//
// Run via `pnpm -C apps/client smoke:stroke-renderer`
// (script declared in package.json). Uses a mock
// `Canvas2DStateful` that records every call, so the
// test does not need a real DOM canvas.

import { renderStrokes, type Canvas2DStateful } from "./strokeRenderer.ts";
import type { Stroke, StrokePoint } from "./types.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

interface CallLog {
    method: string;
    args: unknown[];
}

function makeMockCtx(): {
    ctx: Canvas2DStateful;
    log: CallLog[];
} {
    const log: CallLog[] = [];
    const ctx: Canvas2DStateful = {
        clearRect: (...args) => {
            log.push({ method: "clearRect", args });
        },
        beginPath: () => {
            log.push({ method: "beginPath", args: [] });
        },
        moveTo: (x, y) => {
            log.push({ method: "moveTo", args: [x, y] });
        },
        lineTo: (x, y) => {
            log.push({ method: "lineTo", args: [x, y] });
        },
        stroke: () => {
            log.push({ method: "stroke", args: [] });
        },
        strokeStyle: "#000",
        lineWidth: 1,
        lineCap: "butt",
        lineJoin: "miter",
    };
    return { ctx, log };
}

function makePoint(x: number, y: number): StrokePoint {
    return { x, y, pressure: 0, ts: 0 };
}

function makeStroke(opts: {
    id: string;
    points: StrokePoint[];
    color?: string;
    width?: number;
}): Stroke {
    return {
        id: opts.id,
        userId: "u1",
        tool: "pen",
        color: opts.color ?? "#e6e6e6",
        width: opts.width ?? 3,
        points: opts.points,
        startedAt: 0,
        endedAt: 0,
    };
}

const INTRINSIC = { width: 100, height: 100 };
const CSS_W = 100;

process.stdout.write("stroke renderer smoke\n");

// ----- empty history clears the canvas -----
process.stdout.write("empty history\n");
{
    const { ctx, log } = makeMockCtx();
    const count = renderStrokes(ctx, [], INTRINSIC, CSS_W);
    check("0 lineTo calls on empty strokes", count === 0);
    check(
        "calls clearRect on empty history",
        log.length === 1 && log[0]?.method === "clearRect",
    );
}

// ----- single pen stroke (3 points) -> 2 lineTo -----
process.stdout.write("single pen stroke\n");
{
    const { ctx, log } = makeMockCtx();
    const strokes: Stroke[] = [
        makeStroke({
            id: "s1",
            points: [makePoint(0.1, 0.1), makePoint(0.5, 0.5), makePoint(0.9, 0.9)],
        }),
    ];
    const count = renderStrokes(ctx, strokes, INTRINSIC, CSS_W);
    check("3-point stroke -> 2 lineTo calls", count === 2);
    // The renderer should call: clearRect, beginPath,
    // moveTo(p0), lineTo(p1), lineTo(p2), stroke.
    check(
        "first call is clearRect",
        log[0]?.method === "clearRect",
    );
    check(
        "second call is beginPath",
        log[1]?.method === "beginPath",
    );
    check(
        "third call is moveTo (first point)",
        log[2]?.method === "moveTo",
    );
    check(
        "subsequent calls are lineTo",
        log[3]?.method === "lineTo" && log[4]?.method === "lineTo",
    );
    check(
        "last call is stroke",
        log[5]?.method === "stroke",
    );
}

// ----- single-point stroke -> 1 moveTo, 0 lineTo -----
process.stdout.write("single-point stroke\n");
{
    const { ctx, log } = makeMockCtx();
    const strokes: Stroke[] = [
        makeStroke({ id: "s1", points: [makePoint(0.5, 0.5)] }),
    ];
    const count = renderStrokes(ctx, strokes, INTRINSIC, CSS_W);
    check("1-point stroke -> 0 lineTo calls", count === 0);
    const lineTos = log.filter((c) => c.method === "lineTo");
    check(
        "no lineTo calls",
        lineTos.length === 0,
    );
    const moveTos = log.filter((c) => c.method === "moveTo");
    check("1 moveTo for the first (and only) point", moveTos.length === 1);
}

// ----- two strokes -> 2 stroke calls, 4 lineTo -----
process.stdout.write("two strokes\n");
{
    const { ctx, log } = makeMockCtx();
    const strokes: Stroke[] = [
        makeStroke({
            id: "s1",
            points: [makePoint(0, 0), makePoint(0.5, 0.5)],
        }),
        makeStroke({
            id: "s2",
            points: [makePoint(0, 0.5), makePoint(0.5, 0.5), makePoint(1, 0.5)],
        }),
    ];
    const count = renderStrokes(ctx, strokes, INTRINSIC, CSS_W);
    check("two strokes -> 3 lineTo calls", count === 3);
    const strokes$ = log.filter((c) => c.method === "stroke");
    check("two ctx.stroke() calls (one per stroke)", strokes$.length === 2);
}

// ----- intrinsic-size 0 -> no work -----
process.stdout.write("degenerate intrinsic size\n");
{
    const { ctx, log } = makeMockCtx();
    const strokes: Stroke[] = [
        makeStroke({
            id: "s1",
            points: [makePoint(0.5, 0.5), makePoint(0.6, 0.6)],
        }),
    ];
    const count = renderStrokes(ctx, strokes, { width: 0, height: 0 }, CSS_W);
    check("0 lineTo when intrinsic size is 0", count === 0);
    const lineTos = log.filter((c) => c.method === "lineTo");
    check("no lineTo calls", lineTos.length === 0);
}

// ----- stroke width scales with intrinsic / cssWidth ratio -----
process.stdout.write("stroke width scaling\n");
{
    const ctx = makeMockCtx().ctx;
    const strokes: Stroke[] = [
        makeStroke({
            id: "s1",
            points: [makePoint(0.5, 0.5), makePoint(0.6, 0.6)],
            width: 4,
        }),
    ];
    // Intrinsic 1920 wide, CSS 960 wide -> scale = 2.
    // The renderer should set lineWidth = 4 * 2 = 8.
    renderStrokes(ctx, strokes, { width: 1920, height: 1080 }, 960);
    // We assert the assignment at the time of stroke();
    // the mock records property reads but not writes.
    // The captured `lineWidth` at the moment of stroke()
    // is 8 (the renderer's last assignment).
    check(
        "lineWidth at stroke() == 8 (4 CSS px * 2x scale)",
        ctx.lineWidth === 8,
    );
}

// ----- backing-store coords match the intrinsic resolution -----
process.stdout.write("backing-store coords\n");
{
    const { ctx, log } = makeMockCtx();
    const strokes: Stroke[] = [
        makeStroke({
            id: "s1",
            points: [makePoint(0.5, 0.5), makePoint(1, 1)],
        }),
    ];
    renderStrokes(ctx, strokes, { width: 100, height: 100 }, 100);
    // moveTo(49.5, 49.5) (0.5 * 99 = 49.5) and
    // lineTo(99, 99) (1 * 99 = 99).
    const moveTo = log.find((c) => c.method === "moveTo");
    const lineTo = log.find((c) => c.method === "lineTo");
    check(
        "moveTo(49.5, 49.5)",
        moveTo?.args[0] === 49.5 && moveTo?.args[1] === 49.5,
    );
    check(
        "lineTo(99, 99)",
        lineTo?.args[0] === 99 && lineTo?.args[1] === 99,
    );
}

// ----- non-pen strokes are skipped -----
process.stdout.write("non-pen strokes skipped\n");
{
    const { ctx, log } = makeMockCtx();
    const rectStroke: Stroke = {
        ...makeStroke({
            id: "r1",
            points: [makePoint(0, 0), makePoint(0.5, 0.5)],
        }),
        tool: "rect",
    };
    const eraserStroke: Stroke = {
        ...makeStroke({
            id: "e1",
            points: [makePoint(0, 0), makePoint(0.5, 0.5)],
        }),
        tool: "eraser",
    };
    const strokes: Stroke[] = [rectStroke, eraserStroke];
    const count = renderStrokes(ctx, strokes, INTRINSIC, CSS_W);
    check("non-pen strokes produce 0 lineTo", count === 0);
    const lineTos = log.filter((c) => c.method === "lineTo");
    check("no lineTo calls (no pen strokes)", lineTos.length === 0);
}

if (failures > 0) {
    process.stdout.write(`\n${failures} failure(s)\n`);
    process.exit(1);
} else {
    process.stdout.write("\nall checks passed\n");
}