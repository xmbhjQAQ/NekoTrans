# Architecture Notes

## Current Baseline

The repo is organized around a protocol-agnostic transfer core. The desktop app owns task creation, device orchestration, and operator-facing logs. The Android app acts as a foreground transfer agent.

## Transfer Model

- A task expands user-selected files and directories into `TransferItem` entries.
- Files larger than the chunk threshold are broken into dynamically sized chunks for same-file Dual PC -> Android transfers: 8MiB below 1GiB, 32MiB from 1-4GiB, and 64MiB at 4GiB+.
- Small files are grouped into binary bundles instead of being transferred as one protocol transaction per file. The current PC -> Android bundle target is 64MiB or 4096 files, with a final partial bundle flushed immediately. Bundle manifests also carry directory rows so empty directories are created before file payloads.
- The scheduler assigns chunks to `adb` and `wifi` lanes based on requested mode and lane health.
- Completed chunks are committed to a checkpoint store so the task can survive pause, resume, or an application crash.

## What Is Implemented

- task and transfer item data structures
- chunk scheduler and lane assignment policy
- pause/resume-safe checkpoint file format
- JSON-line logging helpers
- durable desktop task JSON records under `.nekotrans/tasks`
- desktop startup recovery for persisted task records and checkpoints
- desktop ADB device discovery, preflight, APK install, push/pull workers, pause/resume/cancel/retry controls, and optional BLAKE3 verification
- desktop Wi-Fi push/pull workers over the Android agent protocol, including UTF-8 percent-encoded path arguments, raw binary push chunks, binary chunk batches, binary pull chunks with base64 compatibility fallback, small-file bundles with directory manifest rows, checkpoint-aware resume, and chunk-status recovery. Binary request clients keep the socket write side open until the response is read because ADB forward can return empty replies when the client half-closes first.
- Dual PC -> Android transfer for multi-file tasks plus a same-file ADB + Wi-Fi convergence path where both lanes write different offsets into the Android agent temp file and the agent performs the final rename; ADB same-file writes now stream local file ranges through `adb exec-in sh -c dd` instead of staging local and remote part files
- Same-file Dual now performs a lightweight startup calibration against the first large file and target root, then derives Wi-Fi chunk stride from measured ADB/Wi-Fi lane throughput. If calibration fails, it falls back to the validated ADB-heavy 3:1 ownership plan. Operators can override with `NEKOTRANS_DUAL_WIFI_STRIDE` and `NEKOTRANS_DUAL_WIFI_BATCH_CHUNKS`.
- desktop log filtering/export plus Android log backhaul and dedupe into `.nekotrans/logs`
- Android foreground service with CPU/Wi-Fi keep-awake locks, a battery-optimization exemption entry point, TCP listener, task/file/chunk commands, file stat, BLAKE3 verify, snapshots, and rolling logs

## Recommended Next Steps

1. Validate the desktop app launch, APK install, Android permission grant, and device preflight loop on real Windows + Android hardware.
2. Run full desktop UI worker validation for ADB, Wi-Fi, multi-file Dual, and same-file Dual transfers, including close/reopen recovery.
3. Validate startup-calibrated same-file Dual on the 8.54GB / 12.38GB fixtures, then keep scheduling chunks toward the faster lane until both lanes finish at roughly the same time.
4. Tune PC -> Android throughput toward 200MB/s+ so the practical bottleneck is Android UFS / filesystem write speed rather than TCP framing, ADB staging, or protocol round trips.
5. Extend the current one-request-per-chunk binary paths into streaming/pipelined transfer protocols and harden bundle resume semantics.
6. Harden same-file Dual resume semantics beyond the current safe overwrite path, including richer sparse/hole detection, lane-level retry telemetry, and validation for possible file-length races when ADB and the Android app write the same emulated-storage file concurrently.
7. Add hardware-backed full 8.54GB large-file, 12.38GB directory, pause/close/reopen/resume, and verify-on corruption validation.
8. Validate long-running screen-off transfers on target OEM devices and document any required battery-optimization exclusions.

The repeatable parts of the hardware smoke are scripted in `scripts/hardware-smoke.ps1`; see `docs/HARDWARE_VALIDATION.md`.
