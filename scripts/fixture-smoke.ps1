param(
    [string]$Serial = "",
    [int]$AgentPort = 38997,
    [string]$FixtureRoot = "",
    [int64]$IsoSampleBytes = 1073741824,
    [int]$ChunkSizeMb = 8,
    [int]$FolderFileLimit = 256,
    [int]$FolderChunkKb = 256,
    [int]$FolderHashLimit = 32,
    [int]$FolderStatLimit = 64,
    [int]$MaxFolderFileMb = 256,
    [int]$FolderByteLimitMb = 64,
    [switch]$SkipIso,
    [switch]$SkipIsoHash,
    [switch]$SkipFolder,
    [switch]$SkipPathSmoke,
    [switch]$CleanupRemote
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$PackageName = "com.nekotrans.agent"
$MainActivity = "$PackageName/.MainActivity"
$WorkRoot = Join-Path $Root ".nekotrans/fixture-smoke"
$script:DeviceSerial = ""
$script:AgentHost = "127.0.0.1"
$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)

$IsoPath = ""
$FolderPath = ""

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

function Quote-ProcessArgument {
    param([string]$Value)
    if ($null -eq $Value) {
        return '""'
    }
    return '"' + $Value.Replace('\', '\\').Replace('"', '\"') + '"'
}

function Invoke-AdbShellBinaryInput {
    param(
        [string]$ShellScript,
        [byte[]]$Payload
    )

    $arguments = @()
    if (-not [string]::IsNullOrWhiteSpace($script:DeviceSerial)) {
        $arguments += @("-s", $script:DeviceSerial)
    }
    $arguments += @("exec-in", "sh", "-c", $ShellScript)

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = "adb"
    $psi.Arguments = ($arguments | ForEach-Object { Quote-ProcessArgument $_ }) -join " "
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $psi
    if (-not $process.Start()) {
        throw "Failed to start adb shell for binary stdin write."
    }
    try {
        $writeError = $null
        try {
            if ($Payload.Length -gt 0) {
                $process.StandardInput.BaseStream.Write($Payload, 0, $Payload.Length)
            }
        } catch {
            $writeError = $_
        } finally {
            $process.StandardInput.Close()
        }
        $stdout = $process.StandardOutput.ReadToEnd()
        $stderr = $process.StandardError.ReadToEnd()
        $process.WaitForExit()
        if ($null -ne $writeError) {
            throw "adb shell binary stdin write failed before process exit. exit=$($process.ExitCode):`n$stdout`n$stderr`n$writeError"
        }
        if ($process.ExitCode -ne 0) {
            throw "adb $($arguments -join ' ') failed with exit code $($process.ExitCode):`n$stdout`n$stderr"
        }
        return (($stdout + "`n" + $stderr).Trim())
    } finally {
        $process.Dispose()
    }
}

function Select-AdbDevice {
    param([string]$RequestedSerial)

    $lines = & adb devices -l 2>&1
    if ($LASTEXITCODE -ne 0) {
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
        throw "No authorized Android device found."
    }
    if ($devices.Count -gt 1) {
        throw "Multiple Android devices found. Re-run with -Serial <serial>."
    }
    return $devices[0]
}

function Quote-AdbShell {
    param([string]$Value)
    return "'" + $Value.Replace("'", "'\''") + "'"
}

function Encode-PathArg {
    param([string]$Path)
    return (($Path -replace "\\", "/") -split "/" | ForEach-Object {
        [System.Uri]::EscapeDataString($_)
    }) -join "/"
}

function Send-AgentCommand {
    param(
        [string]$Command,
        [string]$AgentHost = $script:AgentHost
    )

    $client = $null
    $candidates = @($AgentHost, "127.0.0.1") | Select-Object -Unique
    foreach ($candidate in $candidates) {
        $client = [System.Net.Sockets.TcpClient]::new()
        $connect = $client.BeginConnect($candidate, $AgentPort, $null, $null)
        if ($connect.AsyncWaitHandle.WaitOne(5000)) {
            $client.EndConnect($connect)
            $script:AgentHost = $candidate
            break
        }
        $client.Dispose()
        $client = $null
    }
    if ($null -eq $client) {
        throw "Timed out connecting to agent at ${AgentHost}:$AgentPort and forwarded fallback."
    }

    $reader = $null
    $writer = $null
    try {
        $stream = $client.GetStream()
        $writer = [System.IO.StreamWriter]::new($stream, $Utf8NoBom)
        $writer.NewLine = "`n"
        $writer.AutoFlush = $true
        $writer.WriteLine($Command)
        $reader = [System.IO.StreamReader]::new($stream, $Utf8NoBom)
        $reply = $reader.ReadLine()
        if ([string]::IsNullOrWhiteSpace($reply)) {
            throw "Agent returned an empty reply for '$Command'."
        }
        if ($reply -match '"type"\s*:\s*"Error"') {
            throw "Agent command '$Command' failed: $reply"
        }
        return $reply
    } finally {
        if ($reader) { $reader.Dispose() }
        if ($writer) { $writer.Dispose() }
        $client.Dispose()
    }
}

function Send-AgentBinaryCommand {
    param(
        [string]$Command,
        [byte[]]$Payload,
        [string]$AgentHost = $script:AgentHost
    )

    $client = $null
    $candidates = @($AgentHost, "127.0.0.1") | Select-Object -Unique
    foreach ($candidate in $candidates) {
        $client = [System.Net.Sockets.TcpClient]::new()
        $connect = $client.BeginConnect($candidate, $AgentPort, $null, $null)
        if ($connect.AsyncWaitHandle.WaitOne(5000)) {
            $client.EndConnect($connect)
            $script:AgentHost = $candidate
            break
        }
        $client.Dispose()
        $client = $null
    }
    if ($null -eq $client) {
        throw "Timed out connecting to agent at ${AgentHost}:$AgentPort and forwarded fallback."
    }

    $reader = $null
    try {
        $stream = $client.GetStream()
        $header = $Utf8NoBom.GetBytes($Command.Trim() + "`n")
        $stream.Write($header, 0, $header.Length)
        if ($Payload.Length -gt 0) {
            $stream.Write($Payload, 0, $Payload.Length)
        }
        $stream.Flush()
        $reader = [System.IO.StreamReader]::new($stream, $Utf8NoBom)
        $reply = $reader.ReadToEnd().Trim()
        if ([string]::IsNullOrWhiteSpace($reply)) {
            throw "Agent returned an empty reply for '$Command'."
        }
        if ($reply -match '"type"\s*:\s*"Error"') {
            throw "Agent binary command '$Command' failed: $reply"
        }
        return $reply
    } finally {
        if ($reader) { $reader.Dispose() }
        $client.Dispose()
    }
}

function ConvertTo-BigEndianUInt32 {
    param([uint32]$Value)
    $bytes = [BitConverter]::GetBytes($Value)
    if ([BitConverter]::IsLittleEndian) { [Array]::Reverse($bytes) }
    return $bytes
}

function ConvertTo-BigEndianUInt64 {
    param([uint64]$Value)
    $bytes = [BitConverter]::GetBytes($Value)
    if ([BitConverter]::IsLittleEndian) { [Array]::Reverse($bytes) }
    return $bytes
}

function Send-AgentChunkBatchCommand {
    param(
        [string]$EncodedRelativePath,
        [object[]]$Chunks
    )

    if ($Chunks.Count -eq 0) {
        return
    }

    $client = $null
    $candidates = @($script:AgentHost, "127.0.0.1") | Select-Object -Unique
    foreach ($candidate in $candidates) {
        $client = [System.Net.Sockets.TcpClient]::new()
        $connect = $client.BeginConnect($candidate, $AgentPort, $null, $null)
        if ($connect.AsyncWaitHandle.WaitOne(5000)) {
            $client.EndConnect($connect)
            $script:AgentHost = $candidate
            break
        }
        $client.Dispose()
        $client = $null
    }
    if ($null -eq $client) {
        throw "Timed out connecting to agent at ${script:AgentHost}:$AgentPort and forwarded fallback."
    }

    $reader = $null
    try {
        $stream = $client.GetStream()
        $header = $Utf8NoBom.GetBytes("PUSH_CHUNK_BATCH_BIN $EncodedRelativePath $($Chunks.Count)`n")
        $stream.Write($header, 0, $header.Length)
        foreach ($chunk in $Chunks) {
            $indexBytes = ConvertTo-BigEndianUInt32 ([uint32]$chunk.ChunkIndex)
            $offsetBytes = ConvertTo-BigEndianUInt64 ([uint64]$chunk.Offset)
            $lengthBytes = ConvertTo-BigEndianUInt32 ([uint32]$chunk.Payload.Length)
            $stream.Write($indexBytes, 0, $indexBytes.Length)
            $stream.Write($offsetBytes, 0, $offsetBytes.Length)
            $stream.Write($lengthBytes, 0, $lengthBytes.Length)
            $stream.Write($chunk.Payload, 0, $chunk.Payload.Length)
        }
        $stream.Flush()
        $reader = [System.IO.StreamReader]::new($stream, $Utf8NoBom)
        $reply = $reader.ReadToEnd().Trim()
        if ([string]::IsNullOrWhiteSpace($reply)) {
            throw "Agent returned an empty reply for PUSH_CHUNK_BATCH_BIN."
        }
        if ($reply -match '"type"\s*:\s*"Error"') {
            throw "Agent binary batch command failed: $reply"
        }
        return $reply
    } finally {
        if ($reader) { $reader.Dispose() }
        $client.Dispose()
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
        Send-AgentBinaryCommand "PUSH_FILE_BUNDLE_BIN $BundleId $($manifestBytes.Length) $($Payload.Length)" -Payload $body.ToArray()
    } finally {
        $body.Dispose()
    }
}

function Format-Throughput {
    param(
        [int64]$Bytes,
        [double]$Seconds
    )
    if ($Seconds -le 0) {
        return "n/a"
    }
    return ("{0:N2} MB/s" -f (($Bytes / 1MB) / $Seconds))
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

function Resolve-FixturePaths {
    if ([string]::IsNullOrWhiteSpace($FixtureRoot)) {
        $fixtureCandidates = Get-ChildItem -LiteralPath $Root -Directory -Force | Where-Object {
            @(Get-ChildItem -LiteralPath $_.FullName -File -Filter "*.iso" -Force -ErrorAction SilentlyContinue).Count -gt 0
        }
        if ($fixtureCandidates.Count -eq 0) {
            throw "Could not auto-discover fixture root. Pass -FixtureRoot <path>."
        }
        $script:FixtureRoot = $fixtureCandidates[0].FullName
    }

    $iso = Get-ChildItem -LiteralPath $FixtureRoot -File -Filter "*.iso" -Force | Select-Object -First 1
    if ($null -ne $iso) {
        $script:IsoPath = $iso.FullName
    }

    $folder = Get-ChildItem -LiteralPath $FixtureRoot -Directory -Force | Select-Object -First 1
    if ($null -ne $folder) {
        $script:FolderPath = $folder.FullName
    }
}

function Wait-AgentHello {
    param([string]$PreferredHost)

    for ($attempt = 1; $attempt -le 8; $attempt++) {
        foreach ($candidate in @($PreferredHost, "127.0.0.1") | Select-Object -Unique) {
            try {
                $hello = Send-AgentCommand "HELLO" -AgentHost $candidate
                if ($hello -match '"protocol_version"') {
                    $script:AgentHost = $candidate
                    return $hello
                }
            } catch {
                if ($attempt -eq 8 -and $candidate -eq "127.0.0.1") {
                    throw
                }
            }
        }
        Start-Sleep -Milliseconds 750
    }
}

function New-FilePrefixSample {
    param(
        [string]$Source,
        [string]$Destination,
        [int64]$Bytes
    )

    if ((Test-Path -LiteralPath $Destination) -and (Get-Item -LiteralPath $Destination).Length -eq $Bytes) {
        return
    }

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Destination) | Out-Null
    $input = [System.IO.File]::OpenRead($Source)
    $output = [System.IO.File]::Open($Destination, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
    try {
        $buffer = New-Object byte[] (4MB)
        $remaining = $Bytes
        while ($remaining -gt 0) {
            $toRead = [Math]::Min($buffer.Length, $remaining)
            $read = $input.Read($buffer, 0, $toRead)
            if ($read -le 0) {
                throw "Source ended before sample was created: $Source"
            }
            $output.Write($buffer, 0, $read)
            $remaining -= $read
        }
    } finally {
        $input.Dispose()
        $output.Dispose()
    }
}

function Invoke-PathEncodingSmoke {
    Write-Step "Running path encoding smoke"
    $relative = ".minecraft/versions/L_Ender's Cataclysm $([char]0x574F).jar"
    $encoded = Encode-PathArg $relative
    $payload = $Utf8NoBom.GetBytes("fixture path encoding smoke")
    $taskId = "fixture-path-smoke-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())"

    Send-AgentCommand "START_TASK $taskId" | Out-Null
    $rootReply = Send-AgentCommand "SET_TARGET_ROOT fixture-path-smoke"
    $targetRoot = ($rootReply | ConvertFrom-Json).target_root
    Send-AgentCommand "START_FILE $encoded $($payload.Length)" | Out-Null
    Send-AgentBinaryCommand "PUSH_CHUNK_BIN $encoded 0 0 $($payload.Length)" -Payload $payload | Out-Null
    Send-AgentCommand "COMPLETE_FILE $encoded" | Out-Null
    $stat = Send-AgentCommand "STAT_FILE $encoded"
    $remoteFile = "$targetRoot/$relative"
    $remoteSize = Invoke-Adb -Arguments @("shell", "stat -c %s -- $(Quote-AdbShell $remoteFile)")
    if ($remoteSize.Trim() -ne $payload.Length.ToString()) {
        throw "Path encoding smoke remote size mismatch: $remoteSize"
    }
    Write-Host $stat
}

function Invoke-IsoDualSmoke {
    if (-not (Test-Path -LiteralPath $IsoPath)) {
        throw "ISO fixture not found: $IsoPath"
    }
    Write-Step "Running same-file ADB + Wi-Fi fixture smoke"
    New-Item -ItemType Directory -Force -Path $WorkRoot | Out-Null

    $sourceInfo = Get-Item -LiteralPath $IsoPath
    if ($IsoSampleBytes -le 0 -or $IsoSampleBytes -gt [int64]$sourceInfo.Length) {
        throw "IsoSampleBytes must be between 1 and the ISO size ($($sourceInfo.Length))."
    }
    if ($IsoSampleBytes -eq [int64]$sourceInfo.Length) {
        $sample = $IsoPath
    } else {
        $sample = Join-Path $WorkRoot ("iso-first-{0}.bin" -f $IsoSampleBytes)
        New-FilePrefixSample -Source $IsoPath -Destination $sample -Bytes $IsoSampleBytes
    }
    $chunkSize = [int64]$ChunkSizeMb * 1024 * 1024
    if ($chunkSize -le 0) {
        throw "ChunkSizeMb must be greater than zero."
    }

    $localHash = if ($SkipIsoHash) {
        ""
    } else {
        (Get-FileHash -Algorithm SHA256 -LiteralPath $sample).Hash.ToLowerInvariant()
    }
    $relative = "$(Split-Path -Leaf $IsoPath).first$IsoSampleBytes"
    $encoded = Encode-PathArg $relative
    $overallTimer = [System.Diagnostics.Stopwatch]::StartNew()
    $adbBytes = [int64]0
    $wifiBytes = [int64]0
    $adbTimer = [System.Diagnostics.Stopwatch]::new()
    $wifiTimer = [System.Diagnostics.Stopwatch]::new()
    $pendingWifiChunks = @()
    $taskId = "fixture-large-dual-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())"
    Send-AgentCommand "START_TASK $taskId" | Out-Null
    $rootReply = Send-AgentCommand "SET_TARGET_ROOT fixture-large-dual-smoke"
    $targetRoot = ($rootReply | ConvertFrom-Json).target_root
    Send-AgentCommand "START_FILE $encoded $IsoSampleBytes" | Out-Null

    $remoteTemp = "$targetRoot/$relative.nekotrans-tmp"
    $remoteFinal = "$targetRoot/$relative"
    $remoteParent = $remoteTemp.Substring(0, $remoteTemp.LastIndexOf("/"))
    Invoke-Adb -Arguments @("shell", "mkdir", "-p", $remoteParent) | Out-Null

    $totalChunks = [int][Math]::Ceiling($IsoSampleBytes / [double]$chunkSize)
    $transferTimer = [System.Diagnostics.Stopwatch]::StartNew()
    $input = [System.IO.File]::OpenRead($sample)
    try {
        for ($chunkIndex = 0; $chunkIndex -lt $totalChunks; $chunkIndex++) {
            $offset = [int64]$chunkIndex * $chunkSize
            $currentChunkSize = [int][Math]::Min($chunkSize, $IsoSampleBytes - $offset)
            $payload = New-Object byte[] $currentChunkSize
            $input.Seek($offset, [System.IO.SeekOrigin]::Begin) | Out-Null
            $read = $input.Read($payload, 0, $currentChunkSize)
            if ($read -ne $currentChunkSize) {
                throw "Short read at chunk $chunkIndex."
            }

            if (($chunkIndex % 2) -eq 0) {
                $adbTimer.Start()
                if (($offset % 1048576) -eq 0) {
                    $adbBlockSize = 1048576
                    $seekBlocks = [int64]($offset / 1048576)
                } elseif (($offset % 4096) -eq 0) {
                    $adbBlockSize = 4096
                    $seekBlocks = [int64]($offset / 4096)
                } else {
                    $adbBlockSize = 1
                    $seekBlocks = $offset
                }
                $script = "dd of=$(Quote-AdbShell $remoteTemp) bs=$adbBlockSize seek=$seekBlocks conv=notrunc status=none"
                Invoke-AdbShellBinaryInput -ShellScript $script -Payload $payload | Out-Null
                $adbTimer.Stop()
                $adbBytes += $currentChunkSize
            } else {
                $pendingWifiChunks += [PSCustomObject]@{
                    ChunkIndex = $chunkIndex
                    Offset = $offset
                    Payload = $payload
                }
                if ($pendingWifiChunks.Count -ge 4) {
                    $batchBytes = [int64]0
                    foreach ($pending in $pendingWifiChunks) { $batchBytes += $pending.Payload.Length }
                    $wifiTimer.Start()
                    Send-AgentChunkBatchCommand -EncodedRelativePath $encoded -Chunks $pendingWifiChunks | Out-Null
                    $wifiTimer.Stop()
                    $wifiBytes += $batchBytes
                    $pendingWifiChunks = @()
                }
            }

            if ((($chunkIndex + 1) % 16) -eq 0 -or $chunkIndex + 1 -eq $totalChunks) {
                Write-Host "Transferred $($chunkIndex + 1)/$totalChunks chunks"
            }
        }
    } finally {
        $input.Dispose()
    }
    if ($pendingWifiChunks.Count -gt 0) {
        $batchBytes = [int64]0
        foreach ($pending in $pendingWifiChunks) { $batchBytes += $pending.Payload.Length }
        $wifiTimer.Start()
        Send-AgentChunkBatchCommand -EncodedRelativePath $encoded -Chunks $pendingWifiChunks | Out-Null
        $wifiTimer.Stop()
        $wifiBytes += $batchBytes
    }
    $transferTimer.Stop()

    Send-AgentCommand "COMPLETE_FILE $encoded" | Out-Null
    $stat = Send-AgentCommand "STAT_FILE $encoded"
    if (-not $SkipIsoHash) {
        $remoteHashLine = Invoke-Adb -Arguments @("shell", "sha256sum $(Quote-AdbShell $remoteFinal)")
        $remoteHash = ($remoteHashLine -split "\s+")[0].ToLowerInvariant()
        if ($remoteHash -ne $localHash) {
            throw "Fixture ISO sample SHA-256 mismatch. local=$localHash remote=$remoteHash"
        }
    }
    $overallTimer.Stop()
    Write-Host $stat
    if ($SkipIsoHash) {
        Write-Host "SHA256: skipped"
    } else {
        Write-Host "SHA256: $localHash"
    }
    Write-Host "ISO fixture throughput: data=$(Format-Throughput $IsoSampleBytes $transferTimer.Elapsed.TotalSeconds), end_to_end=$(Format-Throughput $IsoSampleBytes $overallTimer.Elapsed.TotalSeconds), adb=$(Format-Throughput $adbBytes $adbTimer.Elapsed.TotalSeconds), wifi=$(Format-Throughput $wifiBytes $wifiTimer.Elapsed.TotalSeconds), chunks=$totalChunks"
}

function Get-RelativePath {
    param(
        [string]$RootPath,
        [string]$FilePath
    )
    return $FilePath.Substring($RootPath.Length + 1).Replace("\", "/")
}

function Select-FolderFixtureFiles {
    param([string]$RootPath)

    $allFiles = @(Get-ChildItem -LiteralPath $RootPath -File -Recurse -Force | Sort-Object FullName)
    $maxBytes = [int64]$MaxFolderFileMb * 1024 * 1024
    $all = if ($MaxFolderFileMb -gt 0) {
        @($allFiles | Where-Object { $_.Length -le $maxBytes })
    } else {
        $allFiles
    }
    if ($FolderFileLimit -le 0) {
        $primaryLimit = [int]::MaxValue
    } else {
        $primaryLimit = $FolderFileLimit
    }

    $budgetBytes = if ($FolderByteLimitMb -gt 0) {
        [int64]$FolderByteLimitMb * 1024 * 1024
    } else {
        [int64]::MaxValue
    }
    $selected = New-Object System.Collections.Generic.List[object]
    $seen = @{}
    $selectedBytes = [int64]0

    foreach ($file in @($all | Where-Object Length -eq 0 | Select-Object -First 8)) {
        $key = $file.FullName
        if (-not $seen.ContainsKey($key)) {
            $seen[$key] = $true
            $selected.Add($file) | Out-Null
        }
    }
    foreach ($file in @($all | Select-Object -First $primaryLimit)) {
        $key = $file.FullName
        if ($seen.ContainsKey($key)) {
            continue
        }
        if ($file.Length -gt 0 -and ($selectedBytes + [int64]$file.Length) -gt $budgetBytes) {
            continue
        }
        $seen[$key] = $true
        $selected.Add($file) | Out-Null
        $selectedBytes += [int64]$file.Length
    }
    foreach ($file in @($all | Sort-Object Length -Descending | Select-Object -First 16)) {
        $key = $file.FullName
        if ($seen.ContainsKey($key)) {
            continue
        }
        if ($file.Length -gt 0 -and ($selectedBytes + [int64]$file.Length) -gt $budgetBytes) {
            continue
        }
        $seen[$key] = $true
        $selected.Add($file) | Out-Null
        $selectedBytes += [int64]$file.Length
    }

    return $selected | Sort-Object FullName -Unique
}

function Invoke-FolderSubsetSmoke {
    if (-not (Test-Path -LiteralPath $FolderPath)) {
        throw "Folder fixture not found: $FolderPath"
    }
    Write-Step "Running folder fixture Wi-Fi smoke"

    $folderRoot = (Resolve-Path -LiteralPath $FolderPath).Path
    $files = @(Select-FolderFixtureFiles -RootPath $folderRoot)
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    $taskId = "fixture-folder-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())"
    Send-AgentCommand "START_TASK $taskId" | Out-Null
    $rootReply = Send-AgentCommand "SET_TARGET_ROOT fixture-folder-smoke"
    $targetRoot = ($rootReply | ConvertFrom-Json).target_root

    if ($files.Count -eq 0) {
        throw "Folder fixture selection is empty. Increase -FolderByteLimitMb or check the fixture directory."
    }
    $totalBytes = [int64]0
    foreach ($file in $files) {
        $totalBytes += [int64]$file.Length
    }
    $largest = @($files | Sort-Object Length -Descending | Select-Object -First 1)
    $largestBytes = if ($largest.Count -gt 0) { [int64]$largest[0].Length } else { 0 }
    Write-Host "Folder fixture selection: files=$($files.Count), bytes=$totalBytes, largest=$largestBytes, budget=${FolderByteLimitMb}MiB"

    $manifest = [System.Text.StringBuilder]::new()
    $payload = [System.IO.MemoryStream]::new()
    try {
        foreach ($file in $files) {
            $relative = Get-RelativePath -RootPath $folderRoot -FilePath $file.FullName
            $encoded = Encode-PathArg $relative
            $mtime = [DateTimeOffset]::new($file.LastWriteTimeUtc).ToUnixTimeMilliseconds()
            [void]$manifest.Append("F`t$encoded`t$($file.Length)`t$mtime`n")
            if ($file.Length -gt 0) {
                $stream = [System.IO.File]::OpenRead($file.FullName)
                try {
                    $stream.CopyTo($payload)
                } finally {
                    $stream.Dispose()
                }
            }
        }

        $bundleTimer = [System.Diagnostics.Stopwatch]::StartNew()
        $reply = Send-AgentFileBundleCommand "fixture-folder-bundle-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())" $manifest.ToString() $payload.ToArray()
        $bundleTimer.Stop()
        Write-Host $reply
        $ack = $reply | ConvertFrom-Json
        if ([int]$ack.files -ne [int]$files.Count -or [int64]$ack.bytes -ne [int64]$totalBytes) {
            throw "Folder fixture bundle ACK mismatch. local_files=$($files.Count) ack_files=$($ack.files) local_bytes=$totalBytes ack_bytes=$($ack.bytes)"
        }
        Write-Host "Folder fixture bundle send: throughput=$(Format-Throughput $totalBytes $bundleTimer.Elapsed.TotalSeconds), bundle_count=1"
    } finally {
        $payload.Dispose()
    }

    $hashed = 0
    $index = 0
    $statLimit = if ($FolderStatLimit -lt 0) { 0 } else { $FolderStatLimit }
    $filesToStat = if ($statLimit -eq 0) {
        @()
    } elseif ($files.Count -le $statLimit) {
        $files
    } else {
        @($files | Select-Object -First $statLimit)
    }
    foreach ($file in $filesToStat) {
        $index += 1
        $relative = Get-RelativePath -RootPath $folderRoot -FilePath $file.FullName
        $encoded = Encode-PathArg $relative
        $stat = Send-AgentCommand "STAT_FILE $encoded" | ConvertFrom-Json
        if ([int64]$stat.size_bytes -ne [int64]$file.Length) {
            throw "Folder fixture size mismatch for $relative. local=$($file.Length) remote=$($stat.size_bytes)"
        }

        if ($hashed -lt $FolderHashLimit) {
            $remoteFile = "$targetRoot/$relative"
            $remoteHashLine = if ($file.Length -eq 0) {
                ""
            } else {
                Invoke-Adb -Arguments @("shell", "sha256sum $(Quote-AdbShell $remoteFile)")
            }
            if ($file.Length -gt 0) {
                $localHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $file.FullName).Hash.ToLowerInvariant()
                $remoteHash = ($remoteHashLine -split "\s+")[0].ToLowerInvariant()
                if ($localHash -ne $remoteHash) {
                    throw "Folder fixture SHA-256 mismatch for $relative. local=$localHash remote=$remoteHash"
                }
            }
            $hashed += 1
        }

        if (($index % 50) -eq 0 -or $index -eq $filesToStat.Count) {
            Write-Host "Folder fixture verify: $index/$($filesToStat.Count) sampled files"
        }
    }

    $timer.Stop()
    Write-Host "Folder fixture completed: $($files.Count) files, $totalBytes bytes, $($filesToStat.Count) stat check(s), $hashed hash check(s), throughput=$(Format-Throughput $totalBytes $timer.Elapsed.TotalSeconds), bundle_count=1."
}

Write-Step "Selecting Android device"
$script:DeviceSerial = Select-AdbDevice -RequestedSerial $Serial
Write-Host "Using device: $script:DeviceSerial"

Resolve-FixturePaths
Write-Host "Fixture root: $FixtureRoot"
if (-not [string]::IsNullOrWhiteSpace($IsoPath)) {
    Write-Host "ISO fixture: $IsoPath"
}
if (-not [string]::IsNullOrWhiteSpace($FolderPath)) {
    Write-Host "Folder fixture: $FolderPath"
}

if ($CleanupRemote) {
    Write-Step "Cleaning remote fixture test directories"
    $remoteBase = "/storage/emulated/0/Android/data/com.nekotrans.agent/files/wifi-skeleton"
    Invoke-Adb -Arguments @("shell", "rm", "-rf", "$remoteBase/fixture-path-smoke", "$remoteBase/fixture-large-dual-smoke", "$remoteBase/fixture-folder-smoke") | Out-Null
}

Write-Step "Launching Android activity"
Invoke-Adb -Arguments @("shell", "am", "start", "-n", $MainActivity) | Write-Host
Start-Sleep -Seconds 3

Write-Step "Forwarding local port $AgentPort"
Invoke-Adb -Arguments @("forward", "tcp:$AgentPort", "tcp:$AgentPort") | Write-Host

$wifiIp = Get-DeviceWifiIp
if (-not [string]::IsNullOrWhiteSpace($wifiIp)) {
    Write-Step "Probing direct LAN agent at ${wifiIp}:$AgentPort"
    Wait-AgentHello -PreferredHost $wifiIp | Out-Null
} else {
    Write-Step "Probing forwarded agent"
    Wait-AgentHello -PreferredHost "127.0.0.1" | Out-Null
}

if (-not $SkipPathSmoke) {
    Invoke-PathEncodingSmoke
}
if (-not $SkipIso) {
    Invoke-IsoDualSmoke
}
if (-not $SkipFolder) {
    Invoke-FolderSubsetSmoke
}

Write-Step "Fixture smoke completed"
Write-Host "Agent host: $script:AgentHost"
