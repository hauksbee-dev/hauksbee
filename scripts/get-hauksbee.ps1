<#
.SYNOPSIS
  Download and transactionally install hauksbee, hauksbee-ci, and hauksbee-mcp on Windows.

.DESCRIPTION
  The Windows counterpart of scripts/get-hauksbee.sh. Fetches one explicitly
  pinned GitHub Release asset, verifies its source commit and sha256 with
  Get-FileHash, and installs all three executables to the install prefix. Safe
  to re-run: the complete new bin directory is staged and validated before the
  old directory is swapped out; a failed swap restores the old installation.

  Windows releases use the `windows-x86_64` suffix and `.zip` assets. They are
  permissive-only: Renode and Espressif QEMU are compiled in, while the AVR
  backend is disabled because this repository has no supported native MSVC
  libsimavr build.

  Which build you get:
    Windows releases are permissive-only. The binary is Apache-2.0, cannot do
    AVR co-sim, and retains the Renode and Espressif QEMU backends. The
    installer selects that shape even when -Permissive is omitted and names
    the limitation before downloading. LICENSE-BINARY.txt records it too.

.PARAMETER Version
  Install a specific release tag (e.g. v0.1.0). Default: the latest release.

.PARAMETER ExpectedCommit
  Optional exact 40-character source commit printed by the immutable release.
  When given, the installer resolves Version through GitHub and refuses any
  mismatch; either way the resolved release commit must match the version
  every installed binary reports.

.PARAMETER Prefix
  Install binaries to <Prefix>\bin. Default: $env:LOCALAPPDATA\hauksbee.

.PARAMETER Permissive
  Accepted for parity with the Unix installer. Windows has only this GPL-free
  shape, so omitting the switch selects the same artifact with a notice.

.EXAMPLE
  irm https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.ps1 | iex

.EXAMPLE
  # Fully pinned: fetch the installer at the release commit and refuse drift.
  $releaseCommit = "REPLACE_WITH_RELEASE_COMMIT_SHA"
  $releaseTag = "REPLACE_WITH_RELEASE_TAG"
  $script = irm "https://raw.githubusercontent.com/hauksbee-dev/hauksbee/$releaseCommit/scripts/get-hauksbee.ps1"
  & ([ScriptBlock]::Create($script)) -Version $releaseTag -ExpectedCommit $releaseCommit

.EXAMPLE
  .\get-hauksbee.ps1 -Version v0.1.0 -ExpectedCommit 0123456789abcdef0123456789abcdef01234567 -Permissive

.NOTES
  No credential is needed: the releases are public. Optionally set
  $env:HAUKSBEE_GITHUB_TOKEN (or the CI-compatible $env:GITHUB_TOKEN fallback)
  to raise the GitHub API rate limit or to target a private mirror or GitHub
  Enterprise host via $env:HAUKSBEE_API_BASE.
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$ExpectedCommit = "",
    [string]$Prefix = "",
    [switch]$Permissive
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($Version -and $Version -notmatch '^v[0-9A-Za-z._-]+$') {
    throw "-Version must name one explicit v* release tag."
}
if ($ExpectedCommit -and $ExpectedCommit -notmatch '^[0-9a-f]{40}$') {
    throw "-ExpectedCommit must be the exact lowercase 40-character release source commit."
}

$Repo = "hauksbee-dev/hauksbee"
# The API base is overridable for GitHub Enterprise or the local contract test.
$ApiBase = if ($env:HAUKSBEE_API_BASE) { $env:HAUKSBEE_API_BASE } else { "https://api.github.com/repos/$Repo" }
# Optional: the releases are public, so no credential is needed. A token, when
# present, raises the API rate limit or authorizes a private mirror / GHE host.
$apiToken = if ($env:HAUKSBEE_GITHUB_TOKEN) { $env:HAUKSBEE_GITHUB_TOKEN } else { $env:GITHUB_TOKEN }

function Assert-SafeZip([string]$Archive) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        foreach ($entry in $zip.Entries) {
            $normalized = $entry.FullName -replace '\\', '/'
            if ($normalized.StartsWith('/') -or $normalized -match '(^|/)\.\.(/|$)' -or $normalized -match '^[A-Za-z]:') {
                throw "unsafe ZIP entry in $Archive`: $($entry.FullName)"
            }
            $unixType = (($entry.ExternalAttributes -shr 16) -band 0xF000)
            if ($unixType -notin @(0, 0x4000, 0x8000)) {
                throw "unsafe ZIP member type in $Archive`: $($entry.FullName)"
            }
        }
    } finally {
        $zip.Dispose()
    }
}

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
if (-not $Permissive) {
    Write-Host "Windows releases are permissive-only: selecting the renode+qemu build (AVR disabled)."
    $Permissive = $true
}

