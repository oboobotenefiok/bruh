# bruh install script for Windows (PowerShell 5.1+)
# Usage: irm https://raw.githubusercontent.com/oboobotenefiok/bruh/main/install.ps1 | iex

# It covers essential errors warnings and stuff like that. I won't comment much here.

$ErrorActionPreference = "Stop"
$Repo   = "https://github.com/oboobotenefiok/bruh"
$BinDir = "$env:LOCALAPPDATA\bruh\bin"

function Write-Step($msg) { Write-Host "  $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "  ✓ $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "  ○ $msg" -ForegroundColor Yellow }

Write-Host ""
Write-Host "  bruh — persistent developer memory" -ForegroundColor White
Write-Host "  $Repo" -ForegroundColor DarkGray
Write-Host ""

# ── Create bin dir ─────────────────────────────────────────────────────────────
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

# ── Download binary ────────────────────────────────────────────────────────────
$Arch    = if ([Environment]::Is64BitOperatingSystem) { "x86_64" } else { "x86" }
$BinUrl  = "$Repo/releases/latest/download/bruh-windows-$Arch.exe"
$BinPath = "$BinDir\bruh.exe"

Write-Step "Downloading bruh for Windows $Arch..."
try {
    Invoke-WebRequest -Uri $BinUrl -OutFile $BinPath -UseBasicParsing
    Write-Ok "Downloaded to $BinPath"
} catch {
    Write-Warn "No pre-built binary found. Building from source..."
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Host "  cargo not found. Install Rust from https://rustup.rs" -ForegroundColor Red
        exit 1
    }
    # I was thinking of automatically installing Rust for the user but I think that will be invasive.
    $TmpDir = [System.IO.Path]::GetTempPath() + "bruh_build"
    git clone --depth=1 $Repo $TmpDir 2>$null
    Push-Location $TmpDir
    cargo build --release --quiet
    Copy-Item "target\release\bruh.exe" $BinPath
    Pop-Location
    Remove-Item -Recurse -Force $TmpDir
    Write-Ok "Built from source"
}

# ── Add to PATH ────────────────────────────────────────────────────────────────
$CurrentPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($CurrentPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$CurrentPath;$BinDir", "User")
    $env:Path += ";$BinDir"
    Write-Ok "Added $BinDir to PATH"
}

Write-Host ""
Write-Ok "bruh installed!"
Write-Host ""

# ── Run init ───────────────────────────────────────────────────────────────────
$Run = Read-Host "  Run bruh init now? [Y/n]"
if ($Run -eq "" -or $Run -match "^[Yy]") {
    & $BinPath init
}

Write-Host ""
