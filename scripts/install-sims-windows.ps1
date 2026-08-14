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
    [string]$EvidenceOut = "",
    [switch]$Check,
    [switch]$RenodeOnly,
    [switch]$QemuOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
# StrictMode treats this automatic variable as absent until the first native
# process sets it. Initialise it so a successfully invoked command that leaves
# it untouched cannot make the verification path fail while reading the status.
$global:LASTEXITCODE = 0

$RenodeVersion = "1.16.1"
$QemuTag = "esp-develop-9.2.2-20260417"
$QemuAssetVersion = "esp_develop_9.2.2_20260417"
$RenodeArtifactSha256 = "895fddb36f65237af5a47928e49984cf1e1992e27e0d37546b3b8ea29ad57385"
$RenodeInstallTreeSha256 = "3b12f1dd7b613cd9b73994a985fcd77107f471c352c52b4f3f2ff1528d4e7e8d"
$QemuXtensaArtifactSha256 = "7716f734130a20193ab45a4c14581918822e5ae684eb5cf3073b9429bee29825"
$QemuRiscv32ArtifactSha256 = "ec900387a3f7b54800d4690db575b86162769add55aa3b09056a943b29ec6644"
$QemuInstallTreeSha256 = "4f02f4495f50ddf3baed71de29192932bd09053f0a1df498b854e0f5be0d8171"
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

function Get-InstallTreeSha256([string]$Root) {
    $rootPath = (Resolve-Path -LiteralPath $Root).Path.TrimEnd([char[]]"\/")
    $rows = New-Object System.Collections.Generic.List[string]
    foreach ($file in Get-ChildItem -LiteralPath $rootPath -File -Recurse) {
        $relative = $file.FullName.Substring($rootPath.Length).TrimStart([char[]]"\/") -replace '\\', '/'
        $digest = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $rows.Add("$digest  $relative`n")
    }
    $ordered = $rows.ToArray()
    [Array]::Sort($ordered, [StringComparer]::Ordinal)
    $bytes = [Text.Encoding]::UTF8.GetBytes([string]::Concat($ordered))
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

function Assert-InstallTree([string]$Actual, [string]$Expected, [string]$Label) {
    $actualDigest = Get-InstallTreeSha256 $Actual
    $expectedDigest = Get-InstallTreeSha256 $Expected
    if ($actualDigest -ne $expectedDigest) {
        throw "$Label install tree does not match the freshly extracted pinned archive payload"
    }
    return @{ Actual = $actualDigest; Expected = $expectedDigest }
}

function Assert-RenodeVersion([string]$Path) {
    $output = & $Path --version 2>&1 | Select-Object -First 1 | Out-String
    if ($LASTEXITCODE -ne 0 -or $output.Trim() -notmatch "^(Renode v|Renode, version )$([regex]::Escape($RenodeVersion))(?:\.|\s|$)") {
        throw "Renode payload reports the wrong version: $($output.Trim())"
    }
}

function Assert-QemuVersion([string]$Path) {
    $version = & $Path --version 2>&1 | Select-Object -First 1 | Out-String
    if ($LASTEXITCODE -ne 0 -or $version -notmatch "\($([regex]::Escape($QemuAssetVersion))\)") {
        throw "QEMU payload reports the wrong pinned version: $($version.Trim())"
    }
    $machines = & $Path -machine help 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0 -or $machines -notmatch '(?m)^esp32') {
        throw "$(Split-Path -Leaf $Path) is not the Espressif fork"
    }
}

function New-VerifiedSnapshot($Asset, [string]$Path) {
    $snapshot = Join-Path ([IO.Path]::GetTempPath()) "$($Asset.Name).snapshot-$([guid]::NewGuid().ToString('N'))"
    $complete = $false
    try {
        Copy-Item -LiteralPath $Path -Destination $snapshot
        Assert-Hash $snapshot $Asset.Sha256
        $complete = $true
        return $snapshot
    } finally {
        if (-not $complete -and (Test-Path -LiteralPath $snapshot)) {
            Remove-Item -LiteralPath $snapshot -Force
        }
    }
}

