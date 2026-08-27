#!/usr/bin/env bash
# scripts/gen-bindings.sh
#
# Regenerate the Locast desktop client's TypeScript IPC bindings from
# the Rust commands and events declared in
# `apps/client/src-tauri/src/commands/` and `apps/client/src-tauri/src/events.rs`.
#
# Per P0-T06 and section 26.4 of docs/ARCHITECTURE.md, the generated
# `apps/client/src/bindings/index.ts` is checked in and CI verifies
# that running this script is a no-op against the committed file.
#
# Usage:
#   ./scripts/gen-bindings.sh
#
# Exit code 0 on success; non-zero on any failure.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

echo "gen-bindings: regenerating apps/client/src/bindings/index.ts"
cargo test \
    -p locast-client \
    --test gen_bindings \
    -- --ignored

echo "gen-bindings: OK"
