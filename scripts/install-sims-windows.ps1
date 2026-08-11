<#
.SYNOPSIS
  Install the release-gate Renode and Espressif QEMU builds on Windows.

.DESCRIPTION
  This is the native-Windows counterpart of install-sims.sh --require-pinned.
  Every executable archive is held to a repository-recorded SHA-256, extracted
  into a fresh tree, checked through the executable itself, then swapped into
  the exact locations hauksbee discovers. A failed install leaves the previous
  tree in place.
#>
[CmdletBinding()]
param(
    [string]$CacheDir = "",
    [switch]$Check,
    [switch]$RenodeOnly,
    [switch]$QemuOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RenodeVersion = "1.16.1"
$QemuTag = "esp-develop-9.2.2-20260417"
$QemuAssetVersion = "esp_develop_9.2.2_20260417"
$assets = @(
    @{
        Name = "renode-$RenodeVersion.windows-portable-dotnet.zip"
        Sha256 = "d09b7934cfd560cd06bde8f131ef78f521f10d423d5aac6096f2a583224aeb3e"
        Url = "https://github.com/renode/renode/releases/download/v$RenodeVersion/renode-$RenodeVersion.windows-portable-dotnet.zip"
        Kind = "renode"
    },
    @{
        Name = "qemu-xtensa-softmmu-$QemuAssetVersion-x86_64-w64-mingw32.tar.xz"
        Sha256 = "3c483d77f5350a568df1faf4d8dbc82c95d6bc2b826d0d4be910485e0a68ca2a"
        Url = "https://github.com/espressif/qemu/releases/download/$QemuTag/qemu-xtensa-softmmu-$QemuAssetVersion-x86_64-w64-mingw32.tar.xz"
        Kind = "qemu"
    },
    @{
        Name = "qemu-riscv32-softmmu-$QemuAssetVersion-x86_64-w64-mingw32.tar.xz"
        Sha256 = "697aa4800a1f52be0b1693b30e22a684f7ea93c46c489e619384cae7b0e9b87b"
        Url = "https://github.com/espressif/qemu/releases/download/$QemuTag/qemu-riscv32-softmmu-$QemuAssetVersion-x86_64-w64-mingw32.tar.xz"
        Kind = "qemu"
    }
)

if (-not $CacheDir) {
    $cacheBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
    $CacheDir = Join-Path $cacheBase "hauksbee-simulator-archives"
}
$renodeTarget = Join-Path $HOME "renode-portable"
$qemuTarget = Join-Path $HOME ".hauksbee-qemu-esp"
if ($RenodeOnly -and $QemuOnly) { throw "-RenodeOnly and -QemuOnly are mutually exclusive" }
$selectedAssets = @($assets | Where-Object {
    (-not $RenodeOnly -and -not $QemuOnly) -or
    ($RenodeOnly -and $_.Kind -eq "renode") -or
    ($QemuOnly -and $_.Kind -eq "qemu")
})

function Assert-Hash([string]$Path, [string]$Expected) {
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Expected) {
        throw "checksum mismatch for $Path`: expected $Expected, got $actual"
    }
}

function Get-PinnedAsset($Asset) {
    New-Item -ItemType Directory -Path $CacheDir -Force | Out-Null
    $path = Join-Path $CacheDir $Asset.Name
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        try {
            Assert-Hash $path $Asset.Sha256
            return $path
        } catch {
            $quarantine = "$path.corrupt-$([guid]::NewGuid().ToString('N'))"
            Move-Item -LiteralPath $path -Destination $quarantine
        }
    }
    $partial = "$path.partial-$PID"
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $Asset.Url -OutFile $partial
        Assert-Hash $partial $Asset.Sha256
        Move-Item -LiteralPath $partial -Destination $path
    } finally {
        if (Test-Path -LiteralPath $partial) {
            Remove-Item -LiteralPath $partial -Force
        }
    }
    return $path
}

function Assert-SafeTar([string]$Archive) {
    $entries = & tar -tf $Archive
    if ($LASTEXITCODE -ne 0) { throw "could not list archive $Archive" }
    foreach ($entry in $entries) {
        $normalized = $entry -replace '\\', '/'
        if ($normalized.StartsWith('/') -or $normalized -match '(^|/)\.\.(/|$)' -or $normalized -match '^[A-Za-z]:') {
            throw "unsafe archive entry in $Archive`: $entry"
        }
    }
}

function Recover-StaleBackup([string]$Target) {
    $parent = Split-Path -Parent $Target
    $leaf = Split-Path -Leaf $Target
    $backups = @(Get-ChildItem -LiteralPath $parent -Filter "$leaf.install-backup-*" -Directory -ErrorAction SilentlyContinue)
    if (Test-Path -LiteralPath $Target) {
        foreach ($stale in $backups) {
            Remove-Item -LiteralPath $stale.FullName -Recurse -Force
        }
        return
    }
    if ($backups.Count -gt 1) {
        throw "multiple interrupted-install backups exist for $Target; refusing to guess which is authoritative"
    }
    if ($backups.Count -eq 1) {
        Move-Item -LiteralPath $backups[0].FullName -Destination $Target
        Write-Warning "Recovered the previous simulator tree after an interrupted swap."
    }
}

