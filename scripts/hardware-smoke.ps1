param(
    [string]$Serial = "",
    [string]$Gradle = "C:\gradle-9.4.1\bin\gradle.bat",
    [int]$AgentPort = 38997,
    [int]$LargeFileMb = 64,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$PackageName = "com.nekotrans.agent"
$MainActivity = "$PackageName/.MainActivity"
$TransferService = "$PackageName/.TransferService"
$ApkPath = Join-Path $Root "apps/android-agent/app/build/outputs/apk/debug/app-debug.apk"
$SmokeRoot = Join-Path $Root ".nekotrans/hardware-smoke"
$script:AgentSmokeHost = "127.0.0.1"
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message"
}

function Invoke-Adb {
    param(
        [string[]]$Arguments,
        [switch]$AllowFailure
    )

    $fullArgs = @()
    if (-not [string]::IsNullOrWhiteSpace($script:DeviceSerial)) {
        $fullArgs += @("-s", $script:DeviceSerial)
    }
    $fullArgs += $Arguments

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & adb @fullArgs 2>&1
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    $text = ($output | Out-String).Trim()
    if ($exitCode -ne 0 -and -not $AllowFailure) {
        throw "adb $($fullArgs -join ' ') failed with exit code ${exitCode}:`n$text"
    }
    return $text
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

function Send-AgentCommand {
    param(
        [string]$Command,
        [string]$AgentHost = $script:AgentSmokeHost
    )

    $client = $null
    $candidates = @($AgentHost, "127.0.0.1") | Select-Object -Unique
    foreach ($candidate in $candidates) {
        $client = New-Object System.Net.Sockets.TcpClient
        $connect = $client.BeginConnect($candidate, $AgentPort, $null, $null)
        if ($connect.AsyncWaitHandle.WaitOne(3000)) {
            try {
                $client.EndConnect($connect)
                $script:AgentSmokeHost = $candidate
                break
            } catch {
                $client.Close()
                $client = $null
                continue
            }
        }
        $client.Close()
        $client = $null
    }
    if ($null -eq $client) {
        throw "Timed out connecting to agent at ${AgentHost}:$AgentPort and forwarded fallback."
    }

    try {
        $stream = $client.GetStream()
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        $writer = New-Object System.IO.StreamWriter($stream, $utf8NoBom)
        $reader = New-Object System.IO.StreamReader($stream, $utf8NoBom)
        $writer.NewLine = "`n"
        $writer.WriteLine($Command)
        $writer.Flush()
        $reply = $reader.ReadLine()
        if ([string]::IsNullOrWhiteSpace($reply)) {
            throw "Agent returned an empty reply for '$Command'."
        }
        if ($reply -match '"type"\s*:\s*"Error"') {
            throw "Agent command '$Command' failed: $reply"
        }
        return $reply
    } finally {
        $client.Close()
    }
}

function Send-AgentBinaryCommand {
    param(
        [string]$Command,
        [byte[]]$Payload,
        [string]$AgentHost = $script:AgentSmokeHost
    )

    $client = $null
    $candidates = @($AgentHost, "127.0.0.1") | Select-Object -Unique
    foreach ($candidate in $candidates) {
        $client = New-Object System.Net.Sockets.TcpClient
        $connect = $client.BeginConnect($candidate, $AgentPort, $null, $null)
        if ($connect.AsyncWaitHandle.WaitOne(3000)) {
            try {
                $client.EndConnect($connect)
                $script:AgentSmokeHost = $candidate
                break
            } catch {
                $client.Close()
                $client = $null
                continue
            }
        }
        $client.Close()
        $client = $null
    }
    if ($null -eq $client) {
        throw "Timed out connecting to agent at ${AgentHost}:$AgentPort and forwarded fallback."
    }

    try {
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        $stream = $client.GetStream()
        $header = $utf8NoBom.GetBytes($Command.Trim() + "`n")
        $stream.Write($header, 0, $header.Length)
        if ($Payload.Length -gt 0) {
            $stream.Write($Payload, 0, $Payload.Length)
        }
        $stream.Flush()
        $reader = New-Object System.IO.StreamReader($stream, $utf8NoBom)
        $reply = $reader.ReadToEnd().Trim()
        if ([string]::IsNullOrWhiteSpace($reply)) {
            throw "Agent returned an empty reply for '$Command'."
        }
        if ($reply -match '"type"\s*:\s*"Error"') {
            throw "Agent binary command '$Command' failed: $reply"
        }
        return $reply
    } finally {
        $client.Close()
    }
}

function Read-AgentLineFromStream {
    param([System.IO.Stream]$Stream)

    $buffer = [System.IO.MemoryStream]::new()
    try {
        while ($true) {
            $value = $Stream.ReadByte()
            if ($value -lt 0) {
                break
            }
            if ($value -eq 10) {
                break
            }
            if ($value -ne 13) {
                $buffer.WriteByte([byte]$value)
            }
        }
        return $Utf8NoBom.GetString($buffer.ToArray())
    } finally {
        $buffer.Dispose()
    }
}

function Read-ExactFromStream {
    param(
        [System.IO.Stream]$Stream,
        [int]$Length
    )

    $buffer = New-Object byte[] $Length
    $offset = 0
    while ($offset -lt $Length) {
        $read = $Stream.Read($buffer, $offset, $Length - $offset)
        if ($read -le 0) {
            throw "Stream ended before $Length byte(s) were read; received $offset."
        }
        $offset += $read
    }
    return $buffer
}

function Receive-AgentBinaryReply {
    param(
        [string]$Command,
        [string]$AgentHost = $script:AgentSmokeHost
    )

    $client = $null
    $candidates = @($AgentHost, "127.0.0.1") | Select-Object -Unique
    foreach ($candidate in $candidates) {
        $client = New-Object System.Net.Sockets.TcpClient
        $connect = $client.BeginConnect($candidate, $AgentPort, $null, $null)
        if ($connect.AsyncWaitHandle.WaitOne(3000)) {
            try {
                $client.EndConnect($connect)
                $script:AgentSmokeHost = $candidate
                break
            } catch {
                $client.Close()
                $client = $null
                continue
            }
        }
        $client.Close()
        $client = $null
    }
    if ($null -eq $client) {
        throw "Timed out connecting to agent at ${AgentHost}:$AgentPort and forwarded fallback."
    }

    try {
        $stream = $client.GetStream()
        $header = $Utf8NoBom.GetBytes($Command.Trim() + "`n")
        $stream.Write($header, 0, $header.Length)
        $stream.Flush()

        $replyHeader = Read-AgentLineFromStream -Stream $stream
        if ([string]::IsNullOrWhiteSpace($replyHeader)) {
            throw "Agent returned an empty binary header for '$Command'."
        }
        if ($replyHeader -match '"type"\s*:\s*"Error"') {
            throw "Agent binary reply command '$Command' failed: $replyHeader"
        }
        $json = $replyHeader | ConvertFrom-Json
        $length = [int]$json.length
        $payload = Read-ExactFromStream -Stream $stream -Length $length
        return [pscustomobject]@{
            Header = $replyHeader
            Json = $json
            Payload = $payload
        }
    } finally {
        $client.Close()
    }
}

function Send-AgentFileBundleCommand {
    param(
        [string]$BundleId,
        [string]$Manifest,
        [byte[]]$Payload
    )

    $manifestBytes = $Utf8NoBom.GetBytes($Manifest)
    $body = [System.IO.MemoryStream]::new()
    try {
        $body.Write($manifestBytes, 0, $manifestBytes.Length)
        if ($Payload.Length -gt 0) {
            $body.Write($Payload, 0, $Payload.Length)
        }
        return Send-AgentBinaryCommand "PUSH_FILE_BUNDLE_BIN $BundleId $($manifestBytes.Length) $($Payload.Length)" -Payload $body.ToArray()
    } finally {
        $body.Dispose()
    }
}

function Encode-PathArg {
    param([string]$Path)
    return (($Path -replace "\\", "/") -split "/" | ForEach-Object {
        [System.Uri]::EscapeDataString($_)
    }) -join "/"
}

function Assert-BytesEqual {
    param(
        [byte[]]$Expected,
        [byte[]]$Actual,
        [string]$Message
    )

    if ($Expected.Length -ne $Actual.Length) {
        throw "$Message Length mismatch. expected=$($Expected.Length) actual=$($Actual.Length)"
    }
    for ($i = 0; $i -lt $Expected.Length; $i++) {
        if ($Expected[$i] -ne $Actual[$i]) {
            throw "$Message Byte mismatch at offset ${i}: expected=$($Expected[$i]) actual=$($Actual[$i])"
        }
    }
}

function Get-DeviceWifiIp {
    $route = Invoke-Adb -Arguments @("shell", "ip", "route")
    foreach ($line in ($route -split "`r?`n")) {
        if ($line -match "\bsrc\s+([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+)") {
            return $Matches[1]
        }
    }
    return ""
}

function Assert-Contains {
    param(
        [string]$Text,
        [string]$Pattern,
        [string]$Message
    )

    if ($Text -notmatch $Pattern) {
        throw "$Message`nReply: $Text"
    }
}

function Wait-AgentHello {
    param(
        [string]$PreferredHost,
        [int]$Attempts = 8
    )

    for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
        foreach ($candidate in @($PreferredHost, "127.0.0.1") | Select-Object -Unique) {
            try {
                $hello = Send-AgentCommand "HELLO" -AgentHost $candidate
                Assert-Contains -Text $hello -Pattern '"protocol_version"' -Message "Agent HELLO did not return a capability payload."
                $script:AgentSmokeHost = $candidate
                return $hello
            } catch {
                if ($attempt -eq $Attempts -and $candidate -eq "127.0.0.1") {
                    throw
                }
            }
        }
        Start-Sleep -Milliseconds 750
    }
}

