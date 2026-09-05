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
    [string]$Out = "dist"
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
# `doctor` exits nonzero when an external backend is absent, which is normal on
# a bare release runner. The compile-time AVR line is the shape assertion.
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
    foreach ($item in @("db", "examples", "integrations")) {
        Copy-Item -LiteralPath (Join-Path $repoRoot $item) -Destination (Join-Path $rootDir $item) -Recurse
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
