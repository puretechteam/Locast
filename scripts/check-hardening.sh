#!/usr/bin/env bash
# P8-T08: assert the hardening checklist in docs/ARCHITECTURE.md section
# 21.17. Run by the `hardening` CI job.
#
#   scripts/check-hardening.sh static        Dockerfile, compose, lints, CI
#   scripts/check-hardening.sh image <tag>   the built server image
#
# The cargo-deny item is asserted by the job's `needs: supply-chain`.
# Requires: bash, grep, awk, sed; `image` and the compose check also need
# docker and jq.

set -euo pipefail
cd "$(dirname "$0")/.."

HARDENING_RUSTFLAGS='-C target-feature=+crt-static -C link-arg=-fstack-protector-strong'
DOCKERFILE=apps/server/Dockerfile
failed=0

ok() { echo "ok   $*"; }
bad() {
    echo "FAIL $*"
    failed=1
}

is_root_user() {
    case "$1" in
    "" | root | root:* | 0 | 0:*) return 0 ;;
    *) return 1 ;;
    esac
}

static_checks() {
    # Hardening RUSTFLAGS, applied to the server binary via an explicit target.
    if grep -qF "ENV RUSTFLAGS=\"$HARDENING_RUSTFLAGS\"" "$DOCKERFILE"; then
        ok "Dockerfile sets RUSTFLAGS=\"$HARDENING_RUSTFLAGS\""
    else
        bad "Dockerfile does not set the section 21.17 RUSTFLAGS"
    fi
    if grep -qE 'cargo build --release -p locast-server --target ' "$DOCKERFILE"; then
        ok "Dockerfile builds the server with an explicit --target"
    else
        bad "Dockerfile server build lacks an explicit --target"
    fi

    # Non-root: the last USER in the final stage.
    local user
    user=$(awk 'toupper($1) == "FROM" { u = "" } toupper($1) == "USER" { u = $2 } END { print u }' "$DOCKERFILE")
    if is_root_user "$user"; then
        bad "final Dockerfile stage runs as '${user:-root (no USER)}'"
    else
        ok "final Dockerfile stage runs as non-root user '$user'"
    fi

    # cgroup limits: every service in every compose file has 2 GB RAM and 1 CPU.
    local files f
    files=$(git ls-files '*compose*.yml' '*compose*.yaml')
    if [ -z "$files" ]; then
        bad "no compose file found"
    fi
    for f in $files; do
        local cfg missing
        # Production compose files require deployment values; these
        # placeholders exist only to render the config and are never used.
        cfg=$(LOCAST_DOMAIN=render.invalid LOCAST_TURN_SHARED_SECRET=render-only \
            docker compose -f "$f" config --format json)
        missing=$(echo "$cfg" | jq -r '.services | to_entries[]
            | select((.value.mem_limit | tostring) != "2147483648" or (.value.cpus | tostring) != "1")
            | .key')
        if [ -n "$missing" ]; then
            bad "$f: services without mem_limit 2g / cpus 1: $(echo "$missing" | tr '\n' ' ')"
        else
            ok "$f: every service has mem_limit 2g and cpus 1"
        fi
    done

    # No `unsafe`: the workspace lint is on and every member inherits it.
    if sed -n '/^\[workspace\.lints\.rust\]/,/^\[/p' Cargo.toml | grep -qE '^unsafe_code = "(deny|forbid)"'; then
        ok "workspace lint unsafe_code is deny/forbid"
    else
        bad "Cargo.toml lacks [workspace.lints.rust] unsafe_code = \"deny\""
    fi
    local member
    for member in $(sed -n '/^members = \[/,/^\]/p' Cargo.toml | grep -o '"[^"]*"' | tr -d '"'); do
        if sed -n '/^\[lints\]/,/^\[/p' "$member/Cargo.toml" | grep -qE '^workspace = true'; then
            ok "$member inherits the workspace lints"
        else
            bad "$member/Cargo.toml lacks [lints] workspace = true"
        fi
    done

    # Warnings are CI failures, and clippy (which enforces the lint) runs.
    if grep -qE '^\s+RUSTFLAGS: -D warnings' .github/workflows/ci.yml \
        && grep -qF 'cargo clippy --workspace --all-targets -- -D warnings' .github/workflows/ci.yml; then
        ok "CI treats warnings as errors and runs clippy -D warnings"
    else
        bad "CI does not treat warnings as errors in build and clippy"
    fi
}

image_checks() {
    local tag="$1" user uid cid bin kind
    user=$(docker image inspect -f '{{.Config.User}}' "$tag")
    if is_root_user "$user"; then
        bad "image user is '${user:-root (unset)}'"
    else
        ok "image user is '$user'"
    fi
    uid=$(docker run --rm --entrypoint id "$tag" -u)
    if [ "$uid" = "0" ]; then
        bad "container process runs as uid 0"
    else
        ok "container process runs as uid $uid"
    fi

    # `+crt-static` yields a statically linked binary; check the shipped one.
    bin=$(mktemp)
    cid=$(docker create "$tag")
    docker cp "$cid:/app/locast-server" "$bin" >/dev/null
    docker rm "$cid" >/dev/null
    kind=$(file -b "$bin")
    rm -f "$bin"
    if echo "$kind" | grep -qE 'statically linked|static-pie linked'; then
        ok "server binary is statically linked ($kind)"
    else
        bad "server binary is not statically linked: $kind"
    fi

    # The hardened binary must still start and serve /health.
    local name=locast-hardening-smoke healthy=0
    docker run -d --rm --name "$name" -p 127.0.0.1:18787:8787 "$tag" >/dev/null
    for _ in $(seq 1 30); do
        if curl -fsS http://127.0.0.1:18787/health >/dev/null 2>&1; then
            healthy=1
            break
        fi
        sleep 1
    done
    docker rm -f "$name" >/dev/null 2>&1 || true
    if [ "$healthy" = 1 ]; then
        ok "hardened image starts and serves /health"
    else
        bad "hardened image did not serve /health within 30 s"
    fi
}

case "${1:-}" in
static) static_checks ;;
image)
    [ -n "${2:-}" ] || {
        echo "usage: $0 image <tag>"
        exit 2
    }
    image_checks "$2"
    ;;
*)
    echo "usage: $0 static | image <tag>"
    exit 2
    ;;
esac

if [ "$failed" != 0 ]; then
    echo "hardening checklist: FAILED"
    exit 1
fi
echo "hardening checklist: passed"
