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
    $target = "aarch64-pc-windows-msvc"
} else {
    $target = "x86_64-pc-windows-msvc"
}
$assetName = "budi-$tag-$target.zip"
$baseUrl = "https://github.com/$Repo/releases/download/$tag"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) "budi-install-$(Get-Random)"
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

try {
    Log "Downloading $assetName ($tag)..."
    $zipPath = Join-Path $tempDir $assetName
    Invoke-WebRequest -Uri "$baseUrl/$assetName" -OutFile $zipPath -UseBasicParsing

    # Verify checksum if available.
    try {
        $sumsPath = Join-Path $tempDir "SHA256SUMS"
        Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing
        $expected = (Get-Content $sumsPath | Where-Object { $_ -match $assetName } | ForEach-Object { ($_ -split '\s+')[0] })
        if ($expected) {
            $actual = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
            if ($expected -ne $actual) { Fail "Checksum mismatch for $assetName" }
            Log "Checksum verified."
        }
    } catch {
        Log "Checksum file unavailable - skipping verification."
    }

    Log "Extracting..."
    $extractDir = Join-Path $tempDir "extracted"
    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
    $pkgDir = Join-Path $extractDir "budi-$tag-$target"
    if (-not (Test-Path $pkgDir)) { Fail "Unexpected archive layout" }

    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    foreach ($bin in @("budi.exe", "budi-daemon.exe")) {
        $src = Join-Path $pkgDir $bin
        if (Test-Path $src) {
            Copy-Item $src (Join-Path $BinDir $bin) -Force
            Log "Installed $bin -> $BinDir\$bin"
        }
    }

    # Verify.
    $budiExe = Join-Path $BinDir "budi.exe"
    & $budiExe --version
    if ($LASTEXITCODE -ne 0) { Fail "Installed binary failed to run" }

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
    if ($LASTEXITCODE -eq 0 -or $LASTEXITCODE -eq 2) {
        Log ""
        if ($LASTEXITCODE -eq 2) {
            Log "Setup complete with warnings. Run 'budi doctor' to check what needs fixing."
        } else {
            Log "Setup complete! Restart Claude Code and Cursor to activate hooks."
        }
    } else {
        Log ""
        Log "budi init failed. Run 'budi doctor' to check what needs fixing."
    }
} finally {
    if (Test-Path $tempDir) { Remove-Item $tempDir -Recurse -Force -ErrorAction SilentlyContinue }
}
