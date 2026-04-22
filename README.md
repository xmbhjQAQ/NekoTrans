# Nekotrans

Nekotrans is a greenfield Windows + Android transfer project aimed at high-throughput file delivery over ADB USB, Wi-Fi, or both at once.

This initial implementation includes:

- a Rust transfer core with resumable task models
- chunk scheduling primitives for single-track and dual-track modes
- checkpoint persistence for pause/resume and crash recovery
- structured JSON log records
- a Tauri desktop shell with ADB/Wi-Fi task creation, persisted task records, native path pickers, task controls, and log filtering/export
- real desktop ADB push/pull workers with checkpointed progress and optional BLAKE3 verification
- real desktop Wi-Fi push/pull workers over the Android agent line/base64 protocol, including checkpoint-aware resume paths
- a Dual PC -> Android worker that can split multi-file work by lane and route large same-file transfers through a shared ADB + Wi-Fi temp/finalize path
- an Android foreground agent with a TCP listener, file/chunk read/write/stat/verify commands, snapshots, and rolling logs

## Repository Layout

- `crates/transfer-core`: protocol-agnostic task, checkpoint, scheduler, and logging primitives
- `apps/desktop`: Tauri desktop shell prototype and modern frontend
- `apps/android-agent`: Android foreground-service skeleton
- `docs/ARCHITECTURE.md`: implementation notes and next milestones
- `docs/HARDWARE_VALIDATION.md`: Windows + Android smoke-test runbook
- `scripts/hardware-smoke.ps1`: repeatable ADB/agent hardware smoke script

## Local Validation

The offline-verifiable gate currently used for each iteration is:

```powershell
cargo test -p transfer-core
cargo test --manifest-path apps\desktop\src-tauri\Cargo.toml
cargo check --manifest-path apps\desktop\src-tauri\Cargo.toml
node --check apps\desktop\ui\main.js
& 'C:\gradle-9.4.1\bin\gradle.bat' :app:assembleDebug
```

Run the Gradle command from `apps/android-agent`. Full launch, install, permission, restart/resume, and high-throughput transfer validation still require Windows + Android hardware.

For hardware smoke testing after connecting an authorized Android device:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\hardware-smoke.ps1
```
