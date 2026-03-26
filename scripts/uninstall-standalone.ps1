# Standalone uninstaller for budi on Windows.
# Usage: irm https://raw.githubusercontent.com/siropkin/budi/main/scripts/uninstall-standalone.ps1 | iex
$ErrorActionPreference = "Stop"

$BinDir = if ($env:BIN_DIR) { $env:BIN_DIR } else { Join-Path $env:LOCALAPPDATA "budi\bin" }

function Log($msg) { Write-Host "[budi-uninstall] $msg" }

# 1. Run `budi uninstall --yes` if available (removes hooks, statusline, config, data).
$budiExe = Join-Path $BinDir "budi.exe"
if (Test-Path $budiExe) {
    Log "Running budi uninstall to remove hooks, status line, and data..."
    try { & $budiExe uninstall --yes 2>$null } catch {}
}

# 2. Stop daemon processes.
Log "Stopping daemon..."
try {
    Get-Process -Name "budi-daemon" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Log "Stopped budi-daemon."
} catch {
    Log "No running daemon found."
}

# 3. Remove binaries.
foreach ($bin in @("budi.exe", "budi-daemon.exe", "budi-bench.exe")) {
    $target = Join-Path $BinDir $bin
    if (Test-Path $target) {
        Remove-Item $target -Force
        Log "Removed $target"
    }
}

# 4. Remove BIN_DIR if empty.
if ((Test-Path $BinDir) -and @(Get-ChildItem $BinDir).Count -eq 0) {
    Remove-Item $BinDir -Force
    Log "Removed empty directory $BinDir"
}

# Also remove parent budi dir if empty (e.g. %LOCALAPPDATA%\budi).
$parentDir = Split-Path $BinDir -Parent
if ($parentDir -and (Test-Path $parentDir) -and @(Get-ChildItem $parentDir).Count -eq 0) {
    Remove-Item $parentDir -Force
    Log "Removed empty directory $parentDir"
}

# 5. Remove BIN_DIR from user PATH.
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -like "*$BinDir*") {
    $newPath = ($userPath -split ";" | Where-Object { $_ -ne $BinDir }) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Log "Removed $BinDir from user PATH."
}

Log ""
Log "Uninstall complete."
Log "Restart your terminal for PATH changes to take effect."
