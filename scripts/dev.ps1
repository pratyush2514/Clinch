#Requires -Version 5.1
<#
.SYNOPSIS
    Fast Clinch dev loop for Windows: pull, set env, launch. No cargo clean.

.DESCRIPTION
    Replaces the manual ritual of git pull + $env:... + cargo clean + build.

    `cargo clean` is the slow part: it wipes the incremental cache and forces
    a full rebuild (minutes). It is NOT needed after a pull — `npm run tauri
    dev` runs `cargo run`, which rebuilds only what changed (usually seconds;
    "Finished dev profile in 0.85s" when nothing changed). Only use -Clean for
    a corrupted target/ directory or a Rust toolchain change.

    The Groq key is NEVER written to the repo. It is read from:
      1. $env:GROQ_API_KEY if already set in this shell, else
      2. a local .env.local file (repo root, gitignored) with lines like:
           GROQ_API_KEY=gsk_...
         '#' starts a comment. Create it once and stop pasting the key.

    The script guarantees you run the LATEST main: after fetching, it
    compares your checkout against origin/main and refuses to launch a
    stale tree. Use -Force when your local tree has diverged and you just
    want origin/main (discards local changes).

    If PowerShell blocks the script, run once as admin:
      Set-ExecutionPolicy -Scope CurrentUser RemoteSigned
    or launch bypassing the policy for this run only:
      powershell -ExecutionPolicy Bypass -File scripts\dev.ps1

.EXAMPLE
    .\scripts\dev.ps1            # pull latest main, env, launch (incremental)
    .\scripts\dev.ps1 -NoPull    # skip git sync (uncommitted local work)
    .\scripts\dev.ps1 -Force     # discard local changes, take origin/main
    .\scripts\dev.ps1 -Clean     # full rebuild (rare)
#>
param(
    [switch]$NoPull,
    [switch]$Force,
    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

$gitDir = Join-Path $RepoRoot ".git"
if (-not (Test-Path $gitDir)) {
    Write-Warning "No .git directory here — cannot pull or verify the commit. Clone the repo instead of using a zip download."
}
elseif (-not $NoPull) {
    Write-Host "Fetching latest main..." -ForegroundColor Cyan
    git fetch origin main
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "git fetch failed — cannot verify the latest commit. Launching with the current tree."
    }
    else {
        $remoteHash = (git rev-parse origin/main).Trim()
        $localHash = (git rev-parse HEAD).Trim()
        if ($localHash -ne $remoteHash) {
            if ($Force) {
                Write-Host "Force: discarding local changes, resetting to origin/main..." -ForegroundColor Yellow
                git reset --hard origin/main
                if ($LASTEXITCODE -ne 0) { Write-Warning "git reset failed — launching with the current tree." }
                $localHash = (git rev-parse HEAD).Trim()
            }
            else {
                Write-Host "Updating to latest main..." -ForegroundColor Cyan
                git pull --ff-only origin main
                if ($LASTEXITCODE -eq 0) {
                    $localHash = (git rev-parse HEAD).Trim()
                }
                else {
                    Write-Host ""
                    Write-Host "STOP: your tree is not on the latest main, so I won't launch a stale build." -ForegroundColor Red
                    Write-Host "  local:  $localHash"
                    Write-Host "  origin: $remoteHash"
                    Write-Host "Your tree has local changes or has diverged. Stash them, or re-run with -Force to discard local changes and take origin/main."
                    exit 1
                }
            }
        }
        if ($localHash -eq $remoteHash) {
            Write-Host "On latest main: $localHash" -ForegroundColor Green
        }
        else {
            Write-Warning "Still not on latest main (local $localHash vs origin $remoteHash) — launching anyway."
        }
    }
}

# Local secrets file: KEY=VALUE per line, '#' comments. Gitignored, and the
# push tooling excludes it, so it can never land on GitHub.
$envFile = Join-Path $RepoRoot ".env.local"
if ((-not $env:GROQ_API_KEY) -and (Test-Path $envFile)) {
    Get-Content $envFile | ForEach-Object {
        $line = $_.Trim()
        if ($line -and -not $line.StartsWith("#") -and ($line -match "^([^=]+)=(.*)$")) {
            $name = $Matches[1].Trim()
            $value = $Matches[2].Trim().Trim('"', "'")
            [Environment]::SetEnvironmentVariable($name, $value, "Process")
        }
    }
}

if (-not $env:CLINCH_CHROMIUM_PATH) {
    $chrome = @(
        "$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
        "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
        "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($chrome) { $env:CLINCH_CHROMIUM_PATH = $chrome }
}
if (-not $env:CLINCH_GROUNDER_PROVIDER) { $env:CLINCH_GROUNDER_PROVIDER = "groq" }

if (-not $env:GROQ_API_KEY) {
    Write-Warning "GROQ_API_KEY is not set: grounding will miss. Set it in this shell or in .env.local (never commit it)."
}

if ($Clean) {
    Write-Host "Cleaning target/ — full rebuild will take several minutes..." -ForegroundColor Yellow
    cargo clean
}

Write-Host "Launching Clinch (incremental build)..." -ForegroundColor Green
npm run tauri dev
