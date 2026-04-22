# Hardware Validation

Use this runbook for the remaining Windows + Android validation work. The script automates the repeatable parts and fails early when no authorized Android device is attached.

## Prerequisites

- Windows host with `adb` on `PATH`.
- Android phone with USB debugging enabled and the RSA prompt accepted.
- Local Gradle install at `C:\gradle-9.4.1\bin\gradle.bat`, or pass `-Gradle <path>`.
- For external target roots, grant all-files access in the Android UI if the best-effort `appops` grant does not apply on the device.
- If screen-off LAN traffic still stalls on a specific OEM ROM, tap `Allow Screen-Off Network` in Nekotrans Agent and also exempt the app from any vendor battery optimization / lock-screen cleanup screen before treating it as a transfer protocol failure.

## One-Shot Smoke

From the repository root:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\hardware-smoke.ps1
```

When multiple devices are connected:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\hardware-smoke.ps1 -Serial <adb-serial>
```

The smoke script performs:

- Android debug APK build and reinstall.
- Activity and foreground service launch.
- Best-effort notification and all-files app-op permission grants.
- `adb forward tcp:38997 tcp:38997` for the Android agent.
- `HELLO` and `PING` through the forwarded port, followed by direct LAN `HELLO` when a Wi-Fi IP is discoverable.
- `START_TASK`, `SET_TARGET_ROOT`, `START_FILE`, raw binary `PUSH_CHUNK_BIN`, binary batch `PUSH_CHUNK_BATCH_BIN`, small-file/directory bundle `PUSH_FILE_BUNDLE_BIN`, `CHUNK_STATUS`, `COMPLETE_FILE`, `STAT_FILE`, base64 `PULL_CHUNK`, binary `PULL_CHUNK_BIN`, `VERIFY_FILE`, and `LOG_SNAPSHOT` protocol checks. The bundle check includes directory-only manifest rows. When direct LAN probing succeeds, the file protocol smoke uses the phone's LAN IP instead of the forwarded port.
- Android agent `force-stop`/restart followed by disk-backed `CHUNK_STATUS` recovery probing.
- Same-file ADB + Wi-Fi convergence smoke: deterministic 4 MiB local file, even chunks written by ADB into the agent temp file, odd chunks written by Wi-Fi `PUSH_CHUNK_BIN`, agent `COMPLETE_FILE`, then ADB pull-back and SHA-256 comparison.
- Raw ADB large-file push size validation to `/sdcard/Download/NekotransSmoke/adb-large.bin`.
- Agent pause/resume command smoke.

## Screen-Off LAN Probe

After the foreground service is running, confirm `dumpsys power` contains `Nekotrans:TransferWakeLock` and `dumpsys wifi` contains `Nekotrans:TransferWifiLock`, then turn the phone screen off and probe direct LAN `HELLO` from the PC. If direct LAN stalls while ADB-forward probing still works, use the in-app `Allow Screen-Off Network` button and the OEM battery / lock-screen cleanup settings before investigating the transfer protocol.

## Real Fixture Set

The repository-local `测试文件` fixture set is intended for heavier validation:

- `测试项目1(windows镜像).iso`: single large-file fixture, 8,543,608,832 bytes.
- `测试项目2(大文件夹)`: large directory fixture, 26,358 files, 2,025 directories, 12,384,839,348 bytes.

Validated so far on device `6bdab3c9`:

