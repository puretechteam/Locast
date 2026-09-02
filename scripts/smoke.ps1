# scripts/smoke.ps1
#
# Developer-facing smoke test entry point for Locast (P3-T14).
#
# Builds the smoke test binary, runs the `#[ignore]`'d Rust test
# (which itself starts the real signaling server in-process and
# drives a HOST -> VIEWER download via two isolated client rigs),
# verifies the JSON result the test wrote, and tears everything
# down deterministically.
#
# Usage:
#   pwsh ./scripts/smoke.ps1
#
# Exit codes:
#   0  success
#   1  unexpected / unclassified failure
#   2  cargo build --tests failed
#   3  cargo test failed to launch or exited with a non-test error
#   4  smoke test terminated but no result.json was produced
#   5  result.json missing or malformed
#   6  result.json reports success=false
#   7  total wall-clock budget (120s) exceeded
#       (60s is the per-test-run roadmap budget; 120s includes
#       the cold cargo build step on first invocation)

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path (Join-Path $scriptDir '..')).Path
Set-Location -LiteralPath $repoRoot

$cyan   = "`e[36m"
$green  = "`e[32m"
$red    = "`e[31m"
$yellow = "`e[33m"
$reset  = "`e[0m"

$budgetSeconds = 120
$tempBase = [System.IO.Path]::GetTempPath()
$tempDir  = Join-Path $tempBase ("locast-smoke-{0}-{1:yyyyMMddHHmmss}" -f $PID, (Get-Date))
$buildLog  = Join-Path $tempDir 'build.log'
$testLog   = Join-Path $tempDir 'test.log'
$resultJson = Join-Path $tempDir 'result.json'

$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
$aborted = $false
$tempCleaned = $false
$summary = New-Object System.Collections.Generic.List[string]

function Write-Stage([int]$Num, [int]$Total, [string]$Message) {
    Write-Host "${cyan}[$Num/$Total] $Message${reset}"
}

function Write-Ok([string]$Message) {
    Write-Host "${green}[OK] $Message${reset}"
}

function Write-Err([string]$Message) {
    Write-Host "${red}[ERROR] $Message${reset}"
}

function Add-Summary([string]$Line) {
    $script:summary.Add($Line) | Out-Null
}

function Print-Summary {
    if ($summary.Count -eq 0) { return }
    Write-Host ''
    Write-Host "${cyan}Smoke summary${reset}"
    foreach ($line in $summary) {
        Write-Host "  $line"
    }
}

