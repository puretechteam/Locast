#!/usr/bin/env bash
# P9-T03: CycloneDX SBOM for the Locast signaling server.
#
#   apps/server/scripts/sbom.sh <output-dir>
#
# Writes <output-dir>/locast-server.sbom.json (the Rust dependency graph of
# the locast-server crate, via cargo-cyclonedx) and validates it against the
# CycloneDX 1.5 schema; exits non-zero if generation or validation fails.
#
# Requires (pinned to match .github/workflows/release.yml):
#   cargo-cyclonedx 0.5.9, cyclonedx-cli 0.33.1 on PATH as `cyclonedx`
#   (or set CYCLONEDX_CLI to its path).

set -euo pipefail

root="$(cd "$(dirname "$0")/../../.." && pwd)"
out="${1:?usage: apps/server/scripts/sbom.sh <output-dir>}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
cli="${CYCLONEDX_CLI:-cyclonedx}"
tmp_name="locast-sbom-tmp.cdx"

cleanup() {
    # cargo-cyclonedx writes one file next to every workspace member's
    # manifest; only the server's is kept.
    local member
    for member in $(sed -n '/^members = \[/,/^\]/p' "$root/Cargo.toml" | grep -o '"[^"]*"' | tr -d '"'); do
        rm -f "$root/$member/$tmp_name.json"
    done
}
trap cleanup EXIT

(cd "$root" && cargo cyclonedx --manifest-path apps/server/Cargo.toml \
    --format json --spec-version 1.5 --target all --override-filename "$tmp_name" --quiet)
mv "$root/apps/server/$tmp_name.json" "$out/locast-server.sbom.json"

"$cli" validate --input-file "$out/locast-server.sbom.json" \
    --input-format json --input-version v1_5 --fail-on-errors

echo "wrote $out/locast-server.sbom.json (valid CycloneDX 1.5)"