- Path encoding smoke preserved `.minecraft/versions/L_Ender's Cataclysm 坏.jar` on Android, confirming spaces, apostrophes, and Unicode names survive the agent protocol.
- A 1 GiB slice of `测试项目1(windows镜像).iso` transferred through same-file ADB + Wi-Fi convergence with 128 x 8 MiB chunks and matching SHA-256 on PC and Android.
- The large-file fixture smoke now uses `adb exec-in sh -c dd` for ADB binary stdin writes. Plain `adb shell` is avoided for this path because it can close stdin early on Windows with random binary payloads.
- An 8.54GB ISO smoke completed with 64MiB chunks, hash skipped for pure transfer timing, and remote size matching `8,543,608,832` bytes. Best current script result: `data=68.23MB/s`, `end_to_end=67.92MB/s`, `adb=105.35MB/s`, `wifi=52.35MB/s`, `chunks=128`.
- Interpretation: the fixture script still schedules ADB and Wi-Fi sequentially, so its total throughput is not the final concurrent aggregate. The lane timers are still useful: Wi-Fi is currently the weak lane even though the phone reports 5GHz 11ac, RSSI around `-33dBm`, and Tx/Rx link speed `1733Mbps`.
- Rust/product-path benchmark `fixture_bench` avoids PowerShell data-plane overhead and runs ADB + Wi-Fi concurrently. On the 8.54GB ISO, fixed 50/50 chunk ownership reached `data=102.09MB/s`, `adb=132.48MB/s`, `wifi=51.81MB/s`; ADB-heavy 3:1 ownership reached `data=136.45MB/s`, `adb=102.23MB/s`, `wifi=44.12MB/s`, with final remote size matching.
- Product same-file Dual now performs startup lane calibration before the large-file worker begins: it samples the first large source file through ADB `exec-in` and Wi-Fi `PUSH_CHUNK_BIN`, removes the calibration files, and derives `wifi_stride` from measured lane rates. If calibration fails, it falls back to the validated ADB-heavy 3:1 plan. Use `NEKOTRANS_DUAL_WIFI_STRIDE` to force a stride and `NEKOTRANS_DUAL_WIFI_BATCH_CHUNKS` to cap Wi-Fi batch size during experiments.
- `scripts/hardware-smoke.ps1 -SkipBuild -Serial 6bdab3c9` completed after the binary protocol updates, covering APK install/launch, forwarded-agent probing, `PUSH_CHUNK_BIN`, base64 `PULL_CHUNK`, binary `PULL_CHUNK_BIN`, `PUSH_FILE_BUNDLE_BIN` directory rows, same-file ADB + Wi-Fi convergence, disk-backed `CHUNK_STATUS` after agent restart, raw ADB 64MiB push, and `PAUSE_TASK`/`RESUME_TASK`.
- `scripts/desktop-worker-smoke.ps1 -Serial 6bdab3c9 -AgentHost 192.168.11.102 -Cleanup -SkipBuild` completed after the headless desktop worker harness was added. It ran persisted `transfer-core` tasks through the desktop ADB-only worker (`files=2 bytes=76`), Wi-Fi-only worker (`files=2 bytes=76`), and Dual worker (`files=3 bytes=34,603,084`) with BLAKE3 verification and checkpoint completion assertions.
- ADB-forwarded binary protocol clients should not half-close the write side before reading the response. The Android agent reads exact byte counts, and both the Rust client and smoke scripts keep the socket open until the response is received to avoid empty replies over ADB forward.
- Observed LAN usage can look like a sine/sawtooth wave because Wi-Fi currently sends framed batches, waits for Android write/ACK, then sends the next batch. Do not treat that graph as a pure Wi-Fi PHY limit until the benchmark separates socket send, Android write, ACK wait, and local read time.
- Safety caveat: very small Wi-Fi batches and some chunk-weight patterns produced short final files in Rust bench. This points to a possible file-length race when ADB `dd` and Android `RandomAccessFile` write the same emulated-storage file concurrently. Keep size/hash validation enabled for new scheduling experiments.
- Directory fixture smoke now uses the small-file bundle path instead of per-file chunk pushes. A 64MiB-budget run selected 265 files / 65,462,179 bytes, sent one `PUSH_FILE_BUNDLE_BIN`, validated ACK file/byte totals, sampled 16 `STAT_FILE` checks, and completed in about 11 seconds.

Performance target:

- USB2.0 is expected to be roughly 20MB/s+ in good conditions, USB3.0 can approach 500MB/s, and gigabit Wi-Fi is roughly 125MB/s theoretical.
- The PC -> Android ADB+Wi-Fi aggregate target is 200MB/s+. If validation falls below that, first separate data-plane send time from validation time, then attribute the bottleneck to ADB, Wi-Fi, Android UFS/filesystem writes, or protocol overhead.

Fixture smoke example:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\fixture-smoke.ps1 -Serial 6bdab3c9 -SkipIso -SkipPathSmoke -FolderByteLimitMb 64 -FolderStatLimit 16 -FolderHashLimit 0 -CleanupRemote
```

Rust concurrent benchmark example:

```powershell
$iso = (Get-ChildItem -LiteralPath .\测试文件 -File -Recurse -Filter *.iso | Select-Object -First 1).FullName
.\apps\desktop\src-tauri\target\debug\fixture_bench.exe --serial 6bdab3c9 --source $iso --bytes 8543608832 --chunk-mb 64 --wifi-every 4 --wifi-batch-chunks 4 --cleanup
```

## Desktop Launch Smoke

The desktop debug executable can be built and briefly launched with:

```powershell
cargo build --manifest-path apps\desktop\src-tauri\Cargo.toml
.\apps\desktop\src-tauri\target\debug\nekotrans-desktop.exe
```

A successful launch means the process starts and remains alive long enough for the Tauri window to initialize. Close the window after confirming the dashboard appears.

## Still Manual

Some validation remains intentionally human-observed:

- Confirm the Android notification permission dialog state when first launching on Android 13+.
- Confirm all-files access in Android Settings for external absolute target roots.
- Repeat the headless desktop worker smoke after worker/checkpoint changes:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\desktop-worker-smoke.ps1 -Serial 6bdab3c9 -AgentHost 192.168.11.102 -Cleanup
```

- Use the desktop UI to create and run full ADB-only, Wi-Fi-only, and Dual tasks, then close/reopen the app for resume validation. The headless smoke covers the worker/checkpoint path, but the visible UI recovery flow still needs human observation.
- Run at least one long Wi-Fi or Dual transfer with the phone screen off on each target OEM skin, because vendor battery policies can be stricter than stock Android.
- For verify-on corruption testing, intentionally mutate a target file between transfer and verification and confirm the desktop marks the task failed.
