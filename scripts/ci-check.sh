#!/usr/bin/env bash
# scripts/ci-check.sh - run the same checks that .github/workflows/ci.yml
# runs on every PR. Used locally to verify CI parity before pushing.
#
# P0-T04 establishes the script. It exits non-zero on the first failing
# check; the caller's CI is "all green" only when this script exits 0.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
RESET='\033[0m'

step() {
    printf "\n${CYAN}==> %s${RESET}\n" "$1"
}

ok() {
    printf "${GREEN}    ok${RESET}\n"
}

fail() {
    printf "${RED}    FAIL: %s${RESET}\n" "$1" >&2
    exit 1
}

require() {
    if ! command -v "$1" >/dev/null 2>&1; then
        fail "required tool not on PATH: $1"
    fi
}

require cargo
require pnpm

step "cargo fmt --all -- --check"
if cargo fmt --all -- --check; then
    ok
else
    fail "cargo fmt --check found unformatted code"
fi

step "cargo clippy --workspace --all-targets -- -D warnings"
if cargo clippy --workspace --all-targets -- -D warnings; then
    ok
else
    fail "cargo clippy reported warnings or errors"
fi

step "cargo test --workspace"
if cargo test --workspace; then
    ok
else
    fail "cargo test --workspace had failing tests"
fi

step "pnpm install --frozen-lockfile"
if pnpm install --frozen-lockfile; then
    ok
else
    fail "pnpm install --frozen-lockfile failed"
fi

step "pnpm -r typecheck"
if pnpm -r typecheck; then
    ok
else
    fail "pnpm typecheck failed"
fi

step "pnpm -r lint"
if pnpm -r lint; then
    ok
else
    fail "pnpm lint failed"
fi

step "pnpm -r test"
if pnpm -r test; then
    ok
else
    fail "pnpm test failed"
fi

step "cargo build --workspace"
if cargo build --workspace; then
    ok
else
    fail "cargo build --workspace failed"
fi

step "pnpm build (apps/client)"
if (cd apps/client && pnpm build); then
    ok
else
    fail "pnpm build (apps/client) failed"
fi

printf "\n${GREEN}All CI checks passed.${RESET}\n"
