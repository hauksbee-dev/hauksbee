<# Native regression: changing one installed backend byte must invalidate -Check. #>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$installer = Join-Path $PSScriptRoot "install-sims-windows.ps1"
$candidate = Get-ChildItem -LiteralPath (Join-Path $HOME "renode-portable") -File -Recurse |
    Where-Object { $_.Name -ne "Renode.exe" } |
    Sort-Object Length |
    Select-Object -First 1
if (-not $candidate) { throw "no non-launcher Renode payload file is available to test" }
$original = [IO.File]::ReadAllBytes($candidate.FullName)
try {
    $modified = [byte[]]::new($original.Length + 1)
    [Array]::Copy($original, $modified, $original.Length)
    [IO.File]::WriteAllBytes($candidate.FullName, $modified)
    $rejected = $false
    try {
        & $installer -Check
    } catch {
        $rejected = $true
    }
    if (-not $rejected) { throw "modified installed simulator payload passed -Check" }
} finally {
    [IO.File]::WriteAllBytes($candidate.FullName, $original)
}
& $installer -Check
Write-Host "Windows simulator full-payload integrity refusal passed."
