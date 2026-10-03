// scripts/perf-compare.mjs - compare criterion results against the checked-in
// baseline (P8-T06). Called by scripts/check-perf.sh after `cargo bench`.
//
//   node scripts/perf-compare.mjs                 compare; exit 1 on regression
//   node scripts/perf-compare.mjs --update        rewrite this platform's baseline
//   node scripts/perf-compare.mjs --out FILE      also write current medians to FILE
//   node scripts/perf-compare.mjs --allow-missing-baseline
//                                                 exit 0 when no baseline exists yet
//
// Baselines live in apps/client/src-tauri/benches/baseline/<platform>.json
// (platform = process.platform: linux, darwin, win32). Absolute timings only
// compare meaningfully on the same class of machine, so every platform has its
// own file. Compared value: the criterion median, in nanoseconds per iteration.
//
// A bench regresses when median > baseline * (1 + threshold_pct / 100).
// Improvements never fail. A bench present in the results but not in the
// baseline (or vice versa) also fails, so adding or removing a bench forces a
// deliberate `--update`.

import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const targetDir = process.env.CARGO_TARGET_DIR
  ? resolve(process.env.CARGO_TARGET_DIR)
  : join(repoRoot, "target");
const criterionDir = join(targetDir, "criterion");
const baselineDir = join(repoRoot, "apps", "client", "src-tauri", "benches", "baseline");
const baselinePath = join(baselineDir, `${process.platform}.json`);

// ARCHITECTURE 28.11 sets the gate at 10 percent, and that is the default.
// Measured run-to-run noise on a developer machine showed two classes of bench
// that cannot hold 10 percent without flaking, so they carry a wider
// per-bench threshold in the baseline file (still checked, never off):
//   * filesystem / SQLite bound benches (swing of tens of percent between runs)
//   * the scheduler decision micro-bench (about 2 us per MiB; ~15 percent swing)
const DEFAULT_THRESHOLD_PCT = 10;
const IO_BOUND_THRESHOLD_PCT = 50;
const IO_BOUND_GROUPS = new Set(["library_scan", "library_list", "reassembly"]);
const PER_BENCH_THRESHOLD_PCT = { "scheduler/schedule_256x1mib_4_peers": 25 };

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const outIndex = args.indexOf("--out");
const outPath = outIndex >= 0 ? resolve(args[outIndex + 1] ?? "") : null;

/** Walk target/criterion and return { "group/bench": median_ns }. */
function readResults() {
  const results = {};
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const full = join(dir, entry.name);
      if (entry.name === "new" && existsSync(join(full, "estimates.json"))) {
        const meta = JSON.parse(readFileSync(join(full, "benchmark.json"), "utf8"));
        const est = JSON.parse(readFileSync(join(full, "estimates.json"), "utf8"));
        results[meta.full_id] = est.median.point_estimate;
      } else if (entry.name !== "report" && entry.name !== "base" && entry.name !== "change") {
        walk(full);
      }
    }
  };
  if (!existsSync(criterionDir)) {
    console.error(`no criterion output at ${criterionDir}; run cargo bench first`);
    process.exit(2);
  }
  walk(criterionDir);
  if (Object.keys(results).length === 0) {
    console.error("criterion output contains no benchmarks");
    process.exit(2);
  }
  return results;
}

const fmt = (ns) =>
  ns >= 1e9 ? `${(ns / 1e9).toFixed(3)} s`
    : ns >= 1e6 ? `${(ns / 1e6).toFixed(3)} ms`
    : ns >= 1e3 ? `${(ns / 1e3).toFixed(3)} us`
    : `${ns.toFixed(1)} ns`;

const results = readResults();

if (outPath) {
  mkdirSync(dirname(outPath), { recursive: true });
  writeFileSync(outPath, JSON.stringify({ platform: process.platform, medians_ns: results }, null, 2) + "\n");
}

if (flag("--update")) {
  const benches = {};
  for (const id of Object.keys(results).sort()) {
    const group = id.split("/")[0];
    const threshold_pct =
      PER_BENCH_THRESHOLD_PCT[id] ??
      (IO_BOUND_GROUPS.has(group) ? IO_BOUND_THRESHOLD_PCT : DEFAULT_THRESHOLD_PCT);
    benches[id] = { median_ns: Math.round(results[id]), threshold_pct };
  }
  mkdirSync(baselineDir, { recursive: true });
  writeFileSync(baselinePath, JSON.stringify({ platform: process.platform, benches }, null, 2) + "\n");
  console.log(`wrote ${baselinePath} (${Object.keys(benches).length} benches)`);
  process.exit(0);
}

if (!existsSync(baselinePath)) {
  const msg = `no perf baseline for platform '${process.platform}' at ${baselinePath}`;
  if (flag("--allow-missing-baseline")) {
    console.log(`${msg}; skipping comparison (run with --update to record one)`);
    process.exit(0);
  }
  console.error(`${msg}; run scripts/check-perf.sh --update on this platform`);
  process.exit(1);
}

const baseline = JSON.parse(readFileSync(baselinePath, "utf8")).benches;
let failed = false;

for (const id of Object.keys(results).sort()) {
  const base = baseline[id];
  if (!base) {
    console.error(`FAIL ${id}: no baseline entry (run --update to record it)`);
    failed = true;
    continue;
  }
  const pct = ((results[id] - base.median_ns) / base.median_ns) * 100;
  const limit = base.threshold_pct ?? DEFAULT_THRESHOLD_PCT;
  const regressed = pct > limit;
  const sign = pct >= 0 ? "+" : "";
  console.log(
    `${regressed ? "FAIL" : "ok  "} ${id}: ${fmt(results[id])} vs ${fmt(base.median_ns)} ` +
      `(${sign}${pct.toFixed(1)}%, limit +${limit}%)`,
  );
  if (regressed) failed = true;
}
for (const id of Object.keys(baseline)) {
  if (!(id in results)) {
    console.error(`FAIL ${id}: in baseline but not produced by cargo bench`);
    failed = true;
  }
}

process.exit(failed ? 1 : 0);
