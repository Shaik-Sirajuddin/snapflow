#!/usr/bin/env pwsh
# Snapflow installer for Windows -- downloads the latest GitHub Release
# bundle built by .github/workflows/build-windows.yml and installs it.
# Mirrors scripts/install.sh's behavior (version-skip, checksum, backup-on-
# upgrade, PATH persistence) for the platform install.sh explicitly can't
# cover (`uname`/bash-only, no Windows path in that script by design).
#
# Usage:
#   irm https://raw.githubusercontent.com/Shaik-Sirajuddin/snapflow/main/scripts/install.ps1 | iex
#
# Env overrides (same names as install.sh, read via $env:):
#   SNAPFLOW_VERSION      release tag to install (default: latest)
#   SNAPFLOW_INSTALL_DIR  where the bundle is unpacked (default: $env:LOCALAPPDATA\snapflow)
#   SNAPFLOW_BIN_DIR      where snapflowd.exe/snapflow.exe get PATH'd (default: $env:LOCALAPPDATA\snapflow\bin)
#   SNAPFLOW_ASSET_URL    skip the GitHub API lookup and install this zip URL directly
#                         (mainly for testing against a non-published build)
#
$ErrorActionPreference = "Stop"

function Info($msg) { Write-Host "==> $msg" }
function Die($msg) { Write-Error "error: $msg"; exit 1 }

function Start-DaemonAndWait($daemonPath) {
    # A responsive daemon owns the lock already; reuse it and never restart
    # the user's live projects during an upgrade. status is also the portable
    # readiness check: on Windows it dials the named pipe directly.
    & $daemonPath status *> $null
    if ($LASTEXITCODE -eq 0) {
        Info "snapflowd is already running; reusing the existing daemon."
        return
    }

    $logDir = Join-Path $env:LOCALAPPDATA "snapflow"
    New-Item -ItemType Directory -Path $logDir -Force | Out-Null
    $stdout = Join-Path $logDir "snapflowd-install.stdout.log"
    $stderr = Join-Path $logDir "snapflowd-install.stderr.log"
    Info "Starting snapflowd in the background..."
    Start-Process -FilePath $daemonPath -ArgumentList @("serve") -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr | Out-Null

    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        Start-Sleep -Milliseconds 250
        & $daemonPath status *> $null
        if ($LASTEXITCODE -eq 0) {
            Info "snapflowd is ready."
            return
        }
    } while ([DateTime]::UtcNow -lt $deadline)
    Die "snapflowd did not become ready within 15 seconds; see $stderr"
}

if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64" -and $env:PROCESSOR_ARCHITEW6432 -ne "AMD64") {
    Die "unsupported architecture: $env:PROCESSOR_ARCHITECTURE -- only x86_64/AMD64 builds are published"
}

$Repo = "Shaik-Sirajuddin/snapflow"
$Version = if ($env:SNAPFLOW_VERSION) { $env:SNAPFLOW_VERSION } else { "latest" }
$InstallDir = if ($env:SNAPFLOW_INSTALL_DIR) { $env:SNAPFLOW_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "snapflow" }
$BinDir = if ($env:SNAPFLOW_BIN_DIR) { $env:SNAPFLOW_BIN_DIR } else { Join-Path $InstallDir "bin" }
$VersionFile = Join-Path $InstallDir ".snapflow-version"

function Resolve-AssetUrl {
    if ($env:SNAPFLOW_ASSET_URL) {
        return @{ Url = $env:SNAPFLOW_ASSET_URL; Tag = $null }
    }

    $apiUrl = if ($Version -eq "latest") {
        "https://api.github.com/repos/$Repo/releases/latest"
    } else {
        "https://api.github.com/repos/$Repo/releases/tags/$Version"
    }

    Info "Looking up release ($Version) for windows..."
    try {
        $release = Invoke-RestMethod -Uri $apiUrl -Headers @{ "User-Agent" = "snapflow-install.ps1" }
    } catch {
        Die "failed to query $apiUrl -- has a Windows release been published yet? ($_)"
    }

    $asset = $release.assets | Where-Object { $_.name -match '^snapflow-windows-x86_64-.*\.zip$' -and $_.name -notmatch '\.sha256$' } | Select-Object -First 1
    if (-not $asset) {
        Die "no windows zip found in release $Version -- check https://github.com/$Repo/releases"
    }
    return @{ Url = $asset.browser_download_url; Tag = $release.tag_name }
}

