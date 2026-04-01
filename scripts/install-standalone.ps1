# Standalone installer for budi on Windows.
# Usage: irm https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.ps1 | iex
$ErrorActionPreference = "Stop"

$Repo = "siropkin/budi"
$BinDir = if ($env:BIN_DIR) { $env:BIN_DIR } else { Join-Path $env:LOCALAPPDATA "budi\bin" }
$Version = $env:VERSION

function Log($msg) { Write-Host "[budi-install] $msg" }
function Fail($msg) { Write-Error "[budi-install] ERROR: $msg"; exit 1 }

# Resolve version tag.
if ($Version) {
    $tag = $Version
} else {
    Log "Fetching latest release tag..."
    $release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
    $tag = $release.tag_name
    if (-not $tag) { Fail "Could not determine latest release" }
}

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($arch -eq [System.Runtime.InteropServices.Architecture]::Arm64) {
    Log "ARM64 detected — using x86_64 build (runs via Windows x86 emulation)"
}
$target = "x86_64-pc-windows-msvc"
$assetName = "budi-$tag-$target.zip"
$baseUrl = "https://github.com/$Repo/releases/download/$tag"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) "budi-install-$(Get-Random)"
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

try {
    Log "Downloading $assetName ($tag)..."
    $zipPath = Join-Path $tempDir $assetName
    Invoke-WebRequest -Uri "$baseUrl/$assetName" -OutFile $zipPath -UseBasicParsing

    # Verify checksum (required by default).
    try {
        $sumsPath = Join-Path $tempDir "SHA256SUMS"
        Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing
        $expectedLine = Get-Content $sumsPath | Where-Object {
            ($_ -split '\s+')[-1].TrimStart('*') -eq $assetName
        } | Select-Object -First 1
        if (-not $expectedLine) { Fail "Checksum for $assetName not found in SHA256SUMS" }

        $expected = (($expectedLine -split '\s+')[0]).ToLower()
        $actual = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) { Fail "Checksum mismatch for $assetName" }
        Log "Checksum verified."
    } catch {
        if ($env:BUDI_ALLOW_INSECURE_NO_CHECKSUM -eq "1") {
            Log "WARNING: checksum file unavailable - continuing due to BUDI_ALLOW_INSECURE_NO_CHECKSUM=1."
        } else {
            Fail "Checksum file unavailable. Refusing insecure install. Set BUDI_ALLOW_INSECURE_NO_CHECKSUM=1 to override."
        }
    }

    Log "Extracting..."
    $extractDir = Join-Path $tempDir "extracted"
    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
    $pkgDir = Join-Path $extractDir "budi-$tag-$target"
    if (-not (Test-Path $pkgDir)) { Fail "Unexpected archive layout" }

    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    $bins = @("budi.exe", "budi-daemon.exe")
    $backupDir = Join-Path $tempDir "backup"
    New-Item -ItemType Directory -Path $backupDir -Force | Out-Null

    foreach ($bin in $bins) {
        $src = Join-Path $pkgDir $bin
        if (-not (Test-Path $src)) { Fail "Missing binary in release archive: $bin" }

        $dst = Join-Path $BinDir $bin
        if (Test-Path $dst) {
            Copy-Item $dst (Join-Path $backupDir "$bin.bak") -Force
        }
    }

    try {
        foreach ($bin in $bins) {
            $src = Join-Path $pkgDir $bin
            $dst = Join-Path $BinDir $bin
            $staged = Join-Path $BinDir ".$bin.new.$PID"
            Copy-Item $src $staged -Force
            if (Test-Path $dst) { Remove-Item $dst -Force }
            Move-Item $staged $dst -Force
            Log "Installed $bin -> $BinDir\$bin"
        }
    } catch {
        foreach ($bin in $bins) {
            $dst = Join-Path $BinDir $bin
            $bak = Join-Path $backupDir "$bin.bak"
            if (Test-Path $dst) { Remove-Item $dst -Force -ErrorAction SilentlyContinue }
            if (Test-Path $bak) { Copy-Item $bak $dst -Force }
        }
        throw
    }

    # Verify.
    $budiExe = Join-Path $BinDir "budi.exe"
    $daemonExe = Join-Path $BinDir "budi-daemon.exe"
    & $budiExe --version
    if ($LASTEXITCODE -ne 0) { Fail "Installed budi.exe failed to run" }
    & $daemonExe --version
    if ($LASTEXITCODE -ne 0) { Fail "Installed budi-daemon.exe failed to run" }

    # Check PATH.
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$BinDir*") {
        Log "Adding $BinDir to user PATH..."
        [Environment]::SetEnvironmentVariable("Path", "$BinDir;$userPath", "User")
        $env:Path = "$BinDir;$env:Path"
        Log "PATH updated. Restart your terminal for it to take effect."
    }

    $ver = & $budiExe --version 2>$null
    if (-not $ver) { $ver = "budi" }
    Log ""
    Log "Installed: $ver"
    Log ""

    # Stop existing daemon before init (running executables cannot be overwritten on Windows).
    try {
        Stop-Process -Name "budi-daemon" -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 500
    } catch { }

    # Skip init if called from `budi update` (update handles its own post-install sequence).
    if ($env:BUDI_SKIP_INIT -eq "1") {
        Log "Skipping init (update mode)."
        return
    }

    # Auto-run budi init for a seamless setup experience.
    Log "Running budi init..."
    Log ""
    & $budiExe init
    $initExit = $LASTEXITCODE
    if ($initExit -eq 0 -or $initExit -eq 2) {
        Log ""
        if ($initExit -eq 2) {
            Log "Setup complete with warnings. Run 'budi doctor' to check what needs fixing."
        } else {
            Log "Setup complete! Restart Claude Code and Cursor to activate hooks."
        }
    } else {
        Fail "budi init failed (exit code $initExit). Run 'budi doctor' to diagnose."
    }
} finally {
    if (Test-Path $tempDir) { Remove-Item $tempDir -Recurse -Force -ErrorAction SilentlyContinue }
}