function Quote-AdbShell {
    param([string]$Value)
    return "'" + $Value.Replace("'", "'\''") + "'"
}

function New-DeterministicFile {
    param(
        [string]$Path,
        [int]$SizeBytes
    )

    $parent = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $buffer = New-Object byte[] 65536
    $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
    try {
        $written = 0
        while ($written -lt $SizeBytes) {
            for ($i = 0; $i -lt $buffer.Length; $i++) {
                $buffer[$i] = [byte](($written + $i) % 251)
            }
            $toWrite = [Math]::Min($buffer.Length, $SizeBytes - $written)
            $stream.Write($buffer, 0, $toWrite)
            $written += $toWrite
        }
    } finally {
        $stream.Close()
    }
}

function Read-FileChunkBase64 {
    param(
        [string]$Path,
        [int64]$Offset,
        [int]$Length
    )

    $buffer = New-Object byte[] $Length
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $stream.Seek($Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $read = $stream.Read($buffer, 0, $Length)
        if ($read -ne $Length) {
            $buffer = $buffer[0..($read - 1)]
        }
        return [Convert]::ToBase64String($buffer)
    } finally {
        $stream.Close()
    }
}

function Read-FileChunkBytes {
    param(
        [string]$Path,
        [int64]$Offset,
        [int]$Length
    )

    $buffer = New-Object byte[] $Length
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $stream.Seek($Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $read = $stream.Read($buffer, 0, $Length)
        if ($read -eq $Length) {
            return $buffer
        }
        $trimmed = New-Object byte[] $read
        [Array]::Copy($buffer, $trimmed, $read)
        return $trimmed
    } finally {
        $stream.Close()
    }
}