$resolved = Resolve-AssetUrl
$assetUrl = $resolved.Url
$targetVersion = if ($resolved.Tag) { $resolved.Tag } else { Split-Path $assetUrl -Leaf }

if (Test-Path $VersionFile) {
    $installedVersion = Get-Content $VersionFile -Raw -ErrorAction SilentlyContinue
    $snapflowdExe = Join-Path $BinDir "snapflowd.exe"
    if ($installedVersion -and $installedVersion.Trim() -eq $targetVersion -and (Test-Path $snapflowdExe)) {
        Info "snapflow $targetVersion is already installed and up to date -- nothing to do."
        Start-DaemonAndWait $snapflowdExe
        exit 0
    }
}

$tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmpDir | Out-Null
try {
    $archiveName = Split-Path $assetUrl -Leaf
    $archive = Join-Path $tmpDir $archiveName

    Info "Downloading $archiveName..."
    Invoke-WebRequest -Uri $assetUrl -OutFile $archive -UserAgent "snapflow-install.ps1"

    # Only the sidecar *download* is allowed to fail soft (some assets may
    # not have one published) -- a genuine checksum mismatch below must be
    # a hard failure, so it's deliberately outside this try/catch. Nesting
    # it inside the same block previously swallowed a real mismatch into
    # the same "no .sha256 found" warning as a missing sidecar (caught by
    # actually testing a corrupted checksum, not assumed).
    $shaUrl = "$assetUrl.sha256"
    $shaFile = "$archive.sha256"
    $haveSha = $true
    try {
        Invoke-WebRequest -Uri $shaUrl -OutFile $shaFile -UserAgent "snapflow-install.ps1" -ErrorAction Stop
    } catch {
        $haveSha = $false
        Write-Warning "no .sha256 found for this asset, skipping checksum verification"
    }
    if ($haveSha) {
        Info "Verifying checksum..."
        # The .sha256 sidecar is `sha256sum` output ("<hash>  <filename>"),
        # written on a Linux/macOS runner -- only the hash field applies here.
        $expected = (Get-Content $shaFile -Raw).Trim().Split(" ")[0].ToLower()
        $actual = (Get-FileHash -Path $archive -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) {
            Die "checksum verification failed (expected $expected, got $actual)"
        }
    }

    # Same bounded backup-on-upgrade pattern as install.sh: keep exactly one
    # previous bundle, not a version history, so a botched extraction
    # doesn't leave nothing to fall back to. $InstallDir only ever holds
    # static application content -- real user/project data lives under
    # snapshotd's own data dir, a separate tree this script never touches.
    if (Test-Path $InstallDir) {
        Info "Backing up previous install to $InstallDir.prev..."
        $prevDir = "$InstallDir.prev"
        if (Test-Path $prevDir) { Remove-Item -Recurse -Force $prevDir }
        Rename-Item -Path $InstallDir -NewName (Split-Path $prevDir -Leaf)
    }

    Info "Extracting to $InstallDir..."
    $extractTmp = Join-Path $tmpDir "extracted"
    Expand-Archive -Path $archive -DestinationPath $extractTmp
    # The release zip's top-level entry is the versioned bundle dir itself
    # (snapflow-windows-x86_64-<date>/...) -- strip that one level, same as
    # install.sh's `tar --strip-components=1`, so $InstallDir's own
    # top-level is bin/ and Snapflow/ directly.
    $bundleRoot = Get-ChildItem -Path $extractTmp -Directory | Select-Object -First 1
    if (-not $bundleRoot) { Die "unexpected archive layout: no top-level directory found in $archiveName" }
    New-Item -ItemType Directory -Path $InstallDir | Out-Null
    Get-ChildItem -Path $bundleRoot.FullName | Move-Item -Destination $InstallDir
    Set-Content -Path $VersionFile -Value $targetVersion -NoNewline
} finally {
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
}

