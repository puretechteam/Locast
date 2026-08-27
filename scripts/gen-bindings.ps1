# scripts/gen-bindings.ps1
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
#   pwsh ./scripts/gen-bindings.ps1
#
# Exit code 0 on success; non-zero on any failure.

$ErrorActionPreference = 'Stop'

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path (Join-Path $scriptDir '..')
Set-Location -LiteralPath $repoRoot

Write-Host 'gen-bindings: regenerating apps/client/src/bindings/index.ts'
cargo test `
    -p locast-client `
    --test gen_bindings `
    -- --ignored

if ($LASTEXITCODE -ne 0) {
    throw "gen-bindings: cargo test failed with exit code $LASTEXITCODE"
}

Write-Host 'gen-bindings: OK'