# ---------------------------------------------------------------------------
# Resolve the release tag (latest or pinned)
# ---------------------------------------------------------------------------
$headers = @{
    "Accept" = "application/vnd.github+json"
    "X-GitHub-Api-Version" = "2026-03-10"
}
if ($apiToken) { $headers["Authorization"] = "Bearer $apiToken" }

$releaseUri = if ($Version) { "$ApiBase/releases/tags/$Version" } else { "$ApiBase/releases/latest" }
try {
    $release = Invoke-RestMethod -Uri $releaseUri -Headers $headers
} catch {
    Write-Error ("Could not resolve the release through the GitHub API: $_`n" +
        "Pass -Version vX.Y.Z explicitly, or check https://github.com/$Repo/releases")
    exit 1
}
if ($Version -and $release.tag_name -ne $Version) {
    Write-Error "The GitHub API response did not identify the requested release tag."
    exit 1
}
if (-not $Version) {
    $Version = [string]$release.tag_name
    if ($Version -notmatch '^v[0-9A-Za-z._-]+$') {
        Write-Error "The latest release reports an unusable tag name '$Version'."
        exit 1
    }
    Write-Host "Latest release: $Version"
}
if (-not $release.immutable) {
    Write-Error "Release $Version is not immutable; refusing replaceable release assets."
    exit 1
}
$tagCommit = Invoke-RestMethod -Uri "$ApiBase/commits/$Version" -Headers $headers
if (-not $tagCommit.sha -or $tagCommit.sha -notmatch '^[0-9a-f]{40}$') {
    Write-Error "Could not resolve $Version to one immutable source commit."
    exit 1
}
if ($ExpectedCommit -and $tagCommit.sha -cne $ExpectedCommit) {
    Write-Error "Release $Version resolves to $($tagCommit.sha), not expected commit $ExpectedCommit."
    exit 1
}
# From here on the API-resolved commit is the identity every staged and
# installed binary must attest, whether or not the caller pinned it.
$ResolvedCommit = [string]$tagCommit.sha

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
function Resolve-Asset([string]$Name) {
    $matches = @($release.assets | Where-Object { $_.name -ceq $Name })
    if ($matches.Count -ne 1 -or -not $matches[0].url -or -not $matches[0].digest) {
        throw "Release $Version does not contain exactly one $Name asset."
    }
    $url = [string]$matches[0].url
    $digest = [string]$matches[0].digest
    $allowedPrefix = "$ApiBase/releases/assets/"
    if (-not $url.StartsWith($allowedPrefix, [System.StringComparison]::Ordinal)) {
        throw "Refusing release asset URL outside the configured GitHub API: $url"
    }
    if ($digest -notmatch '^sha256:[0-9a-f]{64}$') {
        throw "Release asset $Name has no valid GitHub asset digest."
    }
    return @{ Url = $url; Digest = $digest.Substring(7) }
}
$ZipAsset = Resolve-Asset $ZipName
$ChecksumAsset = Resolve-Asset $ChecksumName
$ZipUrl = $ZipAsset.Url
$ChecksumUrl = $ChecksumAsset.Url
$assetHeaders = @{
    "Accept" = "application/octet-stream"
    "X-GitHub-Api-Version" = "2026-03-10"
}
if ($apiToken) { $assetHeaders["Authorization"] = "Bearer $apiToken" }

function Invoke-TokenFreeVersionProbe([string]$Path) {
    $savedHauksbee = [Environment]::GetEnvironmentVariable("HAUKSBEE_GITHUB_TOKEN", "Process")
    $savedGithub = [Environment]::GetEnvironmentVariable("GITHUB_TOKEN", "Process")
    $savedGh = [Environment]::GetEnvironmentVariable("GH_TOKEN", "Process")
    try {
        Remove-Item Env:HAUKSBEE_GITHUB_TOKEN -ErrorAction SilentlyContinue
        Remove-Item Env:GITHUB_TOKEN -ErrorAction SilentlyContinue
        Remove-Item Env:GH_TOKEN -ErrorAction SilentlyContinue
        $output = (& $Path --version 2>&1 | Out-String).Trim()
        return @{ ExitCode = $LASTEXITCODE; Output = $output }
    } finally {
        if ($null -eq $savedHauksbee) { Remove-Item Env:HAUKSBEE_GITHUB_TOKEN -ErrorAction SilentlyContinue } else { $env:HAUKSBEE_GITHUB_TOKEN = $savedHauksbee }
        if ($null -eq $savedGithub) { Remove-Item Env:GITHUB_TOKEN -ErrorAction SilentlyContinue } else { $env:GITHUB_TOKEN = $savedGithub }
        if ($null -eq $savedGh) { Remove-Item Env:GH_TOKEN -ErrorAction SilentlyContinue } else { $env:GH_TOKEN = $savedGh }
    }
}

