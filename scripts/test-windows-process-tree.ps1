<# Native regression: a timed-out Cargo-like parent may not leave a grandchild. #>
[CmdletBinding()]
param(
    [switch]$ChildMode,
    [string]$Marker = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($ChildMode) {
    $grandchild = Start-Process -FilePath "powershell.exe" -ArgumentList @(
        "-NoProfile", "-Command", "Start-Sleep -Seconds 300"
    ) -PassThru
    [IO.File]::WriteAllText($Marker, [string]$grandchild.Id)
    Wait-Process -Id $grandchild.Id
    return
}

. (Join-Path $PSScriptRoot "windows-owned-process.ps1")

$scratch = Join-Path ([IO.Path]::GetTempPath()) "hauksbee job test $([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $scratch | Out-Null
$markerPath = Join-Path $scratch "grandchild.pid"
try {
    $result = Invoke-HauksbeeJobProcess -FilePath "powershell.exe" -ArgumentList @(
        "-NoProfile", "-File", $PSCommandPath, "-ChildMode", "-Marker", $markerPath
    ) -WorkingDirectory $PSScriptRoot -StandardOutput (Join-Path $scratch "stdout.log") `
        -StandardError (Join-Path $scratch "stderr.log") -TimeoutMilliseconds 5000
    if (-not $result.TimedOut -or $result.ExitCode -ne 124) {
        throw "owned process did not report its timeout"
    }
    if (-not (Test-Path -LiteralPath $markerPath)) {
        throw "timeout fixture did not launch its grandchild"
    }
    $grandchildPid = [int](Get-Content -LiteralPath $markerPath -Raw)
    Start-Sleep -Milliseconds 200
    if (Get-Process -Id $grandchildPid -ErrorAction SilentlyContinue) {
        throw "grandchild $grandchildPid survived timeout Job termination"
    }
    Write-Host "Windows Job timeout killed the complete child tree."
} finally {
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