function Replace-Tree([string]$Staging, [string]$Target) {
    $parent = Split-Path -Parent $Target
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    $lock = [IO.File]::Open("$Target.install.lock", [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try {
        Recover-StaleBackup $Target
        $backup = "$Target.install-backup-$([guid]::NewGuid().ToString('N'))"
        $movedOld = $false
        try {
            if (Test-Path -LiteralPath $Target) {
                Move-Item -LiteralPath $Target -Destination $backup
                $movedOld = $true
            }
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
        if ($movedOld -and (Test-Path -LiteralPath $backup)) {
            Remove-Item -LiteralPath $backup -Recurse -Force
        }
    } finally {
        $lock.Dispose()
    }
}

if ($Check) {
    foreach ($asset in $selectedAssets) {
        Assert-Hash (Join-Path $CacheDir $asset.Name) $asset.Sha256
    }
    if (-not $QemuOnly) {
        $renode = Get-ChildItem -LiteralPath $renodeTarget -Filter Renode.exe -File -Recurse | Select-Object -First 1
        if (-not $renode) { throw "pinned Renode is not installed" }
        & $renode.FullName --version | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "pinned Renode failed its version probe" }
    }
    if (-not $RenodeOnly) {
        foreach ($binary in @("qemu-system-xtensa.exe", "qemu-system-riscv32.exe")) {
            $output = & (Join-Path $qemuTarget "qemu\bin\$binary") -machine help 2>&1 | Out-String
            if ($LASTEXITCODE -ne 0 -or $output -notmatch '(?m)^esp32') {
                throw "$binary is not the Espressif fork"
            }
        }
    }
    Write-Host "Requested pinned Windows simulator tree(s) are present and executable."
    return
}

$downloaded = @{}
foreach ($asset in $selectedAssets) { $downloaded[$asset.Name] = Get-PinnedAsset $asset }

$renodeWork = Join-Path ([IO.Path]::GetTempPath()) "renode.install-staging-$([guid]::NewGuid().ToString('N'))"
$qemuWork = Join-Path ([IO.Path]::GetTempPath()) "qemu.install-staging-$([guid]::NewGuid().ToString('N'))"
try {
    if (-not $QemuOnly) {
        Expand-Archive -LiteralPath $downloaded[$assets[0].Name] -DestinationPath $renodeWork
        $renodeExe = Get-ChildItem -LiteralPath $renodeWork -Filter Renode.exe -File -Recurse | Select-Object -First 1
        if (-not $renodeExe) { throw "Renode.exe missing from the pinned Renode archive" }
        $renodeStage = "$renodeTarget.install-staging-$([guid]::NewGuid().ToString('N'))"
        if (Test-Path -LiteralPath $renodeStage) { Remove-Item -LiteralPath $renodeStage -Recurse -Force }
        New-Item -ItemType Directory -Path $renodeStage | Out-Null
        $top = @(Get-ChildItem -LiteralPath $renodeWork)
        $source = if ($top.Count -eq 1 -and $top[0].PSIsContainer) { $top[0].FullName } else { $renodeWork }
        Get-ChildItem -LiteralPath $source | Copy-Item -Destination $renodeStage -Recurse
        $stagedRenode = Get-ChildItem -LiteralPath $renodeStage -Filter Renode.exe -File -Recurse | Select-Object -First 1
        if (-not $stagedRenode) { throw "Renode.exe missing from the staged Renode tree" }
        & $stagedRenode.FullName --version | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "pinned Renode failed its version probe" }
        Replace-Tree $renodeStage $renodeTarget
    }

    if (-not $RenodeOnly) {
        New-Item -ItemType Directory -Path $qemuWork | Out-Null
        foreach ($asset in $assets | Where-Object { $_.Kind -eq "qemu" }) {
            $archive = $downloaded[$asset.Name]
            Assert-SafeTar $archive
            & tar -xf $archive -C $qemuWork
            if ($LASTEXITCODE -ne 0) { throw "could not extract $archive" }
        }
        foreach ($binary in @("qemu-system-xtensa.exe", "qemu-system-riscv32.exe")) {
            $output = & (Join-Path $qemuWork "qemu\bin\$binary") -machine help 2>&1 | Out-String
            if ($LASTEXITCODE -ne 0 -or $output -notmatch '(?m)^esp32') {
                throw "$binary is not the Espressif fork"
            }
        }
        $qemuStage = "$qemuTarget.install-staging-$([guid]::NewGuid().ToString('N'))"
        if (Test-Path -LiteralPath $qemuStage) { Remove-Item -LiteralPath $qemuStage -Recurse -Force }
        Move-Item -LiteralPath $qemuWork -Destination $qemuStage
        Replace-Tree $qemuStage $qemuTarget
    }
    Write-Host "Installed the requested checksum-pinned Windows simulator backend(s)."
} finally {
    foreach ($path in @($renodeWork, $qemuWork)) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Recurse -Force }
    }
}
