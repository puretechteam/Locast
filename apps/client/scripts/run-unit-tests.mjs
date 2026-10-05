// Runs every `*.smoke.ts` under src/ as a unit test.
//
// The client's pure-logic tests are plain TypeScript programs that assert
// with a local `check()` helper and exit non-zero on failure. They are run
// with Node's built-in type stripping, so no test framework or bundler is
// needed. Files are discovered by glob (not listed by hand) so a new
// `*.smoke.ts` is picked up automatically and cannot be forgotten by CI.
//
// Requires Node 22.6+ (`--experimental-strip-types`).

import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { dirname, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const clientRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const srcRoot = join(clientRoot, "src");

const [major = 0, minor = 0] = process.versions.node.split(".").map(Number);
if (major < 22 || (major === 22 && minor < 6)) {
    process.stderr.write(
        `unit tests need Node 22.6+ for --experimental-strip-types (running ${process.version})\n`,
    );
    process.exit(2);
}

function findSmokeFiles(dir) {
    const out = [];
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
        const full = join(dir, entry.name);
        if (entry.isDirectory()) {
            out.push(...findSmokeFiles(full));
        } else if (entry.name.endsWith(".smoke.ts")) {
            out.push(full);
        }
    }
    return out;
}

const files = findSmokeFiles(srcRoot).sort();
if (files.length === 0) {
    process.stderr.write("no *.smoke.ts files found under src/ - refusing to report success\n");
    process.exit(2);
}

const failed = [];
for (const file of files) {
    const name = relative(clientRoot, file).split(sep).join("/");
    process.stdout.write(`\n=== ${name}\n`);
    const result = spawnSync(
        process.execPath,
        ["--experimental-strip-types", "--no-warnings", file],
        { cwd: clientRoot, stdio: "inherit" },
    );
    if (result.status !== 0) {
        failed.push(`${name} (${result.status ?? result.signal})`);
    }
}

process.stdout.write(`\n${files.length - failed.length}/${files.length} unit test files passed\n`);
if (failed.length > 0) {
    process.stdout.write(`FAILED:\n${failed.map((f) => `  ${f}`).join("\n")}\n`);
    process.exit(1);
}