function Remove-TempDir {
    param([bool]$KeepResult)
    if ($script:tempCleaned) { return }
    $script:tempCleaned = $true
    if (-not (Test-Path -LiteralPath $tempDir)) { return }
    try {
        if ($KeepResult -and (Test-Path -LiteralPath $resultJson)) {
            $saved = Join-Path $repoRoot 'smoke-last-result.json'
            Copy-Item -LiteralPath $resultJson -Destination $saved -Force -ErrorAction SilentlyContinue
        }
        Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
    catch {
        # Best-effort cleanup; never raise from here.
    }
}

function On-Abort([int]$Code) {
    $script:aborted = $true
    Write-Err "aborted (exit $Code)"
    Add-Summary 'log dir: <smoke temp dir> (removed on cleanup)'
    Print-Summary
    Remove-TempDir -KeepResult $false
    [System.Environment]::Exit($Code)
}

# Ctrl+C handler: never leave the temp dir behind.
$null = [Console]::add_CancelKeyPress({
    On-Abort 130
})

try {
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    Add-Summary 'log dir: <smoke temp dir>'

    Write-Stage 1 5 'Building smoke test binary...'
    # Pipe cargo's noisy output to the build log so the
    # terminal stays clean. PowerShell's
    # Start-Process RedirectStandardOutput and
    # RedirectStandardError cannot share a single file,
    # so we redirect them separately to two streams and
    # merge them on the file. cargo's exit code is
    # propagated through Start-Process's ExitCode.
    $buildStdout = Join-Path $tempDir 'build.stdout.log'
    $buildStderr = Join-Path $tempDir 'build.stderr.log'
    '' | Set-Content -LiteralPath $buildLog -Force
    $buildProc = Start-Process `
        -FilePath 'cargo' `
        -ArgumentList @('build','--tests','-p','locast-client','-j','1') `
        -WorkingDirectory $repoRoot `
        -RedirectStandardOutput $buildStdout `
        -RedirectStandardError  $buildStderr `
        -NoNewWindow `
        -PassThru `
        -Wait
    Get-Content -LiteralPath $buildStdout, $buildStderr -Raw -ErrorAction SilentlyContinue |
        ForEach-Object { Add-Content -LiteralPath $buildLog -Value $_ }
    Remove-Item -LiteralPath $buildStdout, $buildStderr -Force -ErrorAction SilentlyContinue
    if ($buildProc.ExitCode -ne 0) {
        Write-Err "cargo build --tests failed (exit $($buildProc.ExitCode)). See $buildLog"
        Add-Summary 'build: FAILED'
        Print-Summary
        Remove-TempDir -KeepResult $true
        exit 2
    }
    Add-Summary 'build: ok'

    Write-Stage 2 5 'Running smoke test (cargo test -j 1)...'
    # The Rust test starts the real signaling server
    # in-process on an ephemeral port, drives a HOST ->
    # VIEWER download via two isolated client rigs, and
    # writes <SMOKE_OUTPUT_DIR>/result.json. Same
    # dual-stream merge pattern as the build step above.
    $env:SMOKE_OUTPUT_DIR = $tempDir
    $env:SMOKE_RESULT_PATH = $resultJson
    # Pass SMOKE_SERVER_LOG through to the Rust test so the
    # operator can override the in-process server's tracing
    # filter without re-running pnpm smoke with a tweaked shell.
    if (Test-Path Env:SMOKE_SERVER_LOG) {
        $env:LOCAST_LOG = $env:SMOKE_SERVER_LOG
    }
    $testStdout = Join-Path $tempDir 'test.stdout.log'
    $testStderr = Join-Path $tempDir 'test.stderr.log'
    '' | Set-Content -LiteralPath $testLog -Force
    $testProc = Start-Process `
        -FilePath 'cargo' `
        -ArgumentList @('test','-j','1','-p','locast-client','--test','smoke_host_viewer','--','--ignored','--nocapture') `
        -WorkingDirectory $repoRoot `
        -RedirectStandardOutput $testStdout `
        -RedirectStandardError  $testStderr `
        -NoNewWindow `
        -PassThru `
        -Wait
    Get-Content -LiteralPath $testStdout, $testStderr -Raw -ErrorAction SilentlyContinue |
        ForEach-Object { Add-Content -LiteralPath $testLog -Value $_ }
    Remove-Item -LiteralPath $testStdout, $testStderr -Force -ErrorAction SilentlyContinue
    Add-Summary ('test exit: {0}' -f $testProc.ExitCode)

    # If the test binary itself errored (Rust panic, compile error from a stale build, missing binary) AND no result.json was written, exit with 3 rather than continuing to the result.json check (which would also fail and report 4). This matches INTEGRATION.md section 17 exit-code 3.
    if ($testProc.ExitCode -ne 0 -and -not (Test-Path -LiteralPath $resultJson)) {
        Write-Err ('smoke test process failed (exit {0}) and wrote no result.json' -f $testProc.ExitCode)
        Add-Summary 'result: missing (test crashed)'
        Print-Summary
        Remove-TempDir -KeepResult $true
        exit 3
    }

    if ($stopwatch.Elapsed.TotalSeconds -gt $budgetSeconds) {
        Write-Err ("Smoke test exceeded {0}s budget" -f $budgetSeconds)
        Add-Summary ('elapsed: {0:N1}s (over budget)' -f $stopwatch.Elapsed.TotalSeconds)
        Print-Summary
        Remove-TempDir -KeepResult $true
        exit 7
    }

    Write-Stage 3 5 'Verifying result.json...'
    if (-not (Test-Path -LiteralPath $resultJson)) {
        Write-Err 'result.json not found at <smoke temp dir>/result.json'
        Add-Summary 'result: missing'
        Print-Summary
        Remove-TempDir -KeepResult $true
        exit 4
    }

    try {
        $result = Get-Content -LiteralPath $resultJson -Raw | ConvertFrom-Json -ErrorAction Stop
    }
    catch {
        Write-Err ("result.json is not valid JSON: {0}" -f $_.Exception.Message)
        Add-Summary 'result: malformed'
        Print-Summary
        Remove-TempDir -KeepResult $true
        exit 5
    }

    if (-not ($result.PSObject.Properties.Name -contains 'success')) {
        Write-Err 'result.json missing required "success" field'
        Add-Summary 'result: invalid schema'
        Print-Summary
        Remove-TempDir -KeepResult $true
        exit 5
    }

    Write-Stage 4 5 'Cleaning up smoke temp directory...'
    Remove-TempDir -KeepResult $true
    Add-Summary 'temp dir: cleaned (result kept at smoke-last-result.json)'

    Write-Stage 5 5 'Exiting...'
    if ($result.success) {
        $stages = if ($result.PSObject.Properties.Name -contains 'stages_passed') { $result.stages_passed } else { '?' }
        Write-Ok ("Smoke test passed in {0}s (stages_passed={1})" -f ([math]::Round($stopwatch.Elapsed.TotalSeconds,1)), $stages)
        Add-Summary ('status: success ({0}s)' -f [math]::Round($stopwatch.Elapsed.TotalSeconds,1))
        Print-Summary
        exit 0
    }
    else {
        $failStage = if ($result.PSObject.Properties.Name -contains 'failure_stage') { $result.failure_stage } else { 'unknown' }
        $failMsg   = if ($result.PSObject.Properties.Name -contains 'failure_message') { $result.failure_message } else { '(no message)' }
        Write-Err ("smoke test reported failure at stage '{0}': {1}" -f $failStage, $failMsg)
        Add-Summary ('status: failure at {0}' -f $failStage)
        Print-Summary
        exit 6
    }
}
catch {
    Write-Err ("unhandled exception: {0}" -f $_.Exception.Message)
    Add-Summary 'status: unhandled error'
    Print-Summary
    if (-not $tempCleaned) { Remove-TempDir -KeepResult $true }
    exit 1
}
finally {
    if (-not $tempCleaned) { Remove-TempDir -KeepResult $true }
    # Unset smoke env vars so they don't leak into subsequent
    # PowerShell sessions (the temp dir they point at has been
    # reaped and is invalid).
    Remove-Item Env:SMOKE_OUTPUT_DIR -ErrorAction SilentlyContinue
    Remove-Item Env:SMOKE_RESULT_PATH -ErrorAction SilentlyContinue
}