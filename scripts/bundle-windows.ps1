<#
.SYNOPSIS
  Package the native Windows permissive release into a verified zip.

.DESCRIPTION
  Windows ships one honest shape: renode+qemu with AVR disabled.  libsimavr has
  no supported native MSVC build in this repository, so this script refuses a
  binary that reports AVR as builtin instead of pretending feature parity.
  The release workflow builds the three binaries and calls this packager.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-f]{40}$')]
    [string]$ExpectedCommit,
    [string]$TargetDir = "target\release",
    [string]$Out = "dist",
    [switch]$RequireAuthenticodeSignature
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if ($Version -notmatch '^[0-9A-Za-z._-]+$') {
    throw "Refusing suspicious version '$Version'."
}
$targetPath = if ([IO.Path]::IsPathRooted($TargetDir)) {
    $TargetDir
} else {
    Join-Path $repoRoot $TargetDir
}
$outPath = if ([IO.Path]::IsPathRooted($Out)) {
    $Out
} else {
    Join-Path $repoRoot $Out
}
$base = "hauksbee-$Version-windows-x86_64-permissive"
$requiredBinaries = @("hauksbee.exe", "hauksbee-ci.exe", "hauksbee-mcp.exe")

function Resolve-SignTool {
    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Path
    }

    $kitsRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    if (Test-Path -LiteralPath $kitsRoot -PathType Container) {
        $candidate = Get-ChildItem -LiteralPath $kitsRoot -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^10\.0\.[0-9.]+$' } |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName "x64\signtool.exe" } |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
            Select-Object -First 1
        if ($null -ne $candidate) {
            return $candidate
        }
    }
    throw "Authenticode release signing requires signtool.exe (Windows SDK) on PATH or under '$kitsRoot'."
}

function Sign-And-VerifyBinaries([string]$BinDir, [string]$WorkDir) {
    if (-not $RequireAuthenticodeSignature) {
        return
    }

    $pfxBase64 = [Environment]::GetEnvironmentVariable("HAUKSBEE_WINDOWS_SIGNING_PFX_BASE64")
    $pfxPassword = [Environment]::GetEnvironmentVariable("HAUKSBEE_WINDOWS_SIGNING_PFX_PASSWORD")
    $timestampUrl = [Environment]::GetEnvironmentVariable("HAUKSBEE_WINDOWS_SIGNING_TIMESTAMP_URL")
    if ([string]::IsNullOrWhiteSpace($pfxBase64)) {
        throw "Release Authenticode signing is required, but HAUKSBEE_WINDOWS_SIGNING_PFX_BASE64 is missing."
    }
    if ([string]::IsNullOrWhiteSpace($pfxPassword)) {
        throw "Release Authenticode signing is required, but HAUKSBEE_WINDOWS_SIGNING_PFX_PASSWORD is missing."
    }
    if ([string]::IsNullOrWhiteSpace($timestampUrl)) {
        $timestampUrl = "http://timestamp.digicert.com"
    }

    $signTool = Resolve-SignTool
    $pfxPath = Join-Path $WorkDir "hauksbee-signing.pfx"
    try {
        try {
            [IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($pfxBase64))
        } catch {
            throw "HAUKSBEE_WINDOWS_SIGNING_PFX_BASE64 is not valid base64: $($_.Exception.Message)"
        }
        if ((Get-Item -LiteralPath $pfxPath).Length -eq 0) {
            throw "HAUKSBEE_WINDOWS_SIGNING_PFX_BASE64 decoded to an empty PFX."
        }

        foreach ($binary in $requiredBinaries) {
            $path = Join-Path $BinDir $binary
            & $signTool sign /q /fd SHA256 /f $pfxPath /p $pfxPassword /tr $timestampUrl /td SHA256 /d "Hauksbee $Version" $path | Out-Host
            $signExitCode = $LASTEXITCODE
            if ($signExitCode -ne 0) {
                throw "signtool failed to sign $binary (exit code $signExitCode)."
            }
            & $signTool verify /q /pa /all $path | Out-Host
            $verifyExitCode = $LASTEXITCODE
            if ($verifyExitCode -ne 0) {
                throw "signtool failed to verify the Authenticode signature on $binary (exit code $verifyExitCode)."
            }
        }
    } finally {
        if (Test-Path -LiteralPath $pfxPath) {
            Remove-Item -LiteralPath $pfxPath -Force -ErrorAction SilentlyContinue
        }
    }
}

function Assert-BinaryVersion([string]$Path, [string]$Name, [string]$ExpectedVersion) {
    $output = (& $Path --version 2>&1 | Out-String).Trim()
    $exitCode = $LASTEXITCODE
    $escapedName = [regex]::Escape($Name -replace '\.exe$', '')
    $escapedVersion = [regex]::Escape($ExpectedVersion)
    $escapedCommit = [regex]::Escape($ExpectedCommit)
    if ($exitCode -ne 0 -or $output -notmatch "(?m)^$escapedName $escapedVersion \(git $escapedCommit\)$") {
        throw "$Name does not identify release version $ExpectedVersion`: $output"
    }
}
foreach ($binary in $requiredBinaries) {
    $path = Join-Path $targetPath $binary
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Required release binary is missing: $path"
    }
    Assert-BinaryVersion $path $binary $Version
}