function Write-AdbChunkAtOffset {
    param(
        [string]$LocalPath,
        [string]$RemoteTempFile,
        [string]$RemoteStageDir,
        [int]$ChunkIndex,
        [int64]$Offset,
        [int]$Length,
        [int]$ChunkSize
    )

    $partPath = Join-Path $SmokeRoot ("dual-adb-{0:D8}.part" -f $ChunkIndex)
    $buffer = New-Object byte[] $Length
    $input = [System.IO.File]::OpenRead($LocalPath)
    try {
        $input.Seek($Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $read = $input.Read($buffer, 0, $Length)
        [System.IO.File]::WriteAllBytes($partPath, $buffer[0..($read - 1)])
    } finally {
        $input.Close()
    }

    $remoteStage = "$RemoteStageDir/dual-adb-$('{0:D8}' -f $ChunkIndex).part"
    Invoke-Adb -Arguments @("push", $partPath, $remoteStage) | Out-Null
    $seekBlocks = [int64]($Offset / $ChunkSize)
    $script = "dd if=$(Quote-AdbShell $remoteStage) of=$(Quote-AdbShell $RemoteTempFile) bs=$ChunkSize seek=$seekBlocks conv=notrunc status=none && rm -f $(Quote-AdbShell $remoteStage)"
    Invoke-Adb -Arguments @("shell", $script) | Out-Null
}

Write-Step "Selecting Android device"
$script:DeviceSerial = Select-AdbDevice -RequestedSerial $Serial
Write-Host "Using device: $script:DeviceSerial"

if (-not $SkipBuild) {
    Write-Step "Building Android debug APK"
    if (-not (Test-Path -LiteralPath $Gradle)) {
        throw "Gradle executable was not found: $Gradle"
    }
    Push-Location (Join-Path $Root "apps/android-agent")
    try {
        & $Gradle :app:assembleDebug
        if ($LASTEXITCODE -ne 0) {
            throw "Gradle assembleDebug failed with exit code $LASTEXITCODE."
        }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path -LiteralPath $ApkPath)) {
    throw "Debug APK was not found: $ApkPath"
}

Write-Step "Installing Android agent"
Invoke-Adb -Arguments @("install", "-r", $ApkPath) | Write-Host

Write-Step "Granting best-effort permissions"
Invoke-Adb -Arguments @("shell", "pm", "grant", $PackageName, "android.permission.POST_NOTIFICATIONS") -AllowFailure | Out-Null
Invoke-Adb -Arguments @("shell", "appops", "set", "--uid", $PackageName, "MANAGE_EXTERNAL_STORAGE", "allow") -AllowFailure | Out-Null
Invoke-Adb -Arguments @("shell", "appops", "set", $PackageName, "MANAGE_EXTERNAL_STORAGE", "allow") -AllowFailure | Out-Null

Write-Step "Launching Android activity"
Invoke-Adb -Arguments @("shell", "am", "start", "-n", $MainActivity) | Write-Host
Start-Sleep -Seconds 3

Write-Step "Forwarding local port $AgentPort to Android agent"
Invoke-Adb -Arguments @("forward", "tcp:$AgentPort", "tcp:$AgentPort") | Write-Host

Write-Step "Probing agent protocol"
$hello = Send-AgentCommand "HELLO"
Assert-Contains -Text $hello -Pattern '"protocol_version"' -Message "HELLO did not return a capability payload."
$ping = Send-AgentCommand "PING"
Assert-Contains -Text $ping -Pattern '"type"\s*:\s*"Pong"' -Message "PING did not return Pong."

$wifiIp = Get-DeviceWifiIp
if (-not [string]::IsNullOrWhiteSpace($wifiIp)) {
    Write-Step "Probing direct LAN agent at ${wifiIp}:$AgentPort"
    $lanHello = Send-AgentCommand "HELLO" -AgentHost $wifiIp
    Assert-Contains -Text $lanHello -Pattern '"protocol_version"' -Message "Direct LAN HELLO did not return a capability payload."
    $script:AgentSmokeHost = $wifiIp
} else {
    Write-Host "No Wi-Fi IP discovered; skipping direct LAN probe."
}

Write-Step "Running Wi-Fi/agent file protocol smoke via ${script:AgentSmokeHost}:$AgentPort"
$taskId = "hardware-smoke-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())"
$payloadText = "nekotrans hardware smoke"
$payload = [System.Text.Encoding]::UTF8.GetBytes($payloadText)
$payloadBase64 = [Convert]::ToBase64String($payload)
$payloadLength = $payload.Length

Send-AgentCommand "START_TASK $taskId" | Write-Host
$agentTargetRootReply = Send-AgentCommand "SET_TARGET_ROOT hardware-smoke"
$agentTargetRootReply | Write-Host
$agentTargetRoot = ($agentTargetRootReply | ConvertFrom-Json).target_root
Send-AgentCommand "START_FILE hello.txt $payloadLength" | Write-Host
Send-AgentBinaryCommand "PUSH_CHUNK_BIN hello.txt 0 0 $payloadLength" -Payload $payload | Write-Host
$status = Send-AgentCommand "CHUNK_STATUS hello.txt 0 0 $payloadLength"
Assert-Contains -Text $status -Pattern '"status"\s*:\s*"committed"' -Message "CHUNK_STATUS did not confirm the pushed chunk."
Send-AgentCommand "COMPLETE_FILE hello.txt" | Write-Host
$stat = Send-AgentCommand "STAT_FILE hello.txt"
Assert-Contains -Text $stat -Pattern """size_bytes"":$payloadLength" -Message "STAT_FILE size did not match the pushed payload."
$pulled = Send-AgentCommand "PULL_CHUNK hello.txt 0 $payloadLength"
$pulledPayload = ($pulled | ConvertFrom-Json).payload
if ($pulledPayload -ne $payloadBase64) {
    throw "PULL_CHUNK payload did not match.`nExpected: $payloadBase64`nActual:   $pulledPayload"
}
$binaryPulled = Receive-AgentBinaryReply "PULL_CHUNK_BIN hello.txt 0 $payloadLength"
if ([int]$binaryPulled.Json.length -ne $payloadLength) {
    throw "PULL_CHUNK_BIN length did not match. expected=$payloadLength actual=$($binaryPulled.Json.length)"
}
Assert-BytesEqual -Expected $payload -Actual $binaryPulled.Payload -Message "PULL_CHUNK_BIN payload did not match."

$bundleFileText = "bundle payload"
$bundlePayload = [System.Text.Encoding]::UTF8.GetBytes($bundleFileText)
$bundleManifest = ""
$bundleManifest += "D`t$(Encode-PathArg 'bundle-empty')`n"
$bundleManifest += "D`t$(Encode-PathArg 'bundle-empty/child folder')`n"
$bundleManifest += "F`t$(Encode-PathArg 'bundle-empty/file.txt')`t$($bundlePayload.Length)`t0`n"
$bundleReply = Send-AgentFileBundleCommand "hardware-dirs-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())" $bundleManifest $bundlePayload
$bundleReply | Write-Host
$bundleAck = $bundleReply | ConvertFrom-Json
if ([int]$bundleAck.directories -ne 2 -or [int]$bundleAck.files -ne 1 -or [int]$bundleAck.bytes -ne $bundlePayload.Length) {
    throw "PUSH_FILE_BUNDLE_BIN directory ACK mismatch: $bundleReply"
}
$bundleStat = Send-AgentCommand "STAT_FILE bundle-empty/file.txt"
Assert-Contains -Text $bundleStat -Pattern """size_bytes"":$($bundlePayload.Length)" -Message "Bundle file STAT_FILE size did not match."
$dirProbe = Invoke-Adb -Arguments @("shell", "if [ -d $(Quote-AdbShell "$agentTargetRoot/bundle-empty/child folder") ]; then echo ok; else echo missing; fi")
if ($dirProbe.Trim() -ne "ok") {
    throw "Bundle directory was not created on Android: $agentTargetRoot/bundle-empty/child folder"
}

$verify = Send-AgentCommand "VERIFY_FILE hello.txt BLAKE3"
Assert-Contains -Text $verify -Pattern '"algorithm"\s*:\s*"BLAKE3"' -Message "VERIFY_FILE did not return a BLAKE3 result."
Send-AgentCommand "LOG_SNAPSHOT" | Write-Host

Write-Step "Running same-file ADB + Wi-Fi convergence smoke"
$dualTaskId = "hardware-dual-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())"
$dualLocal = Join-Path $SmokeRoot "dual-large.bin"
$dualPulled = Join-Path $SmokeRoot "dual-large.pulled.bin"
$dualSize = 4 * 1024 * 1024
$dualChunkSize = 256 * 1024
New-DeterministicFile -Path $dualLocal -SizeBytes $dualSize
Send-AgentCommand "START_TASK $dualTaskId" | Write-Host
$targetRoot = (Send-AgentCommand "SET_TARGET_ROOT hardware-smoke" | ConvertFrom-Json).target_root
Send-AgentCommand "START_FILE dual-large.bin $dualSize" | Write-Host
$remoteStageDir = "$targetRoot/.nekotrans-adb-stage"
$remoteTempFile = "$targetRoot/dual-large.bin.nekotrans-tmp"
$remoteFinalFile = "$targetRoot/dual-large.bin"
Invoke-Adb -Arguments @("shell", "mkdir", "-p", $remoteStageDir) | Out-Null

for ($chunkIndex = 0; $chunkIndex -lt [int]($dualSize / $dualChunkSize); $chunkIndex++) {
    $offset = [int64]$chunkIndex * $dualChunkSize
    if (($chunkIndex % 2) -eq 0) {
        Write-AdbChunkAtOffset -LocalPath $dualLocal -RemoteTempFile $remoteTempFile -RemoteStageDir $remoteStageDir -ChunkIndex $chunkIndex -Offset $offset -Length $dualChunkSize -ChunkSize $dualChunkSize
    } else {
        $chunkPayload = Read-FileChunkBytes -Path $dualLocal -Offset $offset -Length $dualChunkSize
        Send-AgentBinaryCommand "PUSH_CHUNK_BIN dual-large.bin $chunkIndex $offset $dualChunkSize" -Payload $chunkPayload | Out-Null
    }
}

Send-AgentCommand "COMPLETE_FILE dual-large.bin" | Write-Host
Invoke-Adb -Arguments @("pull", $remoteFinalFile, $dualPulled) | Out-Null
$localHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $dualLocal).Hash
$pulledHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $dualPulled).Hash
if ($localHash -ne $pulledHash) {
    throw "Same-file ADB + Wi-Fi convergence hash mismatch. local=$localHash pulled=$pulledHash"
}