function Assert-BinaryVersion([string]$Path, [string]$Name, [string]$ExpectedVersion) {
    $probe = Invoke-TokenFreeVersionProbe $Path
    $escapedName = [regex]::Escape(($Name -replace '\.exe$', ''))
    $escapedVersion = [regex]::Escape($ExpectedVersion)
    $escapedCommit = [regex]::Escape($ResolvedCommit)
    if ($probe.ExitCode -ne 0 -or $probe.Output -notmatch "(?m)^$escapedName $escapedVersion \(git $escapedCommit\)$") {
        throw "staged $Name does not identify release version $ExpectedVersion`: $($probe.Output)"
    }
}

function Recover-StaleBackup([string]$Target) {
    $parent = Split-Path -Parent $Target
    $leaf = Split-Path -Leaf $Target
    $backups = @(Get-ChildItem -LiteralPath $parent -Filter "$leaf.install-backup-*" -Directory -ErrorAction SilentlyContinue)
    if ($backups.Count -gt 1) {
        throw "multiple interrupted-install backups exist for $Target; refusing to guess which is authoritative"
    }
    if ($backups.Count -eq 1) {
        $committed = Join-Path $backups[0].FullName ".hauksbee-install-committed"
        if ((Test-Path -LiteralPath $Target) -and (Test-Path -LiteralPath $committed)) {
            Remove-Item -LiteralPath $backups[0].FullName -Recurse -Force -ErrorAction SilentlyContinue
            return
        }
        if (Test-Path -LiteralPath $Target) {
            Remove-Item -LiteralPath $Target -Recurse -Force
        }
        Move-Item -LiteralPath $backups[0].FullName -Destination $Target
        Write-Warning "Rolled back an installation interrupted before final acceptance."
    }
}

function Replace-Tree([string]$Staging, [string]$Target) {
    $parent = Split-Path -Parent $Target
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    $lockPath = "$Target.install.lock"
    $lock = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try {
        Recover-StaleBackup $Target
        $backup = "$Target.install-backup-$([guid]::NewGuid().ToString('N'))"
        $movedOld = $false
        try {
            if (Test-Path -LiteralPath $Target) {
                Move-Item -LiteralPath $Target -Destination $backup
                $movedOld = $true
            }
            if ($env:HAUKSBEE_TEST_FAIL_INSTALL_SWAP) { throw "injected install swap failure" }
            Move-Item -LiteralPath $Staging -Destination $Target
        } catch {
            if (Test-Path -LiteralPath $Target) {
                Remove-Item -LiteralPath $Target -Recurse -Force
            }
            if ($movedOld -and (Test-Path -LiteralPath $backup)) {
                Move-Item -LiteralPath $backup -Destination $Target
            }
            throw
        }
        return [pscustomobject]@{ Target = $Target; Backup = $backup; Lock = $lock }
    } catch {
        $lock.Dispose()
        throw
    }
}

function Mark-TreeReplaceCommitted($Transaction) {
    if (Test-Path -LiteralPath $Transaction.Backup) {
        New-Item -ItemType File -Path (Join-Path $Transaction.Backup ".hauksbee-install-committed") -Force | Out-Null
    }
}

function Complete-TreeReplace($Transaction) {
    try {
        if (Test-Path -LiteralPath $Transaction.Backup) {
            Remove-Item -LiteralPath $Transaction.Backup -Recurse -Force -ErrorAction SilentlyContinue
        }
    } finally {
        $Transaction.Lock.Dispose()
    }
}

function Undo-TreeReplace($Transaction) {
    try {
        if (Test-Path -LiteralPath $Transaction.Target) {
            Remove-Item -LiteralPath $Transaction.Target -Recurse -Force
        }
        if (Test-Path -LiteralPath $Transaction.Backup) {
            Remove-Item -LiteralPath (Join-Path $Transaction.Backup ".hauksbee-install-committed") -Force -ErrorAction SilentlyContinue
            Move-Item -LiteralPath $Transaction.Backup -Destination $Transaction.Target
        }
    } finally {
        $Transaction.Lock.Dispose()
    }
}