# Ask the binary that will be packaged. A filename is not evidence that the
# GPL AVR backend stayed out of the Windows artifact.
$doctor = & (Join-Path $targetPath "hauksbee.exe") doctor 2>&1 | Out-String
# `doctor` exits zero even when an external backend is absent (normal on a
# bare release runner) and prints one tab-separated `name status detail` row
# per backend. The compile-time AVR row is the shape assertion.
if ($doctor -notmatch '(?m)^avr\s+disabled\b') {
    throw "Windows releases are permissive-only, but hauksbee doctor did not report 'avr disabled':`n$doctor"
}

New-Item -ItemType Directory -Path $outPath -Force | Out-Null
$work = Join-Path ([IO.Path]::GetTempPath()) "hauksbee-windows-$([IO.Path]::GetRandomFileName())"
$rootDir = Join-Path $work $base
$binDir = Join-Path $rootDir "bin"
New-Item -ItemType Directory -Path $binDir -Force | Out-Null

try {
    foreach ($binary in $requiredBinaries) {
        Copy-Item -LiteralPath (Join-Path $targetPath $binary) -Destination (Join-Path $binDir $binary)
    }

    # Ordinary local bundles remain unsigned. Release workflow calls this
    # script with -RequireAuthenticodeSignature, which fails closed if the
    # documented PFX/password secrets or signtool are unavailable. Signing the
    # staged copies before Compress-Archive keeps the zip and its checksum
    # bound to the verified Authenticode payloads.
    Sign-And-VerifyBinaries $binDir $work

    # Same payload layout as scripts/bundle.sh: the model database lives under
    # crates/hauksbee-models/db in the tree and ships as db/ in the bundle.
    foreach ($item in @(
        @{ Source = "crates\hauksbee-models\db"; Name = "db" },
        @{ Source = "examples"; Name = "examples" },
        @{ Source = "integrations"; Name = "integrations" }
    )) {
        Copy-Item -LiteralPath (Join-Path $repoRoot $item.Source) -Destination (Join-Path $rootDir $item.Name) -Recurse
    }
    $ciSpecs = Join-Path $rootDir "examples\ci-specs"
    New-Item -ItemType Directory -Path $ciSpecs -Force | Out-Null
    Copy-Item -Path (Join-Path $repoRoot "crates\hauksbee-ci\examples\*") -Destination $ciSpecs -Recurse
    foreach ($privateSpec in @(
        "tarski_brownout.toml",
        "tarski_brownout_repaired.toml",
        "watchy_v15_display_res.toml",
        "watchy_v15_display_res_undriven.toml",
        "pic_programmer_schematic.toml",
        "olimex_wifi_burst_transient.toml",
        "boot_gate_pass.toml"
    )) {
        Remove-Item -LiteralPath (Join-Path $ciSpecs $privateSpec) -Force -ErrorAction SilentlyContinue
    }
    # Windows is permissive-only. Do not ship checkout/corpus-relative specs
    # whose boards or AVR firmware are absent from this archive.
    $scriptsDir = Join-Path $rootDir "scripts"
    New-Item -ItemType Directory -Path $scriptsDir | Out-Null
    foreach ($script in @("get-hauksbee.ps1", "install-sims-windows.ps1")) {
        Copy-Item -LiteralPath (Join-Path $repoRoot "scripts\$script") -Destination $scriptsDir
    }
    foreach ($item in @("LICENSE", "NOTICE")) {
        Copy-Item -LiteralPath (Join-Path $repoRoot $item) -Destination $rootDir
    }
    Copy-Item -LiteralPath (Join-Path $repoRoot "licenses\\evalexpr-MIT.txt") `
        -Destination (Join-Path $rootDir "LICENSE-EVALEXPR-MIT.txt")

    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [IO.File]::WriteAllText(
        (Join-Path $rootDir "LICENSE-BINARY.txt"),
        "Apache-2.0 binary`r`n`r`nThis Windows x86_64 build disables the GPL-3.0 AVR/libsimavr backend. Renode and Espressif QEMU support remain compiled in; availability is reported by hauksbee doctor.`r`nevalexpr 11.3.1 is MIT-licensed; retain LICENSE-EVALEXPR-MIT.txt.`r`n",
        $utf8
    )
    [IO.File]::WriteAllText((Join-Path $rootDir "VERSION"), "$Version`r`n", $utf8)
    [IO.File]::WriteAllText(
        (Join-Path $rootDir "README-BUNDLE.txt"),
        "Hauksbee $Version for Windows x86_64 (permissive shape).`r`nRun bin\hauksbee.exe --help. For another authenticated install, invoke scripts\get-hauksbee.ps1 with this release tag and exact source commit from the immutable release notes.`r`nInstall the pinned external emulators with scripts\install-sims-windows.ps1.`r`nAVR co-simulation is not compiled into this artifact; use a supported Unix default bundle or build libsimavr under MSYS2 to unlock it.`r`n",
        $utf8
    )

    $zipPath = Join-Path $outPath "$base.zip"
    $checksumPath = "$zipPath.sha256"
    if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
    if (Test-Path -LiteralPath $checksumPath) { Remove-Item -LiteralPath $checksumPath -Force }
    Compress-Archive -LiteralPath $rootDir -DestinationPath $zipPath -CompressionLevel Optimal
    $digest = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText($checksumPath, "$digest  $base.zip`r`n", $utf8)
    Write-Host "Wrote $zipPath"
    Write-Host "Wrote $checksumPath"
} finally {
    if (Test-Path -LiteralPath $work) {
        Remove-Item -LiteralPath $work -Recurse -Force
    }
}
