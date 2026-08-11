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

$actualSha = (& git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $actualSha -notmatch '^[0-9a-f]{40}$') {
    throw "could not resolve the checked-out commit"
}
if ($actualSha -ne $ExpectedSha) {
    throw "required integration SHA mismatch: expected $ExpectedSha, checked out $actualSha"
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

$results = @()
foreach ($gate in $gates) {
    $stdout = Join-Path ([IO.Path]::GetTempPath()) "required-$($gate.Name)-$PID.stdout"
    $stderr = Join-Path ([IO.Path]::GetTempPath()) "required-$($gate.Name)-$PID.stderr"
    try {
        $process = Start-Process -FilePath "cargo" -ArgumentList $gate.Args -NoNewWindow -PassThru `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        if (-not $process.WaitForExit(600000)) {
            taskkill /T /F /PID $process.Id | Out-Null
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

$evidence = [ordered]@{
    schema_version = 1
    platform = "windows-x86_64"
    commit_sha = $actualSha
    renode = (Get-ChildItem -LiteralPath (Join-Path $HOME "renode-portable") -Filter Renode.exe -File -Recurse | Select-Object -First 1).FullName
    qemu_xtensa = (Resolve-Path (Join-Path $HOME ".hauksbee-qemu-esp\qemu\bin\qemu-system-xtensa.exe")).Path
    qemu_riscv32 = (Resolve-Path (Join-Path $HOME ".hauksbee-qemu-esp\qemu\bin\qemu-system-riscv32.exe")).Path
    gates = $results
}
$parent = Split-Path -Parent $EvidenceOut
if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
$evidence | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $EvidenceOut -Encoding utf8
Write-Host "Retained native Windows required-integration evidence at $EvidenceOut"
