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

    If PowerShell blocks the script, run once as admin:
      Set-ExecutionPolicy -Scope CurrentUser RemoteSigned
    or launch bypassing the policy for this run only:
      powershell -ExecutionPolicy Bypass -File scripts\dev.ps1

.EXAMPLE
    .\scripts\dev.ps1            # pull, env, launch (incremental)
    .\scripts\dev.ps1 -NoPull    # skip git pull (uncommitted local work)
    .\scripts\dev.ps1 -Clean     # full rebuild (rare)
#>
param(
    [switch]$NoPull,
    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

if (-not $NoPull -and (Test-Path (Join-Path $RepoRoot ".git"))) {
    Write-Host "Pulling latest main..." -ForegroundColor Cyan
    git pull --ff-only origin main
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "git pull failed (local changes?). Launching with the current tree."
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
