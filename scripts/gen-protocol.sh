#!/usr/bin/env bash
# scripts/gen-protocol.sh
#
# Regenerate the Locast shared protocol TypeScript bindings from the
# `HelloWorld` example struct (and any future envelope types) declared in
# `shared/protocol/src/lib.rs`.
#
# Per P0-T07 and section 26.4 of docs/ARCHITECTURE.md, the generated
# `shared/protocol/ts/index.ts` is checked in and CI verifies that running
# this script is a no-op against the committed file.
#
# Usage:
#   ./scripts/gen-protocol.sh
#
# Exit code 0 on success; non-zero on any failure.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

echo "gen-protocol: regenerating shared/protocol/ts/index.ts"
cargo test -p locast-protocol -- --ignored

echo "gen-protocol: OK"
