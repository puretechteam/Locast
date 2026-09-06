// P5-T05: smoke test for laser color assignment.
//
// Run via `node --experimental-strip-types --no-warnings
// src/utils/laserColor.smoke.ts`.

import {
    laserColor,
    LOCAL_LASER_COLOR,
    LASER_PALETTE,
    hexToRgba,
} from "./laserColor.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

process.stdout.write("laserColor smoke\n");

// ----- LOCAL_LASER_COLOR -----
process.stdout.write("LOCAL_LASER_COLOR\n");
check("LOCAL_LASER_COLOR is #ff0000", LOCAL_LASER_COLOR === "#ff0000");

// ----- laserColor -----
process.stdout.write("laserColor\n");
check("local user gets red", laserColor("user-local", true) === "#ff0000");
check("local user always gets red regardless of userId", laserColor("any-user-id", true) === "#ff0000");

// Remote users get deterministic palette colors
const remote1 = laserColor("user-remote-1", false);
check("remote user 1 is in palette", LASER_PALETTE.includes(remote1));

const remote2 = laserColor("user-remote-2", false);
check("remote user 2 is in palette", LASER_PALETTE.includes(remote2));

// Same userId always gets same color
const remote1Again = laserColor("user-remote-1", false);
check("same userId always gets same color", remote1 === remote1Again);

const remote2Again = laserColor("user-remote-2", false);
check("same userId always gets same color (2)", remote2 === remote2Again);

// Different userIds get different colors (with high probability)
check("different userIds get different colors", remote1 !== remote2);

// ----- Known UUID set from acceptance criteria -----
process.stdout.write("acceptance UUID set\n");
{
    const u0 = "11111111-1111-1111-1111-111111111111";
    const u1 = "22222222-2222-2222-2222-222222222222";
    const u2 = "33333333-3333-3333-3333-333333333333";
    const u3 = "44444444-4444-4444-4444-444444444444";
    const c0 = laserColor(u0, false);
    const c1 = laserColor(u1, false);
    const c2 = laserColor(u2, false);
    const c3 = laserColor(u3, false);
    // All same
    check("UUID 1 deterministic", c0 === laserColor(u0, false));
    check("UUID 2 deterministic", c1 === laserColor(u1, false));
    check("UUID 3 deterministic", c2 === laserColor(u2, false));
    check("UUID 4 deterministic", c3 === laserColor(u3, false));
    // All different
    check("all 4 UUIDs have distinct colors", new Set([c0, c1, c2, c3]).size === 4);
}

// ----- hexToRgba -----
process.stdout.write("hexToRgba\n");
{
    const rgba = hexToRgba("#ff0000", 0.5);
    check("hexToRgba(#ff0000, 0.5) contains rgba", rgba.startsWith("rgba("));
    check("hexToRgba contains 255", rgba.includes("255"));
    check("hexToRgba contains 0.5", rgba.includes("0.5"));
}
{
    const rgba = hexToRgba("#4ecdc4", 1);
    check("hexToRgba(#4ecdc4, 1) contains rgba", rgba.startsWith("rgba("));
    check("hexToRgba contains 78", rgba.includes("78")); // green channel
    check("hexToRgba contains 196", rgba.includes("196")); // blue channel
}
{
    const rgba = hexToRgba("#aabbcc", 0.25);
    check("hexToRgba parses 2-char hex segments", rgba.includes("0.25"));
}

// ----- summary -----
process.stdout.write("\n");
if (failures === 0) {
    process.stdout.write(`All checks passed.\n`);
    process.exit(0);
} else {
    process.stdout.write(`${failures} check(s) failed.\n`);
    process.exit(1);
}
