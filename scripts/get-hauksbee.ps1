<#
.SYNOPSIS
  get-hauksbee.ps1 - download and install prebuilt hauksbee + hauksbee-ci binaries on Windows.

.DESCRIPTION
  The Windows counterpart of scripts/get-hauksbee.sh. Fetches the latest GitHub
  Release asset (or a pinned version), verifies the sha256 checksum with
  Get-FileHash, and extracts hauksbee.exe and hauksbee-ci.exe to the install
  prefix. Safe to re-run: an existing install is only overwritten once the
  checksum of the new download is verified.

  NOTE: there are no published Windows release assets yet. Windows is
  evaluated but NOT a promised target (docs/about/release-and-licensing.md
  section 5); this installer implements the section 3 naming contract with a
  `windows-x86_64` suffix and `.zip` assets so the release workflow and the
  installer cannot drift when that leg lands. Until a `windows-x86_64` asset
  exists on a release, the download step will report 404: build from source
  instead (https://github.com/ETM-Code/hauksbee#quickstart).

  Which build you get:
    Default: the full build, AVR / ATmega co-simulation included. It statically
    links libsimavr, so THE BINARY is GPL-3.0 (hauksbee's source stays
    Apache-2.0). GPL-3.0 constrains redistributing the binary, not running it.
    -Permissive: the same tool without the avr backend, so no GPL code is
    linked and the binary is Apache-2.0. Take it if you redistribute or embed
    hauksbee. It cannot do AVR co-sim; Renode and Espressif QEMU still work.
    Either way, LICENSE-BINARY.txt inside the zip spells out the terms.
    (A first Windows release would ship the permissive shape only: the avr
    backend needs an MSYS2 libsimavr build nobody has written yet.)

.PARAMETER Version
  Install a specific release tag (e.g. v0.1.0). Default: latest.

.PARAMETER Prefix
  Install binaries to <Prefix>\bin. Default: $env:LOCALAPPDATA\hauksbee.

.PARAMETER Permissive
  Install the GPL-free build instead of the default one.

.EXAMPLE
  irm https://raw.githubusercontent.com/ETM-Code/hauksbee/main/scripts/get-hauksbee.ps1 | iex

.EXAMPLE
  .\get-hauksbee.ps1 -Version v0.1.0 -Permissive

.NOTES
  Set $env:GITHUB_TOKEN to avoid GitHub API rate limits (60 req/hr unauthed).
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Prefix = "",
    [switch]$Permissive
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Repo = "ETM-Code/hauksbee"
# Base URLs are overridable for the same reason as the bash installer: a
# GitHub Enterprise host, a self-hosted mirror, or a local mock for testing
# the download/verify/install flow.
$ApiBase = if ($env:HAUKSBEE_API_BASE) { $env:HAUKSBEE_API_BASE } else { "https://api.github.com/repos/$Repo" }
$ReleasesBase = if ($env:HAUKSBEE_RELEASES_BASE) { $env:HAUKSBEE_RELEASES_BASE } else { "https://github.com/$Repo/releases/download" }

if (-not $Prefix) {
    $Prefix = Join-Path $env:LOCALAPPDATA "hauksbee"
}

# ---------------------------------------------------------------------------
# Detect architecture -> asset name suffix
# ---------------------------------------------------------------------------
# Only x86_64 is contracted for now. ARM64 Windows machines run x64 binaries
# through emulation, but that is not a tested claim, so refuse rather than
# guess (same posture as the bash installer's unknown-arch branch).
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($arch -ne [System.Runtime.InteropServices.Architecture]::X64) {
    Write-Error ("No prebuilt hauksbee binary for Windows/$arch (contracted: windows-x86_64). " +
        "Build from source: https://github.com/$Repo#quickstart")
    exit 1
}
$AssetSuffix = "windows-x86_64"
Write-Host "Detected platform: Windows/$arch -> asset suffix: $AssetSuffix"

# ---------------------------------------------------------------------------
# Resolve the release tag (latest or pinned)
# ---------------------------------------------------------------------------
$headers = @{}
if ($env:GITHUB_TOKEN) {
    $headers["Authorization"] = "Bearer $($env:GITHUB_TOKEN)"
}

if (-not $Version) {
    Write-Host "Fetching latest release tag..."
    try {
        $release = Invoke-RestMethod -Uri "$ApiBase/releases/latest" -Headers $headers
        $Version = $release.tag_name
    } catch {
        Write-Error ("Could not determine the latest release tag from the GitHub API: $_`n" +
            "Pass -Version vX.Y.Z explicitly, or check https://github.com/$Repo/releases")
        exit 1
    }
    if (-not $Version) {
        Write-Error "The GitHub API answered without a tag_name. Pass -Version vX.Y.Z explicitly."
        exit 1
    }
}

# Strip a leading 'v' to match the asset naming convention used in bundle.sh.
$VersionBare = $Version -replace '^v', ''
Write-Host "Installing hauksbee $Version ($VersionBare)"

# ---------------------------------------------------------------------------
# Construct asset names (the section 3 naming contract, .zip on Windows)
# ---------------------------------------------------------------------------
$ShapeSuffix = if ($Permissive) { "-permissive" } else { "" }
$AssetName = "hauksbee-$VersionBare-$AssetSuffix$ShapeSuffix"
$ZipName = "$AssetName.zip"
$ChecksumName = "$ZipName.sha256"
$ZipUrl = "$ReleasesBase/$Version/$ZipName"
$ChecksumUrl = "$ReleasesBase/$Version/$ChecksumName"

# ---------------------------------------------------------------------------
# Download to a temp directory; verify; then install
# ---------------------------------------------------------------------------
$workDir = Join-Path ([System.IO.Path]::GetTempPath()) "get-hauksbee-$([System.IO.Path]::GetRandomFileName())"
New-Item -ItemType Directory -Path $workDir | Out-Null
try {
    $zipPath = Join-Path $workDir $ZipName
    $checksumPath = Join-Path $workDir $ChecksumName

    # Download with retries. An explicit loop rather than -MaximumRetryCount,
    # which needs PowerShell 6+; Windows PowerShell 5.1 (the OS default) must
    # be able to run this installer too.
    function Get-WithRetry([string]$Uri, [string]$OutFile) {
        $attempts = 3
        for ($i = 1; $i -le $attempts; $i++) {
            try {
                Invoke-WebRequest -Uri $Uri -OutFile $OutFile -UseBasicParsing
                return
            } catch {
                if ($i -eq $attempts) { throw }
                Write-Host "  download failed (attempt $i/$attempts), retrying in 2s..."
                Start-Sleep -Seconds 2
            }
        }
    }

    Write-Host "Downloading $ZipName..."
    Get-WithRetry $ZipUrl $zipPath

    Write-Host "Downloading checksum..."
    Get-WithRetry $ChecksumUrl $checksumPath

    # -----------------------------------------------------------------------
    # Verify sha256 checksum
    # -----------------------------------------------------------------------
    # The .sha256 file is `<hex>  <basename>` (shasum -a 256 format, produced
    # by bundle.sh). Parse the hex, hash the download with Get-FileHash, and
    # refuse to install on any mismatch.
    Write-Host "Verifying checksum..."
    $checksumLine = (Get-Content $checksumPath -First 1).Trim()
    $expected = ($checksumLine -split '\s+')[0].ToLowerInvariant()
    if ($expected -notmatch '^[0-9a-f]{64}$') {
        Write-Error "The checksum file does not look like a sha256 line: '$checksumLine'. Aborting."
        exit 1
    }
    $actual = (Get-FileHash -Algorithm SHA256 -Path $zipPath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Write-Error ("Checksum mismatch for ${ZipName}:`n  expected $expected`n  got      $actual`n" +
            "Refusing to install a download that does not match its published checksum.")
        exit 1
    }
    Write-Host "Checksum OK."

    # -----------------------------------------------------------------------
    # Extract binaries
    # -----------------------------------------------------------------------
    Write-Host "Extracting binaries..."
    Expand-Archive -Path $zipPath -DestinationPath $workDir

    # Same layout as the tarballs: <base>/bin/<binary>, with .exe on Windows.
    $binDir = Join-Path (Join-Path $workDir $AssetName) "bin"
    $hauksbeeExe = Join-Path $binDir "hauksbee.exe"
    $ciExe = Join-Path $binDir "hauksbee-ci.exe"
    if (-not (Test-Path $hauksbeeExe) -or -not (Test-Path $ciExe)) {
        Write-Error "Unexpected zip layout: bin\hauksbee.exe or bin\hauksbee-ci.exe not found."
        exit 1
    }

    # -----------------------------------------------------------------------
    # Install to <Prefix>\bin
    # -----------------------------------------------------------------------
    $installDir = Join-Path $Prefix "bin"
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
    Copy-Item $hauksbeeExe (Join-Path $installDir "hauksbee.exe") -Force
    Copy-Item $ciExe (Join-Path $installDir "hauksbee-ci.exe") -Force

    Write-Host ""
    Write-Host "Installed:"
    Write-Host "  $(Join-Path $installDir 'hauksbee.exe')"
    Write-Host "  $(Join-Path $installDir 'hauksbee-ci.exe')"

    # -----------------------------------------------------------------------
    # PATH hint
    # -----------------------------------------------------------------------
    $onPath = ($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -ieq $installDir.TrimEnd('\') }
    if (-not $onPath) {
        Write-Host ""
        Write-Host "Add $installDir to your PATH to use the binaries."
        Write-Host "For the current user (takes effect in NEW terminals):"
        Write-Host ""
        Write-Host "  [Environment]::SetEnvironmentVariable('Path',"
        Write-Host "      [Environment]::GetEnvironmentVariable('Path', 'User') + ';$installDir', 'User')"
        Write-Host ""
        Write-Host "Or for this session only:"
        Write-Host ""
        Write-Host "  `$env:Path += ';$installDir'"
    }

    if ($Permissive) {
        $licenceLine = "Apache-2.0 binary (permissive build: no avr backend, no libsimavr, no GPL code)."
    } else {
        $licenceLine = "GPL-3.0 binary (includes the avr backend, which links GPL-3.0 libsimavr); hauksbee's source is Apache-2.0."
    }

    Write-Host ""
    Write-Host "hauksbee $Version installed. Run: hauksbee --help"
    Write-Host "Licence: $licenceLine"
} finally {
    Remove-Item -Recurse -Force $workDir -ErrorAction SilentlyContinue
}
