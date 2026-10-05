#!/usr/bin/env bash
# Assert that a release tag names the version the installers will carry.
# Run by the `preflight` job of .github/workflows/release.yml on tag pushes.
#
#   scripts/check-release-version.sh v1.2.3
#
# The installers take their version from apps/client/src-tauri/tauri.conf.json
# (and the client crate and package.json repeat it). Nothing else ties a tag to
# that number, so a `v1.0.0` tag on a tree that still says 0.0.0 would publish
# installers versioned 0.0.0 under release v1.0.0. This only compares; it never
# changes a version. Requires: bash, grep, sed.

set -euo pipefail
cd "$(dirname "$0")/.."

tag="${1:-}"
if ! [[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
    echo "FAIL tag '$tag' is not of the form v<major>.<minor>.<patch>"
    exit 1
fi
want="${BASH_REMATCH[1]}"

failed=0
check() {
    local where="$1" got="$2"
    if [ "$got" = "$want" ]; then
        echo "ok   $where is $got"
    else
        echo "FAIL $where is '$got', but tag $tag needs $want"
        failed=1
    fi
}

# The first "version" key in each JSON file is the top-level one.
json_version() {
    grep -m 1 '"version"' "$1" | sed 's/.*"version": *"\([^"]*\)".*/\1/'
}

check apps/client/src-tauri/tauri.conf.json "$(json_version apps/client/src-tauri/tauri.conf.json)"
check apps/client/package.json "$(json_version apps/client/package.json)"
# First `version = "..."` line is the [package] version.
check apps/client/src-tauri/Cargo.toml \
    "$(sed -n 's/^version = "\(.*\)"$/\1/p' apps/client/src-tauri/Cargo.toml | head -n 1)"

exit "$failed"
