// P5-T01: pure-math smoke test for the drawing geometry
// helpers.
//
// Run via `pnpm -C apps/client smoke:geometry` (script
// declared in package.json). Pure Node + the
// `--experimental-strip-types` flag, mirroring the
// pattern set by `drift.smoke.ts` (P4-T04) and
// `dedup.smoke.ts` (P4-T07).

import {
    clientToCanvasRect,
    clientToNormalized,
    clamp01,
    normalizedToBacking,
    normalizeCanvasPoint,
} from "./geometry.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

process.stdout.write("geometry smoke\n");

// ----- clientToCanvasRect -----
process.stdout.write("clientToCanvasRect\n");
{
    const r = { left: 100, top: 50, width: 800, height: 600 };
    const p = clientToCanvasRect(150, 75, r);
    check("pointer at (150,75) - rect(100,50) = (50,25)", p.x === 50 && p.y === 25);
}
{
    const r = { left: 0, top: 0, width: 1000, height: 800 };
    const p = clientToCanvasRect(500, 400, r);
    check(
        "pointer inside rect: returns CSS-pixel coords",
        p.x === 500 && p.y === 400,
    );
}
{
    const r = { left: 200, top: 200, width: 400, height: 300 };
    const p = clientToCanvasRect(150, 150, r);
    check(
        "pointer outside rect: returns negative coords (not clamped)",
        p.x === -50 && p.y === -50,
    );
}

// ----- normalizeCanvasPoint -----
process.stdout.write("normalizeCanvasPoint\n");
{
    // 50% on each axis.
    const p = normalizeCanvasPoint(100, 150, 200, 300);
    check("half-x -> 0.5", p.x === 0.5);
    check("half-y -> 0.5", p.y === 0.5);
}
{
    const p = normalizeCanvasPoint(0, 0, 100, 100);
    check("origin -> (0,0)", p.x === 0 && p.y === 0);
}
{
    const p = normalizeCanvasPoint(100, 100, 100, 100);
    check("corner -> (1,1)", p.x === 1 && p.y === 1);
}
{
    const p = normalizeCanvasPoint(200, 200, 100, 100);
    check(
        "out-of-bounds clamps to 1",
        p.x === 1 && p.y === 1,
    );
}
{
    const p = normalizeCanvasPoint(-50, -50, 100, 100);
    check(
        "negative clamps to 0",
        p.x === 0 && p.y === 0,
    );
}
{
    // Degenerate canvas (zero width).
    const p = normalizeCanvasPoint(50, 50, 0, 0);
    check(
        "zero-dim canvas -> (0,0)",
        p.x === 0 && p.y === 0,
    );
}
{
    // Negative canvas dimensions collapse to 0.
    const p = normalizeCanvasPoint(50, 50, -10, -10);
    check(
        "negative-dim canvas -> (0,0)",
        p.x === 0 && p.y === 0,
    );
}

// ----- clientToNormalized (combined) -----
process.stdout.write("clientToNormalized\n");
{
    const r = { left: 100, top: 50, width: 200, height: 300 };
    // pointer (150, 50): x_inside = 50 -> 0.25, y_inside = 0 -> 0.
    const p = clientToNormalized(150, 50, r);
    check(
        "(150, 50) over rect(100,50,200,300) -> (0.25, 0)",
        p.x === 0.25 && p.y === 0,
    );
}
{
    const r = { left: 100, top: 50, width: 200, height: 300 };
    const p = clientToNormalized(300, 350, r);
    check(
        "bottom-right corner of rect -> (1,1)",
        p.x === 1 && p.y === 1,
    );
}
{
    const r = { left: 100, top: 50, width: 200, height: 300 };
    const p = clientToNormalized(200, 50, r);
    check(
        "center x of rect -> 0.5",
        p.x === 0.5,
    );
}

// ----- clamp01 (direct edge cases) -----
process.stdout.write("clamp01\n");
check("0 -> 0", clamp01(0) === 0);
check("1 -> 1", clamp01(1) === 1);
check("0.5 -> 0.5", clamp01(0.5) === 0.5);
check("-0.001 -> 0", clamp01(-0.001) === 0);
check("1.001 -> 1", clamp01(1.001) === 1);
check("NaN -> 0", clamp01(NaN) === 0);
check("Infinity -> 1", clamp01(Infinity) === 1);
check("-Infinity -> 0", clamp01(-Infinity) === 0);

// ----- normalizedToBacking -----
process.stdout.write("normalizedToBacking\n");
{
    // Intrinsic 1920x1080; normalized (0.5, 0.5) -> (959.5, 539.5).
    // The renderer contracts to (w-1) so the last
    // backing-store pixel is reachable.
    const p = normalizedToBacking({ x: 0.5, y: 0.5 }, 1920, 1080);
    check(
        "(0.5, 0.5) at 1920x1080 -> (959.5, 539.5)",
        p.x === 959.5 && p.y === 539.5,
    );
}
{
    // Normalized (1, 1) at 100x100 -> (99, 99) (the last
    // backing-store pixel, not 100).
    const p = normalizedToBacking({ x: 1, y: 1 }, 100, 100);
    check(
        "(1, 1) at 100x100 -> (99, 99)",
        p.x === 99 && p.y === 99,
    );
}
{
    // Normalized (0, 0) -> (0, 0).
    const p = normalizedToBacking({ x: 0, y: 0 }, 1920, 1080);
    check(
        "(0, 0) -> (0, 0)",
        p.x === 0 && p.y === 0,
    );
}
{
    // Degenerate intrinsic size collapses to 0.
    const p = normalizedToBacking({ x: 0.5, y: 0.5 }, 0, 0);
    check(
        "zero intrinsic size -> (0, 0)",
        p.x === 0 && p.y === 0,
    );
}

if (failures > 0) {
    process.stdout.write(`\n${failures} failure(s)\n`);
    process.exit(1);
} else {
    process.stdout.write("\nall checks passed\n");
}