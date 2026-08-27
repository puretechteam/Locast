# scripts/gen-protocol.ps1
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
#   pwsh ./scripts/gen-protocol.ps1
#
# Exit code 0 on success; non-zero on any failure.

$ErrorActionPreference = 'Stop'

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path (Join-Path $scriptDir '..')
Set-Location -LiteralPath $repoRoot

Write-Host 'gen-protocol: regenerating shared/protocol/ts/index.ts'
cargo test -p locast-protocol -- --ignored

if ($LASTEXITCODE -ne 0) {
    throw "gen-protocol: cargo test failed with exit code $LASTEXITCODE"
}

Write-Host 'gen-protocol: OK'
