// P8-T07: flag every Long Task above the threshold in the trace the perf
// scenario captured (perf-results/long-tasks.json).
//
//   node tests/perf/flag-long-tasks.mjs [--file PATH] [--threshold MS] [--strict]
//
// Default: prints a table, emits a GitHub `::warning::` annotation per flagged
// task, and exits 0 (ARCHITECTURE 28.11: long-task regressions are
// "investigated", not an automatic failure). `--strict` exits 1 if any task is
// flagged. Exit 2 means the input is missing or malformed.

import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const valueOf = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] !== undefined ? args[i + 1] : fallback;
};
const file = valueOf(
  "--file",
  fileURLToPath(new URL("../../perf-results/long-tasks.json", import.meta.url)),
);
const threshold = Number(valueOf("--threshold", "50"));
const strict = args.includes("--strict");

if (!Number.isFinite(threshold) || threshold <= 0) {
  console.error(`invalid --threshold: ${valueOf("--threshold", "")}`);
  process.exit(2);
}
if (!existsSync(file)) {
  console.error(`no long-task data at ${file}; run the perf scenario first (pnpm test:perf)`);
  process.exit(2);
}

let tasks;
try {
  tasks = JSON.parse(readFileSync(file, "utf8")).tasks;
} catch (err) {
  console.error(`could not parse ${file}: ${err.message}`);
  process.exit(2);
}
if (!Array.isArray(tasks)) {
  console.error(`${file} has no "tasks" array`);
  process.exit(2);
}

const flagged = tasks
  .filter((t) => typeof t.duration === "number" && t.duration > threshold)
  .sort((a, b) => a.startTime - b.startTime);

console.log(`long tasks observed: ${tasks.length}; above ${threshold} ms: ${flagged.length}`);
for (const t of flagged) {
  const msg = `long task ${t.duration.toFixed(1)} ms at t=${(t.startTime / 1000).toFixed(2)} s`;
  console.log(`  FLAG ${msg}`);
  if (process.env["GITHUB_ACTIONS"] === "true") console.log(`::warning title=Long task::${msg}`);
}
if (flagged.length > 0) {
  const worst = Math.max(...flagged.map((t) => t.duration));
  console.log(`worst: ${worst.toFixed(1)} ms`);
}

process.exit(strict && flagged.length > 0 ? 1 : 0);