Write-Step "Restarting Android agent and probing disk-backed chunk recovery"
Invoke-Adb -Arguments @("shell", "am", "force-stop", $PackageName) | Write-Host
Start-Sleep -Seconds 2
Invoke-Adb -Arguments @("shell", "am", "start", "-n", $MainActivity) | Write-Host
Start-Sleep -Seconds 3
Invoke-Adb -Arguments @("forward", "tcp:$AgentPort", "tcp:$AgentPort") | Write-Host
Wait-AgentHello -PreferredHost $script:AgentSmokeHost | Out-Null
Send-AgentCommand "START_TASK $taskId-restart" | Write-Host
Send-AgentCommand "SET_TARGET_ROOT hardware-smoke" | Write-Host
$restartStatus = Send-AgentCommand "CHUNK_STATUS hello.txt 0 0 $payloadLength"
Assert-Contains -Text $restartStatus -Pattern '"status"\s*:\s*"committed_on_disk"' -Message "Restart CHUNK_STATUS did not confirm disk-backed completion."

Write-Step "Running raw ADB large-file push smoke"
New-Item -ItemType Directory -Force -Path $SmokeRoot | Out-Null
$largePath = Join-Path $SmokeRoot "adb-large.bin"
$largeBytes = [int64]$LargeFileMb * 1024 * 1024
$file = [System.IO.File]::Open($largePath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
try {
    $file.SetLength($largeBytes)
} finally {
    $file.Close()
}

$remoteDir = "/sdcard/Download/NekotransSmoke"
$remoteFile = "$remoteDir/adb-large.bin"
Invoke-Adb -Arguments @("shell", "mkdir", "-p", $remoteDir) | Write-Host
Invoke-Adb -Arguments @("push", $largePath, $remoteFile) | Write-Host
$remoteSize = Invoke-Adb -Arguments @("shell", "stat", "-c", "%s", $remoteFile)
if (($remoteSize.Trim()) -ne $largeBytes.ToString()) {
    throw "Raw ADB large-file size mismatch. local=$largeBytes remote=$remoteSize"
}

Write-Step "Pausing/resuming agent task"
Send-AgentCommand "PAUSE_TASK" | Write-Host
Send-AgentCommand "RESUME_TASK" | Write-Host

Write-Step "Hardware smoke completed"
Write-Host "Device: $script:DeviceSerial"
Write-Host "Agent port: $AgentPort"
Write-Host "ADB large file: $largeBytes bytes"
