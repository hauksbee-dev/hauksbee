<# Native end-to-end contract for get-hauksbee.ps1 against a local private API. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Zip,
    [Parameter(Mandatory = $true)]
    [string]$Checksum,
    [Parameter(Mandatory = $true)]
    [string]$Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$installer = Join-Path $PSScriptRoot "get-hauksbee.ps1"
$Zip = (Resolve-Path $Zip).Path
$Checksum = (Resolve-Path $Checksum).Path
$testRoot = Join-Path ([IO.Path]::GetTempPath()) "hauksbee-installer-contract-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $testRoot | Out-Null

function Start-MockRelease([string]$ZipFile, [string]$ChecksumFile) {
    $zipName = Split-Path -Leaf $ZipFile
    $checksumName = Split-Path -Leaf $ChecksumFile
    $probe = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $probe.Start()
    $port = ([Net.IPEndPoint]$probe.LocalEndpoint).Port
    $probe.Stop()
    $prefix = "http://127.0.0.1:$port/"
    $apiBase = "${prefix}repos/hauksbee-dev/hauksbee"
    $job = Start-Job -ScriptBlock {
        param($Prefix, $ApiBase, $Tag, $ZipPath, $SumPath, $ZipName, $SumName)
        $listener = [Net.HttpListener]::new()
        $listener.Prefixes.Add($Prefix)
        $listener.Start()
        try {
            for ($served = 0; $served -lt 3; $served++) {
                $context = $listener.GetContext()
                $request = $context.Request
                $response = $context.Response
                if ($request.Headers["Authorization"] -ne "Bearer installer-contract-token") {
                    $response.StatusCode = 401
                    $response.Close()
                    continue
                }
                switch -Regex ($request.Url.AbsolutePath) {
                    "/releases/tags/" {
                        $body = [Text.Encoding]::UTF8.GetBytes((@{
                            tag_name = $Tag
                            assets = @(
                                @{ name = $ZipName; url = "$ApiBase/releases/assets/1" },
                                @{ name = $SumName; url = "$ApiBase/releases/assets/2" }
                            )
                        } | ConvertTo-Json -Depth 4 -Compress))
                        $response.ContentType = "application/json"
                    }
                    "/releases/assets/1$" { $body = [IO.File]::ReadAllBytes($ZipPath) }
                    "/releases/assets/2$" { $body = [IO.File]::ReadAllBytes($SumPath) }
                    default {
                        $response.StatusCode = 404
                        $body = [byte[]]::new(0)
                    }
                }
                $response.ContentLength64 = $body.Length
                $response.OutputStream.Write($body, 0, $body.Length)
                $response.Close()
            }
        } finally {
            $listener.Stop()
            $listener.Close()
        }
    } -ArgumentList $prefix, $apiBase, "v$Version", $ZipFile, $ChecksumFile, $zipName, $checksumName
    Start-Sleep -Milliseconds 300
    return @{ Job = $job; ApiBase = $apiBase }
}

function Invoke-InstallerCase(
    [string]$Prefix,
    [string]$ZipFile,
    [string]$ChecksumFile,
    [bool]$FailSwap,
    [string]$PowerShell
) {
    $server = Start-MockRelease $ZipFile $ChecksumFile
    $env:HAUKSBEE_API_BASE = $server.ApiBase
    $env:HAUKSBEE_GITHUB_TOKEN = "installer-contract-token"
    $env:GITHUB_TOKEN = "fallback-contract-token"
    if ($FailSwap) { $env:HAUKSBEE_TEST_FAIL_INSTALL_SWAP = "1" } else { Remove-Item Env:HAUKSBEE_TEST_FAIL_INSTALL_SWAP -ErrorAction SilentlyContinue }
    $stdout = Join-Path $testRoot "installer-$([guid]::NewGuid().ToString('N')).stdout"
    $stderr = "$stdout.stderr"
    try {
        $child = Start-Process -FilePath $PowerShell -ArgumentList @(
            "-NoProfile", "-File", $installer, "-Version", "v$Version", "-Prefix", $Prefix
        ) -Wait -PassThru -NoNewWindow -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        return $child.ExitCode
    } finally {
        Remove-Item Env:HAUKSBEE_API_BASE -ErrorAction SilentlyContinue
        Remove-Item Env:HAUKSBEE_GITHUB_TOKEN -ErrorAction SilentlyContinue
        Remove-Item Env:GITHUB_TOKEN -ErrorAction SilentlyContinue
        Remove-Item Env:HAUKSBEE_TEST_FAIL_INSTALL_SWAP -ErrorAction SilentlyContinue
        Stop-Job $server.Job -ErrorAction SilentlyContinue
        Remove-Job $server.Job -Force -ErrorAction SilentlyContinue
    }
}

