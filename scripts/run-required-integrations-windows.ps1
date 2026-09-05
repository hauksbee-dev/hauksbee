<# Run the three release-required external-emulator flows on native Windows. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ExpectedSha,
    [Parameter(Mandatory = $true)]
    [string]$EvidenceOut
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "windows-owned-process.ps1")

$actualSha = (& git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $actualSha -notmatch '^[0-9a-f]{40}$') {
    throw "could not resolve the checked-out commit"
}
if ($actualSha -ne $ExpectedSha) {
    throw "required integration SHA mismatch: expected $ExpectedSha, checked out $actualSha"
}

# Re-hash the repository-pinned archives, freshly extract them, and compare the
# complete installed payloads in this same evidence process. A prior install
# step or a launcher-only probe is not sufficient proof.
$preflightEvidence = Join-Path ([IO.Path]::GetTempPath()) "required-windows-backends-$PID-$([guid]::NewGuid().ToString('N')).json"
try {
    & (Join-Path $PSScriptRoot "install-sims-windows.ps1") -Check -EvidenceOut $preflightEvidence
    if ($LASTEXITCODE -ne 0) { throw "checksum-pinned simulator preflight failed" }
    $verifiedBackends = (Get-Content -LiteralPath $preflightEvidence -Raw | ConvertFrom-Json).backends
} finally {
    if (Test-Path -LiteralPath $preflightEvidence) { Remove-Item -LiteralPath $preflightEvidence -Force }
}

$renodePath = (Get-ChildItem -LiteralPath (Join-Path $HOME "renode-portable") -Filter Renode.exe -File -Recurse | Select-Object -First 1).FullName
$xtensaPath = (Resolve-Path (Join-Path $HOME ".hauksbee-qemu-esp\qemu\bin\qemu-system-xtensa.exe")).Path
$riscv32Path = (Resolve-Path (Join-Path $HOME ".hauksbee-qemu-esp\qemu\bin\qemu-system-riscv32.exe")).Path
foreach ($path in @($renodePath, $xtensaPath, $riscv32Path)) {
    if (-not $path -or -not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "required pinned simulator executable is missing: $path"
    }
}
$backends = [ordered]@{
    HAUKSBEE_RENODE = [ordered]@{
        path = $renodePath
        artifact_sha256 = $verifiedBackends.HAUKSBEE_RENODE.artifact_sha256
        install_tree_sha256 = $verifiedBackends.HAUKSBEE_RENODE.install_tree_sha256
        archive_sha256s = @("d09b7934cfd560cd06bde8f131ef78f521f10d423d5aac6096f2a583224aeb3e")
    }
    HAUKSBEE_QEMU_XTENSA = [ordered]@{
        path = $xtensaPath
        artifact_sha256 = $verifiedBackends.HAUKSBEE_QEMU_XTENSA.artifact_sha256
        install_tree_sha256 = $verifiedBackends.HAUKSBEE_QEMU_XTENSA.install_tree_sha256
        archive_sha256s = @(
            "3c483d77f5350a568df1faf4d8dbc82c95d6bc2b826d0d4be910485e0a68ca2a",
            "697aa4800a1f52be0b1693b30e22a684f7ea93c46c489e619384cae7b0e9b87b"
        )
    }
    HAUKSBEE_QEMU_RISCV32 = [ordered]@{
        path = $riscv32Path
        artifact_sha256 = $verifiedBackends.HAUKSBEE_QEMU_RISCV32.artifact_sha256
        install_tree_sha256 = $verifiedBackends.HAUKSBEE_QEMU_RISCV32.install_tree_sha256
        archive_sha256s = @(
            "3c483d77f5350a568df1faf4d8dbc82c95d6bc2b826d0d4be910485e0a68ca2a",
            "697aa4800a1f52be0b1693b30e22a684f7ea93c46c489e619384cae7b0e9b87b"
        )
    }
}

$gates = @(
    @{
        Name = "renode-rp2040-adc"
        Test = "rp2040_adc_injection_reaches_firmware"
        Args = @("test", "-p", "hauksbee-mcu", "--no-default-features", "--features", "renode", "--test", "renode_rp2040_adc", "rp2040_adc_injection_reaches_firmware", "--", "--exact", "--nocapture", "--test-threads=1")
    },
    @{
        Name = "qemu-xtensa-i2c"
        Test = "esp32_i2c_firmware_drives_gpio_from_temperature"
        Args = @("test", "-p", "hauksbee-engine", "--no-default-features", "--features", "qemu", "--test", "i2c_sensor_cosim_qemu", "esp32_i2c_firmware_drives_gpio_from_temperature", "--", "--exact", "--nocapture", "--test-threads=1")
    },
    @{
        Name = "qemu-riscv32-circuit"
        Test = "esp32c3_full_cosim_through_solved_circuit"
        Args = @("test", "-p", "hauksbee-engine", "--no-default-features", "--features", "qemu", "--test", "esp32_qemu_cosim", "esp32c3_full_cosim_through_solved_circuit", "--", "--exact", "--nocapture", "--test-threads=1")
    }
)