# The bundle's own snapflowd.exe always lands at $InstallDir\bin -- if
# $BinDir was overridden to somewhere else, copy it there too (matching
# install.sh's explicit `ln -sf $INSTALL_DIR/bin/snapflowd $BIN_DIR/...`
# for a customizable BIN_DIR; a plain copy here since Windows symlinks
# need admin/dev-mode, unlike Unix). Skipped when $BinDir already IS the
# bundle's bin dir (the default case) to avoid copying onto itself.
$bundleBinDir = Join-Path $InstallDir "bin"
if ((Resolve-Path $BinDir -ErrorAction SilentlyContinue).Path -ne (Resolve-Path $bundleBinDir -ErrorAction SilentlyContinue).Path) {
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    Copy-Item -Path (Join-Path $bundleBinDir "snapflowd.exe") -Destination $BinDir -Force
    Info "Copied snapflowd.exe -> $BinDir"
}

# snapflow.exe lives in the bundled Snapflow/ app folder (needs its
# neighboring DLLs/Qt-plugins/qml dirs, same "don't run it standalone
# outside its own folder" constraint install.sh's Linux path documents for
# the equivalent wrapper-script/raw-binary distinction) -- PATH both
# $BinDir (snapflowd.exe) and Snapflow\ (snapflow.exe) rather than
# copying/symlinking snapflow.exe out on its own.
$snapflowAppDir = Get-ChildItem -Path $InstallDir -Directory -Filter "Snapflow*" -ErrorAction SilentlyContinue | Select-Object -First 1
$snapshotBinPath = $null
if ($snapflowAppDir) {
    $candidate = Join-Path $snapflowAppDir.FullName "snapflow.exe"
    if (Test-Path $candidate) { $snapshotBinPath = $candidate }
}
if (-not $snapshotBinPath) {
    $snapshotBinPath = (Get-ChildItem -Path $InstallDir -Recurse -File -Filter "snapflow.exe" -ErrorAction SilentlyContinue | Select-Object -First 1).FullName
}
if ($snapshotBinPath) {
    # Persist the installed production FfiBackend-linked GUI path. This is
    # what snapshotd uses for `launch --gui`; without it an installed daemon
    # falls back to source-checkout discovery and cannot launch the packaged
    # child binary.
    $runtimeConfigDir = Join-Path $env:APPDATA "Snapflow"
    $runtimeConfigFile = Join-Path $runtimeConfigDir "runtime.env"
    New-Item -ItemType Directory -Path $runtimeConfigDir -Force | Out-Null
    $existing = if (Test-Path $runtimeConfigFile) { Get-Content $runtimeConfigFile | Where-Object { $_ -notmatch '^SNAPSHOT_BIN_PATH=' } } else { @() }
    # .NET's UTF8Encoding(false) avoids the BOM emitted by Windows
    # PowerShell 5.1's `-Encoding utf8`; snapshotd also accepts BOM files for
    # compatibility with older installs.
    $runtimeLines = @($existing + "SNAPSHOT_BIN_PATH=$snapshotBinPath")
    [IO.File]::WriteAllText($runtimeConfigFile, (($runtimeLines -join [Environment]::NewLine) + [Environment]::NewLine), ([System.Text.UTF8Encoding]::new($false)))
    Info "Configured production snapshot child -> $snapshotBinPath"
} else {
    Write-Warning "no snapflow.exe found in the installed bundle; daemon launch --gui requires SNAPSHOT_BIN_PATH"
}
$pathDirs = @($BinDir)
if ($snapflowAppDir) { $pathDirs += $snapflowAppDir.FullName }

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$userPathParts = if ($userPath) { $userPath -split ";" } else { @() }
$updatedPath = $false
foreach ($dir in $pathDirs) {
    if ($userPathParts -notcontains $dir) {
        $userPathParts += $dir
        $updatedPath = $true
    }
    # Make it usable in *this* session immediately, same reasoning as
    # install.sh exporting PATH for the rest of its own script/subshells --
    # a piped `iex` can't mutate the parent interactive shell's PATH either.
    if (($env:Path -split ";") -notcontains $dir) {
        $env:Path = "$dir;$env:Path"
    }
}
if ($updatedPath) {
    [Environment]::SetEnvironmentVariable("Path", ($userPathParts -join ";"), "User")
    Info "Added $($pathDirs -join ', ') to your User PATH (new terminals will pick it up automatically)"
}

Start-DaemonAndWait (Join-Path $BinDir "snapflowd.exe")
Info "Done. snapflowd is running; launch 'snapflow.exe' to open the editor."
