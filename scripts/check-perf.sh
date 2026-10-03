#!/usr/bin/env bash
# scripts/check-perf.sh - run the criterion hot-path benches and compare them
# against the checked-in baseline (P8-T06, ARCHITECTURE 28.10 / 28.11).
#
#   scripts/check-perf.sh                    run benches, fail on >threshold regression
#   scripts/check-perf.sh --update           run benches, rewrite this platform's baseline
#   scripts/check-perf.sh --allow-missing-baseline
#                                            CI bootstrap: compare only if a baseline exists
#   scripts/check-perf.sh --out FILE         also write the measured medians to FILE
#
# Baselines are per platform (benches/baseline/<platform>.json); see
# scripts/perf-compare.mjs for the comparison rules. Runs a single cargo job
# so it never competes with another build.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Drop stale results so a removed bench cannot be compared against old output.
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
rm -rf "$target_dir/criterion"

# One --bench flag per target: a bare `cargo bench` would also run the
# crate's unit tests in bench mode.
cargo bench -p locast-client -j 1     --bench library_scan --bench library_list --bench manifest     --bench scheduler --bench reassembly --bench events

node scripts/perf-compare.mjs "$@"