function New-TokenProbeBundle() {
    $base = "hauksbee-$Version-windows-x86_64-permissive"
    $fixtureRoot = Join-Path $testRoot "token-probe"
    $bundleRoot = Join-Path $fixtureRoot $base
    $bin = Join-Path $bundleRoot "bin"
    New-Item -ItemType Directory -Path $bin -Force | Out-Null
    $source = Join-Path $fixtureRoot "token_probe.rs"
    [IO.File]::WriteAllText($source, @"
use std::env;
use std::process;

fn main() {
    if env::var_os("HAUKSBEE_GITHUB_TOKEN").is_some()
        || env::var_os("GITHUB_TOKEN").is_some()
    {
        process::exit(91);
    }
    if env::args_os().skip(1).collect::<Vec<_>>() != ["--version"] {
        process::exit(2);
    }
    let path = env::current_exe().expect("current executable");
    let name = path.file_stem().expect("executable stem").to_string_lossy();
    println!("{} $Version", name);
}
"@)
    $probe = Join-Path $fixtureRoot "token-probe.exe"
    & rustc $source -o $probe
    if ($LASTEXITCODE -ne 0) { throw "could not compile token-isolation probe" }
    foreach ($binary in @("hauksbee.exe", "hauksbee-ci.exe", "hauksbee-mcp.exe")) {
        Copy-Item -LiteralPath $probe -Destination (Join-Path $bin $binary)
    }
    $zip = Join-Path $testRoot "$base.zip"
    Compress-Archive -LiteralPath $bundleRoot -DestinationPath $zip
    $sum = "$zip.sha256"
    $digest = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText($sum, "$digest  $base.zip`r`n")
    return @{ Zip = $zip; Checksum = $sum }
}

try {
    # Complete authenticated metadata, asset, checksum, extraction and install.
    foreach ($runtime in @("powershell.exe", "pwsh.exe")) {
        $goodPrefix = Join-Path $testRoot "good-$($runtime.Replace('.', '-'))"
        if ((Invoke-InstallerCase $goodPrefix $Zip $Checksum $false $runtime) -ne 0) { throw "valid installer flow failed under $runtime" }
        foreach ($binary in @("hauksbee.exe", "hauksbee-ci.exe", "hauksbee-mcp.exe")) {
            $path = Join-Path $goodPrefix "bin\$binary"
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "$binary was not installed" }
            & $path --version | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "installed $binary did not execute" }
        }
    }

    # Downloaded executables must not inherit either private API token during
    # their probe. The fixture exits 91 if either variable reaches it, and the
    # same authenticated flow runs under stock Windows PowerShell 5.1 + pwsh.
    $tokenProbe = New-TokenProbeBundle
    foreach ($runtime in @("powershell.exe", "pwsh.exe")) {
        $prefix = Join-Path $testRoot "token-free-$($runtime.Replace('.', '-'))"
        if ((Invoke-InstallerCase $prefix $tokenProbe.Zip $tokenProbe.Checksum $false $runtime) -ne 0) {
            throw "token-free executable probe failed under $runtime"
        }
    }

    # A corrupt checksum must refuse before creating an installation.
    $checksumName = Split-Path -Leaf $Checksum
    $corrupt = Join-Path $testRoot $checksumName
    $assetName = Split-Path -Leaf $Zip
    [IO.File]::WriteAllText($corrupt, "$('0' * 64)  $assetName`r`n")
    $badPrefix = Join-Path $testRoot "corrupt"
    if ((Invoke-InstallerCase $badPrefix $Zip $corrupt $false "pwsh.exe") -eq 0) { throw "corrupt checksum was accepted" }
    if (Test-Path -LiteralPath (Join-Path $badPrefix "bin")) { throw "corrupt checksum modified the prefix" }

    # A failure after backing up the old tree must rollback it byte-for-byte.
    $rollbackPrefix = Join-Path $testRoot "rollback"
    $oldBin = Join-Path $rollbackPrefix "bin"
    New-Item -ItemType Directory -Path $oldBin -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $oldBin "old-install.marker"), "old-version")
    if ((Invoke-InstallerCase $rollbackPrefix $Zip $Checksum $true "pwsh.exe") -eq 0) { throw "injected rollback failure unexpectedly succeeded" }
    if ((Get-Content -LiteralPath (Join-Path $oldBin "old-install.marker") -Raw) -ne "old-version") {
        throw "rollback did not restore the previous installation"
    }

    # Simulate interruption after the old tree moved but before the new tree
    # arrived. A subsequent locked transaction must recover that sole stale
    # backup before attempting its own swap and rollback.
    $stalePrefix = Join-Path $testRoot "stale-interruption"
    $staleBackup = Join-Path $stalePrefix "bin.install-backup-stale-interruption"
    New-Item -ItemType Directory -Path $staleBackup -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $staleBackup "old-install.marker"), "interrupted-old-version")
    if ((Invoke-InstallerCase $stalePrefix $Zip $Checksum $true "pwsh.exe") -eq 0) { throw "stale-interruption rollback unexpectedly succeeded" }
    if ((Get-Content -LiteralPath (Join-Path $stalePrefix "bin\old-install.marker") -Raw) -ne "interrupted-old-version") {
        throw "stale-interruption backup was not recovered"
    }
    Write-Host "Windows installer dual-runtime, token isolation, checksum, rollback, and interruption contracts passed."
} finally {
    foreach ($name in @("HAUKSBEE_API_BASE", "HAUKSBEE_GITHUB_TOKEN", "GITHUB_TOKEN", "HAUKSBEE_TEST_FAIL_INSTALL_SWAP")) {
        Remove-Item "Env:$name" -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $testRoot) { Remove-Item -LiteralPath $testRoot -Recurse -Force }
}
