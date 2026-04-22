param(
    [string]$Serial = "",
    [string]$AgentHost = "",
    [string]$Modes = "adb,wifi,dual",
    [switch]$Cleanup,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$DesktopManifest = Join-Path $Root "apps/desktop/src-tauri/Cargo.toml"
$SmokeExe = Join-Path $Root "apps/desktop/src-tauri/target/debug/desktop_worker_smoke.exe"

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message"
}

function Select-AdbDevice {
    param([string]$RequestedSerial)

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $lines = & adb devices -l 2>&1
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($exitCode -ne 0) {
        throw "adb devices failed:`n$(($lines | Out-String).Trim())"
    }

    $devices = @()
    foreach ($line in ($lines | Select-Object -Skip 1)) {
        $trimmed = $line.Trim()
        if ($trimmed.Length -eq 0) {
            continue
        }
        $parts = $trimmed -split "\s+"
        if ($parts.Length -ge 2 -and $parts[1] -eq "device") {
            $devices += $parts[0]
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($RequestedSerial)) {
        if ($devices -notcontains $RequestedSerial) {
            throw "Requested Android device '$RequestedSerial' is not connected and authorized."
        }
        return $RequestedSerial
    }

    if ($devices.Count -eq 0) {
        throw "No authorized Android device found. Connect a device, enable USB debugging, and accept the RSA prompt."
    }
    if ($devices.Count -gt 1) {
        throw "Multiple Android devices found. Re-run with -Serial <serial>."
    }
    return $devices[0]
}

Push-Location $Root
try {
    $DeviceSerial = Select-AdbDevice -RequestedSerial $Serial
    Write-Step "Selected Android device $DeviceSerial"

    if (-not $SkipBuild) {
        Write-Step "Building desktop worker smoke binary"
        cargo build --manifest-path $DesktopManifest --bin desktop_worker_smoke
    } elseif (-not (Test-Path -LiteralPath $SmokeExe)) {
        throw "Smoke binary not found at $SmokeExe. Re-run without -SkipBuild first."
    }

    $smokeArgs = @("--serial", $DeviceSerial, "--modes", $Modes)
    if (-not [string]::IsNullOrWhiteSpace($AgentHost)) {
        $smokeArgs += @("--host", $AgentHost)
    }
    if ($Cleanup) {
        $smokeArgs += "--cleanup"
    }

    Write-Step "Running desktop worker smoke ($Modes)"
    & $SmokeExe @smokeArgs
    if ($LASTEXITCODE -ne 0) {
        throw "desktop_worker_smoke failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}
