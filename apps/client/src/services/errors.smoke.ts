// Unit test for `errorText`: the readable message for a rejected IPC call.
//
// Rust's `AppError` reaches the webview as an object tagged with `kind`, not
// as an `Error`. Pages used `err instanceof Error ? err.message : String(err)`,
// which rendered "[object Object]" for every failed join, create, send and
// leave.
//
// Run by `pnpm test` (scripts/run-unit-tests.mjs), which picks up every
// `*.smoke.ts` under src/.

import { errorText } from "./errors.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

check("an Error gives its message", errorText(new Error("boom")) === "boom");
check("a string is returned as is", errorText("plain text") === "plain text");
check(
    "an AppError with a message gives the message",
    errorText({ kind: "Other", message: "room not found" }) === "room not found",
);
check(
    "an AppError without a message gives its kind, not [object Object]",
    errorText({ kind: "SourceMissing", path: "C:\\x.mp4" }) === "SourceMissing",
);
check(
    "an empty message falls back to the kind",
    errorText({ kind: "Other", message: "" }) === "Other",
);
check(
    "an object with neither is stringified rather than throwing",
    errorText({ code: 7 }) === "[object Object]",
);
check("null is stringified", errorText(null) === "null");
check("undefined is stringified", errorText(undefined) === "undefined");
check("a number is stringified", errorText(42) === "42");

if (failures > 0) {
    process.stdout.write(`\n${failures} check(s) failed\n`);
    process.exit(1);
}
process.stdout.write("\nAll checks passed.\n");