$savedBackendEnv = @{}
foreach ($name in @("HAUKSBEE_RENODE", "HAUKSBEE_QEMU_XTENSA", "HAUKSBEE_QEMU_RISCV32", "HAUKSBEE_QEMU_DIR")) {
    $savedBackendEnv[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
}
$env:HAUKSBEE_RENODE = $renodePath
$env:HAUKSBEE_QEMU_XTENSA = $xtensaPath
$env:HAUKSBEE_QEMU_RISCV32 = $riscv32Path
Remove-Item Env:HAUKSBEE_QEMU_DIR -ErrorAction SilentlyContinue

$results = @()
try {
foreach ($gate in $gates) {
    $stdout = Join-Path ([IO.Path]::GetTempPath()) "required-$($gate.Name)-$PID.stdout"
    $stderr = Join-Path ([IO.Path]::GetTempPath()) "required-$($gate.Name)-$PID.stderr"
    try {
        $process = Invoke-HauksbeeJobProcess -FilePath "cargo" -ArgumentList $gate.Args `
            -WorkingDirectory (Get-Location).Path -StandardOutput $stdout -StandardError $stderr `
            -TimeoutMilliseconds 600000
        if ($process.TimedOut) {
            throw "$($gate.Name) exceeded 600 seconds"
        }
        $output = ((Get-Content -LiteralPath $stdout -Raw -ErrorAction SilentlyContinue) +
            (Get-Content -LiteralPath $stderr -Raw -ErrorAction SilentlyContinue))
        Write-Host $output
        if ($process.ExitCode -ne 0) { throw "$($gate.Name) exited $($process.ExitCode)" }
        if ($output -match "SKIP:") { throw "$($gate.Name) reported a skipped integration" }
        $escaped = [regex]::Escape([string]$gate.Test)
        if ($output -notmatch "(?m)^test $escaped \.\.\." -or
            $output -notmatch "(?m)^test result: ok\. 1 passed; 0 failed(?:;|$)") {
            throw "$($gate.Name) did not prove exact test $($gate.Test)"
        }
        $results += $gate.Name
    } finally {
        foreach ($path in @($stdout, $stderr)) {
            if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
        }
    }
}
} finally {
    foreach ($name in $savedBackendEnv.Keys) {
        $value = $savedBackendEnv[$name]
        if ($null -eq $value) {
            Remove-Item "Env:$name" -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable($name, $value, "Process")
        }
    }
}

# A firmware test is repository code and may not mutate a backend after the
# preflight and then retain stale evidence. Re-extract the same pinned archives
# and compare the complete installed trees again before writing the receipt.
$postflightEvidence = Join-Path ([IO.Path]::GetTempPath()) "required-windows-postflight-$PID-$([guid]::NewGuid().ToString('N')).json"
try {
    & (Join-Path $PSScriptRoot "install-sims-windows.ps1") -Check -EvidenceOut $postflightEvidence
    $postflightBackends = (Get-Content -LiteralPath $postflightEvidence -Raw | ConvertFrom-Json).backends
} finally {
    if (Test-Path -LiteralPath $postflightEvidence) { Remove-Item -LiteralPath $postflightEvidence -Force }
}
foreach ($name in $backends.Keys) {
    $postflight = $postflightBackends.PSObject.Properties[$name].Value
    foreach ($field in @(
        "artifact_sha256",
        "install_tree_sha256"
    )) {
        if ($postflight.$field -ne $backends[$name][$field]) {
            throw "required backend changed while gates ran: $name ($field)"
        }
    }
}

$evidence = [ordered]@{
    schema_version = 1
    platform = "windows-x86_64"
    commit_sha = $actualSha
    backends = $backends
    gates = $results
}
$parent = Split-Path -Parent $EvidenceOut
if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
$evidence | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $EvidenceOut -Encoding utf8
Write-Host "Retained native Windows required-integration evidence at $EvidenceOut"
