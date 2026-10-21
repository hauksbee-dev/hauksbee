<#
.SYNOPSIS
  Install a checksum-pinned cargo-nextest on a Windows CI runner.

.DESCRIPTION
  The native Windows tests are sharded with `cargo nextest --partition`, which
  needs the nextest binary. This downloads the upstream release zip for one
  pinned version, refuses it unless its SHA-256 matches the value recorded here,
  and places cargo-nextest.exe next to cargo so `cargo nextest` resolves.
  Bump $Version and $Sha256 together (the sha256 file ships next to each
  release asset on https://github.com/nextest-rs/nextest/releases).
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Version = "0.9.144"
$Sha256 = "edc18a0a36912d8add2a4856714aad8c128303c1de471f14716c2453318e719d"
$Asset = "cargo-nextest-$Version-x86_64-pc-windows-msvc.zip"
$Url = "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-$Version/$Asset"

$work = Join-Path ([System.IO.Path]::GetTempPath()) "hauksbee-nextest-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $work -Force | Out-Null
try {
    $zip = Join-Path $work $Asset
    $downloaded = $false
    foreach ($attempt in 1..3) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $zip
            $downloaded = $true
            break
        } catch {
            if ($attempt -eq 3) { throw }
            Start-Sleep -Seconds (5 * $attempt)
        }
    }
    if (-not $downloaded) { throw "could not download $Url" }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
    if ($actual -ne $Sha256) {
        throw "cargo-nextest $Version archive digest $actual does not match the pinned $Sha256"
    }
    $extract = Join-Path $work "x"
    Expand-Archive -LiteralPath $zip -DestinationPath $extract
    $exe = Get-ChildItem -LiteralPath $extract -Recurse -Filter "cargo-nextest.exe" | Select-Object -First 1
    if (-not $exe) { throw "cargo-nextest.exe missing from $Asset" }
    $cargoBin = Join-Path $(if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }) "bin"
    New-Item -ItemType Directory -Path $cargoBin -Force | Out-Null
    Copy-Item -LiteralPath $exe.FullName -Destination (Join-Path $cargoBin "cargo-nextest.exe") -Force
    & (Join-Path $cargoBin "cargo-nextest.exe") nextest --version
    if ($LASTEXITCODE -ne 0) { throw "installed cargo-nextest does not run" }
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