function Remove-AbandonedStaging([string]$Path) {
    if ($Path -and (Test-Path -LiteralPath $Path)) {
        Remove-Item -LiteralPath $Path -Recurse -Force
    }
}

# ---------------------------------------------------------------------------
# Download to a temp directory; verify; then install
# ---------------------------------------------------------------------------
$workDir = Join-Path ([System.IO.Path]::GetTempPath()) "get-hauksbee-$([System.IO.Path]::GetRandomFileName())"
$installStaging = $null
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
                Invoke-WebRequest -Uri $Uri -OutFile $OutFile -Headers $assetHeaders -UseBasicParsing
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

    # GitHub's immutable-release API attests each uploaded asset independently.
    # Verify those server-recorded digests before trusting the checksum sidecar;
    # a coherently replaced zip + sidecar must not authenticate itself.
    $zipGitHubDigest = (Get-FileHash -Algorithm SHA256 -Path $zipPath).Hash.ToLowerInvariant()
    $checksumGitHubDigest = (Get-FileHash -Algorithm SHA256 -Path $checksumPath).Hash.ToLowerInvariant()
    if ($zipGitHubDigest -cne $ZipAsset.Digest -or $checksumGitHubDigest -cne $ChecksumAsset.Digest) {
        throw "Downloaded bytes do not match the GitHub asset digest for immutable release $Version."
    }

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
    $actual = $zipGitHubDigest
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
    Assert-SafeZip $zipPath
    Expand-Archive -Path $zipPath -DestinationPath $workDir

    # Same layout as the tarballs: <base>/bin/<binary>, with .exe on Windows.
    $binDir = Join-Path (Join-Path $workDir $AssetName) "bin"
    $hauksbeeExe = Join-Path $binDir "hauksbee.exe"
    $ciExe = Join-Path $binDir "hauksbee-ci.exe"
    $mcpExe = Join-Path $binDir "hauksbee-mcp.exe"
    if (-not (Test-Path $hauksbeeExe) -or -not (Test-Path $ciExe) -or -not (Test-Path $mcpExe)) {
        Write-Error "Unexpected zip layout: one or more of bin\hauksbee.exe, bin\hauksbee-ci.exe, bin\hauksbee-mcp.exe is missing."
        exit 1
    }

    # -----------------------------------------------------------------------
    # Install to <Prefix>\bin
    # -----------------------------------------------------------------------
    $installDir = Join-Path $Prefix "bin"
    $installStaging = Join-Path $Prefix "bin.install-staging-$([guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $Prefix -Force | Out-Null
    if (Test-Path -LiteralPath $installStaging) {
        Remove-Item -LiteralPath $installStaging -Recurse -Force
    }
    New-Item -ItemType Directory -Path $installStaging | Out-Null
    foreach ($binary in @("hauksbee.exe", "hauksbee-ci.exe", "hauksbee-mcp.exe")) {
        Copy-Item -LiteralPath (Join-Path $binDir $binary) -Destination (Join-Path $installStaging $binary)
    }
    foreach ($binary in @("hauksbee.exe", "hauksbee-ci.exe", "hauksbee-mcp.exe")) {
        Assert-BinaryVersion (Join-Path $installStaging $binary) $binary $VersionBare
    }
    $installTransaction = Replace-Tree $installStaging $installDir
    try {
        foreach ($binary in @("hauksbee.exe", "hauksbee-ci.exe", "hauksbee-mcp.exe")) {
            Assert-BinaryVersion (Join-Path $installDir $binary) $binary $VersionBare
        }
        Mark-TreeReplaceCommitted $installTransaction
        Complete-TreeReplace $installTransaction
        $installTransaction = $null
    } finally {
        if ($installTransaction) { Undo-TreeReplace $installTransaction }
    }

    Write-Host ""
    Write-Host "Installed:"
    Write-Host "  $(Join-Path $installDir 'hauksbee.exe')"
    Write-Host "  $(Join-Path $installDir 'hauksbee-ci.exe')"
    Write-Host "  $(Join-Path $installDir 'hauksbee-mcp.exe')"

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

    $licenceLine = "Apache-2.0 binary (Windows permissive build: no avr backend, no libsimavr, no GPL code)."

    Write-Host ""
    Write-Host "hauksbee $Version installed. Run: hauksbee --help"
    Write-Host "Licence: $licenceLine"
} finally {
    Remove-AbandonedStaging $installStaging
    Remove-Item -Recurse -Force $workDir -ErrorAction SilentlyContinue
}
