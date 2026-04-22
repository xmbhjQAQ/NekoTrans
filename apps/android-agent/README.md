# Android Agent

This module contains the Android foreground-service side of Nekotrans. It is still intentionally simple, but it now has a real protocol listener and chunk/file executor for ADB-assisted and same-LAN Wi-Fi validation.

Implemented here:

- `MainActivity` for lightweight status UI
- `TransferService` for foreground execution with CPU and Wi-Fi keep-awake locks while the agent is running
- `AgentServer` line-based TCP listener on port `38997`
- `AgentCapability` JSON declaration for desktop handshake probing
- `SessionState` model for protocol-facing task state
- chunk read/write/stat/verify handlers backed by sanitized agent storage roots
- local rolling JSON logs exposed through `LOG_SNAPSHOT`

Planned follow-up:

- broader high-throughput streaming and pipelined data protocols beyond one-request-per-chunk transfers
- broader reconnect and idempotent retry semantics
- hardware restart/resume validation
- final install/test/runbook polish

## Agent Listener

Starting the foreground service also starts a small LAN listener on TCP port `38997`.
It reads one request line, returns one JSON line, then closes the socket.
`HELLO` or an empty request returns the capability JSON, `PING` returns a minimal Pong JSON, and unsupported commands return structured error JSON.
While the service is running it holds a non-reference-counted `PARTIAL_WAKE_LOCK` and a Wi-Fi keep-awake `WifiLock` so screen-off transfers are less likely to be suspended by normal Android idle behavior. The Activity also exposes an `Allow Screen-Off Network` button that opens Android's battery-optimization exemption flow. Aggressive OEM battery/network policies can still require additional vendor-specific lock-screen cleanup settings.

The listener includes these task/control commands:

- `START_TASK <task_id>` stores a task snapshot as `Running` and returns it.
- `SET_TARGET_ROOT <root_name>` selects a sanitized target storage root for subsequent file writes.
- `TASK_SNAPSHOT` returns the current task snapshot, or an `Idle` snapshot when no task exists.
- `PAUSE_TASK` changes the current task snapshot to `Paused`, or returns `Idle` when no task exists.
- `RESUME_TASK` changes the current task snapshot to `Running`, or returns `Idle` when no task exists.
- `CANCEL_TASK` marks the current task as cancelled when one exists.
- `START_FILE <relative_path> <size_bytes>` stores the current file for the active task, resets the acknowledged chunk count, and returns a `FileSnapshot`.
- `CHUNK_ACK <chunk_index>` records an acknowledged chunk index for the current file in memory and returns a `FileSnapshot`.
- `CHUNK_STATUS <relative_path> <chunk_index> <offset> <length>` reports whether a chunk is committed in memory or can be confirmed from disk.
- `PUSH_CHUNK <relative_path> <chunk_index> <offset> <base64_payload>` decodes a base64 payload, writes it to the active temp file at the requested offset, records the ack, and returns a `FileSnapshot`.
- `PUSH_CHUNK_BIN <relative_path> <chunk_index> <offset> <length>` reads exactly `<length>` raw bytes after the command line, writes them to the active temp file at the requested offset, records the ack, and returns a `FileSnapshot`.
- `PUSH_FILE_BUNDLE_BIN <bundle_id> <manifest_length> <payload_length>` reads a tab-delimited manifest followed by concatenated file bytes. Manifest rows can be `F\t<relative_path>\t<size_bytes>\t<modified_at_epoch_ms>` for files or `D\t<relative_path>` for directory creation.
- `PULL_CHUNK <relative_path> <offset> <length>` returns a base64 chunk from the requested file.
- `PULL_CHUNK_BIN <relative_path> <offset> <length>` returns a JSON header line followed by exactly the reported number of raw file bytes.
- `STAT_FILE <relative_path>` returns file existence and size metadata.
- `VERIFY_FILE <relative_path> <algorithm>` returns a BLAKE3 digest for supported requests.
- `FILE_SNAPSHOT` returns the current file snapshot, or an error when no active task/file exists.
- `COMPLETE_FILE <relative_path> <size_bytes>` finalizes the temp file into the selected target root and tolerates duplicate finalize requests.
- `LOG_SNAPSHOT` returns recent Android-side JSON log records for desktop backhaul.

`PUSH_CHUNK_BIN`, `PUSH_CHUNK_BATCH_BIN`, and `PULL_CHUNK_BIN` avoid base64 overhead for the hot chunk paths. Broader streaming/pipelining is still compatibility-oriented and should be extended before final throughput tuning.
Path arguments are UTF-8 percent-decoded by the agent, so desktop clients can preserve spaces, Unicode names, apostrophes, and other normal filename characters while still keeping the line protocol space-delimited.

## Build on Windows

Use the local Gradle install the desktop environment already has:

```powershell
& 'C:\gradle-9.4.1\bin\gradle.bat' :app:assembleDebug
```

The debug APK is expected at:

```text
apps/android-agent/app/build/outputs/apk/debug/app-debug.apk
```

After the APK exists, the desktop app can install it from the device preflight card with `Install Agent`. Opening the Activity also starts the foreground service so ADB-driven hardware smoke can run without tapping the in-app start button. Hardware validation should then grant all-files access when using external roots, probe `HELLO`/`PING`, and run PC -> Android plus Android -> PC transfer smoke tests before restart/resume tests.