function Get-PinnedAsset($Asset) {
    New-Item -ItemType Directory -Path $CacheDir -Force | Out-Null
    $path = Join-Path $CacheDir $Asset.Name
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        try {
            Assert-Hash $path $Asset.Sha256
            return New-VerifiedSnapshot $Asset $path
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
    return New-VerifiedSnapshot $Asset $path
}

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

function Assert-SafeTar([string]$Archive) {
    $entries = & tar -tf $Archive
    if ($LASTEXITCODE -ne 0) { throw "could not list archive $Archive" }
    foreach ($entry in $entries) {
        $normalized = $entry -replace '\\', '/'
        if ($normalized.StartsWith('/') -or $normalized -match '(^|/)\.\.(/|$)' -or $normalized -match '^[A-Za-z]:') {
            throw "unsafe archive entry in $Archive`: $entry"
        }
    }
    # `tar -tf` prints names only. Verbose mode exposes the leading Unix type
    # character; release payloads must contain regular files/directories only,
    # never links, devices, sockets, or FIFOs whose extraction semantics could
    # escape or mutate outside the selected staging root.
    $verbose = & tar -tvf $Archive
    if ($LASTEXITCODE -ne 0) { throw "could not inspect archive metadata $Archive" }
    foreach ($line in $verbose) {
        if ($line -and $line[0] -notin @('-', 'd')) {
            throw "unsafe archive member type in $Archive`: $line"
        }
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
        Write-Warning "Rolled back a simulator install interrupted before final acceptance."
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

if ($Check) {
    $checkAssets = @{}
    $verifyRoot = Join-Path ([IO.Path]::GetTempPath()) "simulator.verify-$([guid]::NewGuid().ToString('N'))"
    $evidence = [ordered]@{ backends = [ordered]@{} }
    try {
        foreach ($asset in $selectedAssets) {
            $cached = Join-Path $CacheDir $asset.Name
            Assert-Hash $cached $asset.Sha256
            $snapshot = New-VerifiedSnapshot $asset $cached
            $checkAssets[$asset.Name] = $snapshot
        }
        New-Item -ItemType Directory -Path $verifyRoot | Out-Null
        if (-not $QemuOnly) {
            $rawRenode = Join-Path $verifyRoot "renode-raw"
            $expectedRenode = Join-Path $verifyRoot "renode"
            $renodeArchive = $checkAssets[$assets[0].Name]
            Assert-SafeZip $renodeArchive
            Expand-Archive -LiteralPath $renodeArchive -DestinationPath $rawRenode
            $top = @(Get-ChildItem -LiteralPath $rawRenode)
            $source = if ($top.Count -eq 1 -and $top[0].PSIsContainer) { $top[0].FullName } else { $rawRenode }
            New-Item -ItemType Directory -Path $expectedRenode | Out-Null
            Get-ChildItem -LiteralPath $source | Copy-Item -Destination $expectedRenode -Recurse
            $renode = Get-ChildItem -LiteralPath $renodeTarget -Filter Renode.exe -File -Recurse | Select-Object -First 1
            $expectedExe = Get-ChildItem -LiteralPath $expectedRenode -Filter Renode.exe -File -Recurse | Select-Object -First 1
            if (-not $renode -or -not $expectedExe) { throw "pinned Renode executable is missing" }
            Assert-RenodeVersion $renode.FullName
            $tree = Assert-InstallTree $renodeTarget $expectedRenode "Renode"
            $artifactSha256 = (Get-FileHash -LiteralPath $renode.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($artifactSha256 -ne $RenodeArtifactSha256 -or $tree.Actual -ne $RenodeInstallTreeSha256) {
                throw "Renode payload does not match the repository-pinned executable and install-tree fingerprints"
            }
            $evidence.backends["HAUKSBEE_RENODE"] = [ordered]@{
                artifact_sha256 = $artifactSha256
                install_tree_sha256 = $tree.Actual
            }
        }
        if (-not $RenodeOnly) {
            $expectedQemu = Join-Path $verifyRoot "qemu"
            New-Item -ItemType Directory -Path $expectedQemu | Out-Null
            foreach ($asset in $assets | Where-Object { $_.Kind -eq "qemu" }) {
                $archive = $checkAssets[$asset.Name]
                Assert-SafeTar $archive
                & tar -xf $archive -C $expectedQemu
                if ($LASTEXITCODE -ne 0) { throw "could not extract $archive for verification" }
            }
            $tree = Assert-InstallTree $qemuTarget $expectedQemu "Espressif QEMU"
            if ($tree.Actual -ne $QemuInstallTreeSha256) {
                throw "Espressif QEMU payload does not match the repository-pinned install-tree fingerprint"
            }
            foreach ($contract in @(
                @{ Key = "HAUKSBEE_QEMU_XTENSA"; File = "qemu-system-xtensa.exe"; Sha256 = $QemuXtensaArtifactSha256 },
                @{ Key = "HAUKSBEE_QEMU_RISCV32"; File = "qemu-system-riscv32.exe"; Sha256 = $QemuRiscv32ArtifactSha256 }
            )) {
                $actualExe = Join-Path $qemuTarget "qemu\bin\$($contract.File)"
                Assert-QemuVersion $actualExe
                $artifactSha256 = (Get-FileHash -LiteralPath $actualExe -Algorithm SHA256).Hash.ToLowerInvariant()
                if ($artifactSha256 -ne $contract.Sha256) {
                    throw "$($contract.File) does not match the repository-pinned executable fingerprint"
                }
                $evidence.backends[$contract.Key] = [ordered]@{
                    artifact_sha256 = $artifactSha256
                    install_tree_sha256 = $tree.Actual
                }
            }
        }
        if ($EvidenceOut) {
            $parent = Split-Path -Parent $EvidenceOut
            if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
            $evidence | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $EvidenceOut -Encoding utf8
        }
        Write-Host "Requested pinned Windows simulator tree(s) match fresh verified archive extraction."
        return
    } finally {
        Remove-AbandonedStaging $verifyRoot
        foreach ($snapshot in $checkAssets.Values) { Remove-Item -LiteralPath $snapshot -Force -ErrorAction SilentlyContinue }
    }
}

$downloaded = @{}
$renodeWork = Join-Path ([IO.Path]::GetTempPath()) "renode.install-staging-$([guid]::NewGuid().ToString('N'))"
$qemuWork = Join-Path ([IO.Path]::GetTempPath()) "qemu.install-staging-$([guid]::NewGuid().ToString('N'))"
$renodeStage = $null
$qemuStage = $null
try {
    $transactions = @()
    foreach ($asset in $selectedAssets) { $downloaded[$asset.Name] = Get-PinnedAsset $asset }
    if (-not $QemuOnly) {
        Assert-SafeZip $downloaded[$assets[0].Name]
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
        Assert-RenodeVersion $stagedRenode.FullName
        $transactions += ,(Replace-Tree $renodeStage $renodeTarget)
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
            Assert-QemuVersion (Join-Path $qemuWork "qemu\bin\$binary")
        }
        $qemuStage = "$qemuTarget.install-staging-$([guid]::NewGuid().ToString('N'))"
        if (Test-Path -LiteralPath $qemuStage) { Remove-Item -LiteralPath $qemuStage -Recurse -Force }
        Move-Item -LiteralPath $qemuWork -Destination $qemuStage
        $transactions += ,(Replace-Tree $qemuStage $qemuTarget)
    }
    if (-not $QemuOnly) {
        $installedRenode = Get-ChildItem -LiteralPath $renodeTarget -Filter Renode.exe -File -Recurse | Select-Object -First 1
        if (-not $installedRenode) { throw "Renode.exe missing after committed install" }
        Assert-RenodeVersion $installedRenode.FullName
    }
    if (-not $RenodeOnly) {
        foreach ($binary in @("qemu-system-xtensa.exe", "qemu-system-riscv32.exe")) {
            Assert-QemuVersion (Join-Path $qemuTarget "qemu\bin\$binary")
        }
    }
    # Persist acceptance for every tree before discarding any backup. If the
    # process dies during cleanup, recovery keeps the accepted target when it
    # sees this marker instead of silently rolling it back on the next run.
    foreach ($transaction in $transactions) { Mark-TreeReplaceCommitted $transaction }
    foreach ($transaction in $transactions) { Complete-TreeReplace $transaction }
    $transactions = @()
    Write-Host "Installed the requested checksum-pinned Windows simulator backend(s)."
} finally {
    [array]::Reverse($transactions)
    foreach ($transaction in $transactions) { Undo-TreeReplace $transaction }
    foreach ($path in @($renodeStage, $qemuStage, $renodeWork, $qemuWork)) {
        Remove-AbandonedStaging $path
    }
    foreach ($snapshot in $downloaded.Values) { Remove-Item -LiteralPath $snapshot -Force -ErrorAction SilentlyContinue }
}
