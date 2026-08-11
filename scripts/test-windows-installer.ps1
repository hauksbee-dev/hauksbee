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
$assetName = Split-Path -Leaf $Zip
$checksumName = Split-Path -Leaf $Checksum
$testRoot = Join-Path ([IO.Path]::GetTempPath()) "hauksbee-installer-contract-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $testRoot | Out-Null

function Start-MockRelease([string]$ChecksumFile) {
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
    } -ArgumentList $prefix, $apiBase, "v$Version", $Zip, $ChecksumFile, $assetName, $checksumName
    Start-Sleep -Milliseconds 300
    return @{ Job = $job; ApiBase = $apiBase }
}

function Invoke-InstallerCase([string]$Prefix, [string]$ChecksumFile, [bool]$FailSwap) {
    $server = Start-MockRelease $ChecksumFile
    $env:HAUKSBEE_API_BASE = $server.ApiBase
    $env:HAUKSBEE_GITHUB_TOKEN = "installer-contract-token"
    if ($FailSwap) { $env:HAUKSBEE_TEST_FAIL_INSTALL_SWAP = "1" } else { Remove-Item Env:HAUKSBEE_TEST_FAIL_INSTALL_SWAP -ErrorAction SilentlyContinue }
    $stdout = Join-Path $testRoot "installer-$([guid]::NewGuid().ToString('N')).stdout"
    $stderr = "$stdout.stderr"
    try {
        $child = Start-Process -FilePath "pwsh" -ArgumentList @(
            "-NoProfile", "-File", $installer, "-Version", "v$Version", "-Prefix", $Prefix
        ) -Wait -PassThru -NoNewWindow -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        return $child.ExitCode
    } finally {
        Remove-Item Env:HAUKSBEE_API_BASE -ErrorAction SilentlyContinue
        Remove-Item Env:HAUKSBEE_GITHUB_TOKEN -ErrorAction SilentlyContinue
        Remove-Item Env:HAUKSBEE_TEST_FAIL_INSTALL_SWAP -ErrorAction SilentlyContinue
        Stop-Job $server.Job -ErrorAction SilentlyContinue
        Remove-Job $server.Job -Force -ErrorAction SilentlyContinue
    }
}

try {
    # Complete authenticated metadata, asset, checksum, extraction and install.
    $goodPrefix = Join-Path $testRoot "good"
    if ((Invoke-InstallerCase $goodPrefix $Checksum $false) -ne 0) { throw "valid installer flow failed" }
    foreach ($binary in @("hauksbee.exe", "hauksbee-ci.exe", "hauksbee-mcp.exe")) {
        $path = Join-Path $goodPrefix "bin\$binary"
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "$binary was not installed" }
        & $path --version | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "installed $binary did not execute" }
    }

    # A corrupt checksum must refuse before creating an installation.
    $corrupt = Join-Path $testRoot $checksumName
    [IO.File]::WriteAllText($corrupt, "$('0' * 64)  $assetName`r`n")
    $badPrefix = Join-Path $testRoot "corrupt"
    if ((Invoke-InstallerCase $badPrefix $corrupt $false) -eq 0) { throw "corrupt checksum was accepted" }
    if (Test-Path -LiteralPath (Join-Path $badPrefix "bin")) { throw "corrupt checksum modified the prefix" }

    # A failure after backing up the old tree must rollback it byte-for-byte.
    $rollbackPrefix = Join-Path $testRoot "rollback"
    $oldBin = Join-Path $rollbackPrefix "bin"
    New-Item -ItemType Directory -Path $oldBin -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $oldBin "old-install.marker"), "old-version")
    if ((Invoke-InstallerCase $rollbackPrefix $Checksum $true) -eq 0) { throw "injected rollback failure unexpectedly succeeded" }
    if ((Get-Content -LiteralPath (Join-Path $oldBin "old-install.marker") -Raw) -ne "old-version") {
        throw "rollback did not restore the previous installation"
    }
    Write-Host "Windows installer end-to-end, corrupt checksum, and rollback contracts passed."
} finally {
    foreach ($name in @("HAUKSBEE_API_BASE", "HAUKSBEE_GITHUB_TOKEN", "HAUKSBEE_TEST_FAIL_INSTALL_SWAP")) {
        Remove-Item "Env:$name" -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $testRoot) { Remove-Item -LiteralPath $testRoot -Recurse -Force }
}
