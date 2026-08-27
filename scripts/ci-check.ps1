# scripts/ci-check.ps1 - run the same checks that .github/workflows/ci.yml
# runs on every PR. Used locally to verify CI parity before pushing.
#
# P0-T04 establishes the script. It stops at the first failing check;
# CI is "all green" only when this script exits 0.

[CmdletBinding()]
param(
    [switch]$SkipPnpmBuild
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $repoRoot

$cyan  = "`e[36m"
$green = "`e[32m"
$red   = "`e[31m"
$reset = "`e[0m"

function Step([string]$Message) {
    Write-Host ""
    Write-Host "${cyan}==> $Message${reset}"
}

function Ok {
    Write-Host "${green}    ok${reset}"
}

function Fail([string]$Message) {
    Write-Host "${red}    FAIL: $Message${reset}" -ForegroundColor Red
    exit 1
}

function Require([string]$Tool) {
    if (-not (Get-Command $Tool -ErrorAction SilentlyContinue)) {
        Fail "required tool not on PATH: $Tool"
    }
}

Require cargo
Require pnpm

Step "cargo fmt --all -- --check"
cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { Fail "cargo fmt --check found unformatted code" }
Ok

Step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { Fail "cargo clippy reported warnings or errors" }
Ok

Step "cargo test --workspace"
cargo test --workspace
if ($LASTEXITCODE -ne 0) { Fail "cargo test --workspace had failing tests" }
Ok

Step "pnpm install --frozen-lockfile"
pnpm install --frozen-lockfile
if ($LASTEXITCODE -ne 0) { Fail "pnpm install --frozen-lockfile failed" }
Ok

Step "pnpm -r typecheck"
pnpm -r typecheck
if ($LASTEXITCODE -ne 0) { Fail "pnpm typecheck failed" }
Ok

Step "pnpm -r lint"
pnpm -r lint
if ($LASTEXITCODE -ne 0) { Fail "pnpm lint failed" }
Ok

Step "pnpm -r test"
pnpm -r test
if ($LASTEXITCODE -ne 0) { Fail "pnpm test failed" }
Ok

Step "cargo build --workspace"
cargo build --workspace
if ($LASTEXITCODE -ne 0) { Fail "cargo build --workspace failed" }
Ok

if (-not $SkipPnpmBuild) {
    Step "pnpm build (apps/client)"
    Push-Location (Join-Path $repoRoot "apps/client")
    try {
        pnpm build
        if ($LASTEXITCODE -ne 0) { Fail "pnpm build (apps/client) failed" }
    }
    finally {
        Pop-Location
    }
    Ok
}

Write-Host ""
Write-Host "${green}All CI checks passed.${reset}"
