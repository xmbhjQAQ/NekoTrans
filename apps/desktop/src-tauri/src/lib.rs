mod adb;
mod agent;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{Manager, State};
use tauri_plugin_dialog::{DialogExt, FilePath};
use transfer_core::models::is_large_file;
use transfer_core::{
    CheckpointEntry, ChunkDescriptor, Direction, EngineTaskSnapshot, LaneAssignment, LogLevel,
    LogRecord, LogScope, TaskConfig, TaskState, TransferEngine, TransferUpdate, TransportMode,
};

#[tauri::command]
fn bootstrap_dashboard(engine: State<'_, DesktopEngine>) -> Result<DashboardState, String> {
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    engine.ensure_demo_task().map_err(|err| err.to_string())?;
    recover_persisted_task_records(&mut engine)?;
    let snapshots = engine.snapshots();
    let mut sample_logs = engine
        .log_lines("demo-task")
        .map_err(|err| err.to_string())?;
    let recoverable_tasks = recoverable_task_ids(&engine)?;
    let (devices, discovery_log) = discover_device_cards();
    sample_logs.splice(0..0, discovery_log);

    Ok(DashboardState {
        app_name: "Nekotrans".to_string(),
        transport_modes: vec![
            "ADB-only".to_string(),
            "Wi-Fi-only".to_string(),
            "Dual Track".to_string(),
        ],
        devices,
        tasks: snapshots.into_iter().map(TaskCard::from).collect(),
        recoverable_tasks,
        sample_logs,
    })
}

#[tauri::command]
fn sample_task_template(engine: State<'_, DesktopEngine>) -> Result<TaskDraft, String> {
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let snapshot = engine.ensure_demo_task().map_err(|err| err.to_string())?;

    Ok(TaskDraft {
        task_id: snapshot.task_id,
        direction: display_direction(snapshot.direction),
        transport_mode: display_transport_mode(snapshot.transport_mode),
        verify_enabled: snapshot.verify_enabled,
        chunk_size_mb: 8,
    })
}

#[tauri::command]
fn tick_demo_task(engine: State<'_, DesktopEngine>) -> Result<TaskCard, String> {
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let update: TransferUpdate = engine
        .tick_task("demo-task", 3)
        .map_err(|err| err.to_string())?;
    Ok(TaskCard::from(update.snapshot))
}

#[tauri::command]
fn pause_demo_task(engine: State<'_, DesktopEngine>) -> Result<TaskCard, String> {
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let snapshot = engine
        .pause_task("demo-task")
        .map_err(|err| err.to_string())?;
    Ok(TaskCard::from(snapshot))
}

#[tauri::command]
fn resume_demo_task(engine: State<'_, DesktopEngine>) -> Result<TaskCard, String> {
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let snapshot = engine
        .resume_task("demo-task")
        .map_err(|err| err.to_string())?;
    Ok(TaskCard::from(snapshot))
}

#[tauri::command]
fn list_task_logs(engine: State<'_, DesktopEngine>) -> Result<Vec<String>, String> {
    let engine = engine.0.lock().map_err(|err| err.to_string())?;
    let mut lines = Vec::new();
    for snapshot in engine.snapshots() {
        let mut task_lines = engine
            .log_lines(&snapshot.task_id)
            .map_err(|err| err.to_string())?;
        lines.append(&mut task_lines);
    }
    Ok(lines)
}

#[tauri::command]
fn create_demo_local_task(engine: State<'_, DesktopEngine>) -> Result<TaskCard, String> {
    let root = std::env::current_dir()
        .map_err(|err| err.to_string())?
        .join("docs");
    let config = TaskConfig::new(
        "local-docs-task",
        Direction::PcToAndroid,
        TransportMode::AdbOnly,
        false,
        root,
        "/sdcard/NekotransDocs",
    );

    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let snapshot = engine
        .create_task_from_paths(config, &[PathBuf::from(".")])
        .map_err(|err| err.to_string())?;
    Ok(TaskCard::from(snapshot))
}

#[tauri::command]
fn create_task_from_draft(
    draft: TaskCreateDraft,
    engine: State<'_, DesktopEngine>,
) -> Result<TaskCard, String> {
    create_transfer_task(draft, engine)
}

#[tauri::command]
fn create_transfer_task(
    draft: TaskCreateDraft,
    engine: State<'_, DesktopEngine>,
) -> Result<TaskCard, String> {
    let source_path_text = draft.source_path.trim();
    if source_path_text.is_empty() {
        return Err("source path is required".to_string());
    }
    let direction = parse_direction_label(&draft.direction)?;
    let source_path = PathBuf::from(source_path_text);
    if direction == Direction::PcToAndroid && !source_path.exists() {
        return Err(format!(
            "source path does not exist: {}",
            source_path.display()
        ));
    }
    let target_root = draft.target_root.trim();
    if target_root.is_empty() {
        return Err("target root is required".to_string());
    }
    let transport_mode = parse_transport_label(&draft.transport_mode)?;
    validate_task_draft_inputs(&draft, direction, transport_mode)?;

    let (source_root, selected_paths) = if direction == Direction::AndroidToPc {
        (PathBuf::from("."), vec![PathBuf::from(".")])
    } else if source_path.is_file() {
        let parent = source_path
            .parent()
            .ok_or_else(|| format!("source file has no parent: {}", source_path.display()))?
            .to_path_buf();
        let file_name = source_path
            .file_name()
            .ok_or_else(|| format!("source file has no name: {}", source_path.display()))?;
        (parent, vec![PathBuf::from(file_name)])
    } else {
        (source_path.clone(), vec![PathBuf::from(".")])
    };

    let task_id = format!(
        "draft-{}-{}",
        sanitize_task_id(
            source_path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("task")
        ),
        epoch_ms()
    );
    let config = TaskConfig::new(
        &task_id,
        direction,
        transport_mode,
        draft.verify_enabled,
        source_root,
        target_root,
    );
    let mut config = config;
    if let Some(chunk_size_bytes) = draft.chunk_size_bytes.filter(|value| *value > 0) {
        config.chunk_size_bytes = chunk_size_bytes;
    }
    if let Some(limit) = draft
        .max_in_flight_chunks_per_lane
        .filter(|value| *value > 0)
    {
        config.max_in_flight_chunks_per_lane = limit;
    }
    if direction == Direction::PcToAndroid && transport_mode == TransportMode::Dual {
        if let Some(chunk_size_bytes) =
            same_file_dual_task_chunk_size(&source_path, config.chunk_size_bytes)?
        {
            config.chunk_size_bytes = chunk_size_bytes;
        }
    }
    let actual_chunk_size_bytes = config.chunk_size_bytes;

    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let android_source_size = if direction == Direction::AndroidToPc {
        match (
            draft.source_size_bytes,
            draft
                .device_serial
                .as_deref()
                .filter(|value| !value.trim().is_empty()),
        ) {
            (Some(size), _) => size,
            (None, Some(serial)) => adb::stat_remote_file_size(serial, source_path_text)
                .map_err(|err| err.to_string())?
                .unwrap_or(0),
            (None, None) => 0,
        }
    } else {
        0
    };

    let snapshot = if direction == Direction::AndroidToPc {
        engine
            .create_task(
                config,
                vec![transfer_core::TransferItem {
                    relative_path: PathBuf::from(source_path_text),
                    size_bytes: android_source_size,
                    modified_at_epoch_ms: epoch_ms(),
                    fingerprint: None,
                }],
            )
            .map_err(|err| err.to_string())?
    } else {
        engine
            .create_or_recover_task_from_paths(config, &selected_paths)
            .map_err(|err| err.to_string())?
    };
    let mut record = TaskFileRecord::from_draft(&draft, &snapshot.task_id);
    record.chunk_size_bytes = Some(actual_chunk_size_bytes);
    record.persist()?;
    Ok(TaskCard::from(snapshot))
}

#[tauri::command]
fn list_tasks(engine: State<'_, DesktopEngine>) -> Result<Vec<TaskCard>, String> {
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    recover_persisted_task_records(&mut engine)?;
    Ok(engine.snapshots().into_iter().map(TaskCard::from).collect())
}

#[tauri::command]
fn cancel_transfer_task(
    task_id: String,
    engine: State<'_, DesktopEngine>,
    registry: State<'_, AdbTransferRegistry>,
) -> Result<TaskCard, String> {
    if let Ok(mut transfers) = registry.0.lock() {
        if let Some(entry) = transfers.get_mut(&task_id) {
            entry.pause_requested.store(true, Ordering::Relaxed);
            entry.view.state = "Cancelled".to_string();
            entry.view.last_event = "cancelled".to_string();
            entry.view.last_message =
                "cancel requested; stopping the active worker at the next safe boundary"
                    .to_string();
        }
    }
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let snapshot = engine
        .cancel_task(&task_id)
        .map_err(|err| err.to_string())?;
    Ok(TaskCard::from(snapshot))
}

#[tauri::command]
fn retry_transfer_task(
    task_id: String,
    engine: State<'_, DesktopEngine>,
) -> Result<TaskCard, String> {
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let snapshot = engine.retry_task(&task_id).map_err(|err| err.to_string())?;
    Ok(TaskCard::from(snapshot))
}

#[tauri::command]
fn pause_transfer_task(
    task_id: String,
    engine: State<'_, DesktopEngine>,
    registry: State<'_, AdbTransferRegistry>,
) -> Result<TaskCard, String> {
    if let Ok(mut transfers) = registry.0.lock() {
        if let Some(entry) = transfers.get_mut(&task_id) {
            entry.pause_requested.store(true, Ordering::Relaxed);
            entry.view.state = "Pausing".to_string();
            entry.view.last_event = "pause-requested".to_string();
            entry.view.last_message =
                "pause requested; waiting for current chunk boundary".to_string();
        }
    }
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let snapshot = engine.pause_task(&task_id).map_err(|err| err.to_string())?;
    Ok(TaskCard::from(snapshot))
}

#[tauri::command]
fn resume_transfer_task(
    task_id: String,
    registry: State<'_, AdbTransferRegistry>,
    engine: State<'_, DesktopEngine>,
) -> Result<AdbTransferCard, String> {
    if let Ok(mut engine) = engine.0.lock() {
        let _ = engine.resume_task(&task_id);
    }
    start_transfer_task(task_id, registry, engine)
}

#[tauri::command]
fn recover_task(task_id: String, engine: State<'_, DesktopEngine>) -> Result<TaskCard, String> {
    let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
    let snapshot = engine
        .recover_task(&task_id)
        .map_err(|err| err.to_string())?;
    Ok(TaskCard::from(snapshot))
}

#[tauri::command]
fn delete_transfer_task(
    task_id: String,
    engine: State<'_, DesktopEngine>,
    registry: State<'_, AdbTransferRegistry>,
) -> Result<(), String> {
    if task_id == "demo-task" {
        return Ok(());
    }

    {
        let transfers = registry.0.lock().map_err(|err| err.to_string())?;
        if let Some(entry) = transfers.get(&task_id) {
            if entry.view.state == "Running" || entry.view.state == "Pausing" {
                return Err("任务正在运行，请先取消后再删除记录。".to_string());
            }
        }
    }

    {
        let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
        engine
            .delete_task_record(&task_id)
            .map_err(|err| err.to_string())?;
    }

    TaskFileRecord::delete(&task_id)?;
    if let Ok(mut transfers) = registry.0.lock() {
        transfers.remove(&task_id);
    }
    Ok(())
}

fn recover_persisted_task_records(engine: &mut TransferEngine) -> Result<(), String> {
    for task_id in TaskFileRecord::list_ids()? {
        if engine
            .snapshots()
            .iter()
            .any(|snapshot| snapshot.task_id == task_id)
        {
            continue;
        }
        let _ = engine.recover_task(&task_id);
    }
    Ok(())
}

fn recoverable_task_ids(engine: &TransferEngine) -> Result<Vec<String>, String> {
    let mut ids = engine
        .recoverable_tasks()
        .map_err(|err| err.to_string())?
        .into_iter()
        .collect::<BTreeSet<_>>();
    for task_id in TaskFileRecord::list_ids()? {
        ids.insert(task_id);
    }
    Ok(ids.into_iter().collect())
}

#[tauri::command]
fn install_agent(serial: String) -> Result<String, String> {
    let apk_path = default_agent_apk_path()?;
    let apk_path_string = apk_path.to_string_lossy().to_string();

    adb::install_agent_apk(&serial, &apk_path_string).map_err(|err| err.to_string())
}

fn default_agent_apk_path() -> Result<PathBuf, String> {
    let current_dir = std::env::current_dir().map_err(|err| err.to_string())?;
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let apk_tail = PathBuf::from("app")
        .join("build")
        .join("outputs")
        .join("apk")
        .join("debug")
        .join("app-debug.apk");
    let repo_relative_apk = PathBuf::from("apps").join("android-agent").join(&apk_tail);
    let desktop_sibling_apk = PathBuf::from("..").join("android-agent").join(&apk_tail);

    let candidates = [
        current_dir.join(&repo_relative_apk),
        current_dir.join(&desktop_sibling_apk),
        manifest_dir.join(&desktop_sibling_apk),
        manifest_dir.join("..").join("..").join(&repo_relative_apk),
    ];

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    let tried = candidates
        .iter()
        .map(|path| format!("  - {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "Android agent APK was not found. Build it first, then retry Install Agent. Tried:\n{tried}"
    ))
}

#[tauri::command]
fn start_adb_docs_push(
    serial: String,
    registry: State<'_, AdbTransferRegistry>,
    engine: State<'_, DesktopEngine>,
) -> Result<AdbTransferCard, String> {
    let docs_path = default_docs_path()?;
    let task_id = format!("adb-docs-{}", sanitize_task_id(&serial));
    {
        let config = TaskConfig::new(
            &task_id,
            Direction::PcToAndroid,
            TransportMode::AdbOnly,
            false,
            docs_path.clone(),
            "/sdcard/NekotransDocs",
        );
        let mut engine = engine.0.lock().map_err(|err| err.to_string())?;
        engine
            .create_or_recover_task_from_paths(config, &[PathBuf::from(".")])
            .map_err(|err| err.to_string())?;
    }

    let record = TaskFileRecord {
        task_id,
        source_path: docs_path.to_string_lossy().to_string(),
        target_root: "/sdcard/NekotransDocs".to_string(),
        direction: "PC -> Android".to_string(),
        transport_mode: "ADB-only".to_string(),
        verify_enabled: false,
        source_size_bytes: None,
        chunk_size_bytes: Some(8 * 1024 * 1024),
        max_in_flight_chunks_per_lane: Some(4),
        device_serial: Some(serial),
        agent_host: None,
        target_path_policy: Some("preserve_relative".to_string()),
        created_at_epoch_ms: epoch_ms(),
    };
    record.persist()?;
    start_adb_transfer_from_record(record, registry, engine)
}

#[tauri::command]
fn start_transfer_task(
    task_id: String,
    registry: State<'_, AdbTransferRegistry>,
    engine: State<'_, DesktopEngine>,
) -> Result<AdbTransferCard, String> {
    let record = TaskFileRecord::load(&task_id)?;
    let direction = parse_direction_label(&record.direction)?;
    let mode = parse_transport_label(&record.transport_mode)?;
    match (direction, mode) {
        (Direction::PcToAndroid, TransportMode::AdbOnly) => {
            start_adb_transfer_from_record(record, registry, engine)
        }
        (Direction::PcToAndroid, TransportMode::WifiOnly) => {
            start_wifi_transfer_from_record(record, registry, engine)
        }
        (Direction::PcToAndroid, TransportMode::Dual) => {
            if record.device_serial.is_some() && record.agent_host.is_some() {
                start_dual_pc_to_android_from_record(record, registry, engine)
            } else if record.device_serial.is_some() {
                start_adb_transfer_from_record(record, registry, engine)
            } else {
                start_wifi_transfer_from_record(record, registry, engine)
            }
        }
        (Direction::AndroidToPc, TransportMode::AdbOnly) => {
            start_adb_pull_from_record(record, registry, engine)
        }
        (Direction::AndroidToPc, TransportMode::WifiOnly) => {
            start_wifi_pull_from_record(record, registry, engine)
        }
        (Direction::AndroidToPc, TransportMode::Dual) => {
            if record.agent_host.is_some() {
                start_wifi_pull_from_record(record, registry, engine)
            } else {
                start_adb_pull_from_record(record, registry, engine)
            }
        }
    }
}

fn start_dual_pc_to_android_from_record(
    mut record: TaskFileRecord,
    registry: State<'_, AdbTransferRegistry>,
    engine: State<'_, DesktopEngine>,
) -> Result<AdbTransferCard, String> {
    if parse_direction_label(&record.direction)? != Direction::PcToAndroid {
        return Err("Dual worker currently supports PC -> Android tasks only".to_string());
    }

    let serial = record
        .device_serial
        .clone()
        .ok_or_else(|| "device_serial is required for Dual transfer".to_string())?;
    let host = record
        .agent_host
        .clone()
        .ok_or_else(|| "agent_host is required for Dual transfer".to_string())?;
    let local_path = PathBuf::from(&record.source_path);
    let manifest = collect_local_transfer_manifest(&local_path)?;
    let directories = manifest.directories;
    let files = manifest.files;
    let (adb_files, wifi_files, same_file_dual_files) = partition_dual_transfer_files(files);
    if directories.is_empty() && wifi_files.is_empty() && same_file_dual_files.is_empty() {
        return start_adb_transfer_from_record(record, registry, engine);
    }
    if let Some(required_chunk_size) = same_file_dual_files
        .iter()
        .map(|file| {
            dual_same_file_chunk_size(
                file.size_bytes,
                record.chunk_size_bytes.unwrap_or(8 * 1024 * 1024),
            )
        })
        .max()
    {
        if record.chunk_size_bytes.unwrap_or(8 * 1024 * 1024) != required_chunk_size {
            if let Ok(mut engine) = engine.0.lock() {
                let _ = engine.reconfigure_task_chunk_size(&record.task_id, required_chunk_size);
            }
            record.chunk_size_bytes = Some(required_chunk_size);
            let _ = record.persist();
        }
    }

    let task_id = record.task_id.clone();
    let registry_handle = registry.0.clone();
    let engine_handle = engine.0.clone();
    let pause_requested = Arc::new(AtomicBool::new(false));
    let aggregate = Arc::new(Mutex::new(DualTransferAggregate::new(
        adb_files.len() + wifi_files.len() + same_file_dual_files.len(),
        adb_files.iter().map(|file| file.size_bytes).sum::<u64>()
            + wifi_files.iter().map(|file| file.size_bytes).sum::<u64>()
            + same_file_dual_files
                .iter()
                .map(|file| file.size_bytes)
                .sum::<u64>(),
    )));
    let lane_label = format!("dual:{serial}+wifi:{host}");
    let view = AdbTransferCard::new(&task_id, &lane_label, &record.target_root);

    {
        let mut transfers = registry_handle.lock().map_err(|err| err.to_string())?;
        if let Some(existing) = transfers.get(&task_id) {
            if existing.view.state == "Running" || existing.view.state == "Pausing" {
                return Ok(existing.view.clone());
            }
        }

        transfers.insert(
            task_id.clone(),
            AdbTransferEntry {
                view: view.clone(),
                pause_requested: pause_requested.clone(),
            },
        );
    }

    let worker_task_id = task_id.clone();
    let worker_registry = registry_handle.clone();
    let worker_engine = engine_handle.clone();
    let worker_target_root = record.target_root.clone();
    let worker_verify_enabled = record.verify_enabled;
    let worker_chunk_size = record.chunk_size_bytes.unwrap_or(8 * 1024 * 1024);
    let wifi_chunk_size = dual_wifi_chunk_size(worker_chunk_size);
    thread::spawn(move || {
        let same_file_dual_files = same_file_dual_files;
        let adb_pause = adb::AdbTransferControl::new(pause_requested.clone());
        let adb_registry = worker_registry.clone();
        let adb_engine = worker_engine.clone();
        let adb_aggregate = aggregate.clone();
        let adb_task_id = worker_task_id.clone();
        let adb_serial = serial.clone();
        let adb_target_root = worker_target_root.clone();
        let adb_handle = thread::spawn(move || {
            run_dual_adb_pc_to_android_files(
                &adb_serial,
                &adb_task_id,
                &adb_target_root,
                adb_files,
                worker_chunk_size,
                worker_verify_enabled,
                adb_pause,
                adb_registry,
                adb_engine,
                adb_aggregate,
            )
        });

        let wifi_registry = worker_registry.clone();
        let wifi_engine = worker_engine.clone();
        let wifi_aggregate = aggregate.clone();
        let wifi_task_id = worker_task_id.clone();
        let wifi_host = host.clone();
        let wifi_target_root = worker_target_root.clone();
        let wifi_pause = pause_requested.clone();
        let wifi_directories = directories;
        let wifi_handle = thread::spawn(move || {
            run_dual_wifi_pc_to_android_files(
                &wifi_host,
                &wifi_task_id,
                &wifi_target_root,
                wifi_directories,
                wifi_files,
                wifi_chunk_size,
                worker_verify_enabled,
                wifi_pause,
                wifi_registry,
                wifi_engine,
                wifi_aggregate,
            )
        });

        let adb_result = adb_handle
            .join()
            .unwrap_or_else(|_| Err("ADB dual worker panicked".to_string()));
        let wifi_result = wifi_handle
            .join()
            .unwrap_or_else(|_| Err("Wi-Fi dual worker panicked".to_string()));

        let same_file_result = if same_file_dual_files.is_empty() {
            Ok("same-file Dual lane skipped".to_string())
        } else {
            run_dual_same_file_pc_to_android_files(
                &serial,
                &host,
                &worker_task_id,
                &worker_target_root,
                same_file_dual_files,
                worker_chunk_size,
                worker_verify_enabled,
                pause_requested.clone(),
                worker_registry.clone(),
                worker_engine.clone(),
                aggregate.clone(),
            )
        };

        let result = match (adb_result, wifi_result, same_file_result) {
            (Ok(adb_message), Ok(wifi_message), Ok(same_file_message)) => Ok(format!(
                "Dual transfer completed.\nADB: {adb_message}\nWi-Fi: {wifi_message}\nSame-file: {same_file_message}"
            )),
            (Err(adb_error), _, _) => Err(adb_error),
            (_, Err(wifi_error), _) => Err(wifi_error),
            (_, _, Err(same_file_error)) => {
                Err(format!("same-file Dual transfer failed: {same_file_error}"))
            }
        };

        settle_worker_result(
            &worker_registry,
            &worker_engine,
            &worker_task_id,
            &result,
            "dual-completed",
        );
    });

    Ok(view)
}

fn settle_worker_result(
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: &Arc<Mutex<TransferEngine>>,
    task_id: &str,
    result: &Result<String, String>,
    completed_event: &str,
) {
    let cancelled = task_is_cancelled(engine, task_id);
    if let Ok(mut transfers) = registry.lock() {
        if let Some(entry) = transfers.get_mut(task_id) {
            match result {
                _ if cancelled => {
                    entry.view.state = "Cancelled".to_string();
                    entry.view.last_event = "cancelled".to_string();
                    entry.view.last_message = "task cancelled by user".to_string();
                }
                Ok(message) => {
                    entry.view.state = "Completed".to_string();
                    entry.view.last_event = completed_event.to_string();
                    entry.view.last_message = message.clone();
                }
                Err(message) => {
                    if is_recoverable_transfer_interruption(message) {
                        entry.view.state = "Paused".to_string();
                        entry.view.last_event = "paused".to_string();
                    } else {
                        entry.view.state = "Failed".to_string();
                        entry.view.last_event = "failed".to_string();
                    }
                    entry.view.last_message = message.clone();
                }
            }
        }
    }

    if cancelled {
        return;
    }

    if let Err(message) = result {
        if let Ok(mut engine) = engine.lock() {
            if is_recoverable_transfer_interruption(message) {
                let _ = engine.pause_task(task_id);
            } else {
                let _ = engine.record_task_failure(task_id, message.clone());
            }
        }
    }
}

fn is_recoverable_transfer_interruption(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    message.contains("暂停")
        || message.contains("管道已结束")
        || message.contains("由于连接方在一段时间后没有正确答复")
        || message.contains("连接尝试失败")
        || lower.contains("paused")
        || lower.contains("os error 109")
        || lower.contains("os error 10060")
        || lower.contains("os error 10061")
        || lower.contains("connection refused")
        || lower.contains("device offline")
        || lower.contains("device not found")
        || lower.contains("no devices/emulators found")
        || lower.contains("closed")
        || lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("timed out")
}

fn lane_interrupted_message(lane: &str, error: impl std::fmt::Display) -> String {
    format!(
        "transfer paused: {lane} lane interrupted; resume will verify committed chunks before skipping them. {error}"
    )
}

fn task_is_cancelled(engine: &Arc<Mutex<TransferEngine>>, task_id: &str) -> bool {
    engine
        .lock()
        .ok()
        .and_then(|engine| {
            engine
                .snapshots()
                .into_iter()
                .find(|snapshot| snapshot.task_id == task_id)
        })
        .map(|snapshot| snapshot.state == TaskState::Cancelled)
        .unwrap_or(false)
}

fn start_adb_transfer_from_record(
    record: TaskFileRecord,
    registry: State<'_, AdbTransferRegistry>,
    engine: State<'_, DesktopEngine>,
) -> Result<AdbTransferCard, String> {
    let serial = record
        .device_serial
        .clone()
        .ok_or_else(|| "device_serial is required for ADB transfer".to_string())?;
    let local_path = PathBuf::from(&record.source_path);
    let remote_path = record.target_root.clone();
    let task_id = record.task_id.clone();
    let registry_handle = registry.0.clone();
    let engine_handle = engine.0.clone();
    let pause_requested = Arc::new(AtomicBool::new(false));
    let view = AdbTransferCard::new(&task_id, &serial, &remote_path);

    {
        let mut transfers = registry_handle.lock().map_err(|err| err.to_string())?;
        if let Some(existing) = transfers.get(&task_id) {
            if existing.view.state == "Running" || existing.view.state == "Pausing" {
                return Ok(existing.view.clone());
            }
        }

        transfers.insert(
            task_id.clone(),
            AdbTransferEntry {
                view: view.clone(),
                pause_requested: pause_requested.clone(),
            },
        );
    }

    let worker_task_id = task_id.clone();
    let worker_serial = serial.clone();
    let worker_registry = registry_handle.clone();
    let worker_engine = engine_handle.clone();
    let worker_remote_path = remote_path.clone();
    let worker_chunk_size = record.chunk_size_bytes.unwrap_or(8 * 1024 * 1024);
    let worker_verify_enabled = record.verify_enabled;
    thread::spawn(move || {
        let control = adb::AdbTransferControl::new(pause_requested);
        let result = adb::push_path_to_device_with_control(
            &worker_serial,
            &local_path,
            &worker_remote_path,
            worker_chunk_size,
            Some(control),
            |progress| {
                if let Ok(mut transfers) = worker_registry.lock() {
                    if let Some(entry) = transfers.get_mut(&worker_task_id) {
                        entry.view.state = "Running".to_string();
                        entry.view.current_file = progress.current_file;
                        entry.view.total_files = progress.total_files;
                        entry.view.pushed_files = progress.pushed_files;
                        entry.view.skipped_files = progress.skipped_files;
                        entry.view.pushed_chunks = progress.pushed_chunks;
                        entry.view.skipped_chunks = progress.skipped_chunks;
                        entry.view.bytes_scanned = progress.bytes_scanned;
                        entry.view.bytes_pushed = progress.bytes_pushed;
                        entry.view.last_event = progress.event.clone();
                        entry.view.last_message = progress.message.clone();
                        entry.view.remote_path = progress.remote_path.clone();
                        entry.view.relative_path = progress.relative_path.clone();
                    }
                }

                if let Ok(mut engine) = worker_engine.lock() {
                    match progress.event.as_str() {
                        "chunk-pushed" | "chunk-skipped" => {
                            if let Some(chunk_index) = progress.chunk_index {
                                let _ = engine.record_real_chunk_commit(
                                    &worker_task_id,
                                    ChunkDescriptor {
                                        file_index: progress.file_index,
                                        chunk_index,
                                        offset: chunk_index as u64 * worker_chunk_size,
                                        length: progress.chunk_length,
                                    },
                                    LaneAssignment::Adb,
                                    progress.event == "chunk-skipped",
                                );
                            }
                        }
                        "file-skipped" => {
                            let _ = engine.record_real_file_complete(
                                &worker_task_id,
                                progress.file_index,
                                LaneAssignment::Adb,
                                true,
                            );
                        }
                        "file-completed" => {
                            let _ = engine.record_real_file_complete(
                                &worker_task_id,
                                progress.file_index,
                                LaneAssignment::Adb,
                                false,
                            );
                        }
                        _ => {}
                    }
                }
            },
        )
        .map_err(|err| err.to_string())
        .and_then(|message| {
            if worker_verify_enabled {
                verify_adb_pc_to_android_transfer(
                    &worker_serial,
                    &local_path,
                    &worker_remote_path,
                )?;
            }
            Ok(message)
        });

        settle_worker_result(
            &worker_registry,
            &worker_engine,
            &worker_task_id,
            &result,
            "completed",
        );
    });

    Ok(view)
}

fn start_adb_pull_from_record(
    record: TaskFileRecord,
    registry: State<'_, AdbTransferRegistry>,
    engine: State<'_, DesktopEngine>,
) -> Result<AdbTransferCard, String> {
    let serial = record
        .device_serial
        .clone()
        .ok_or_else(|| "device_serial is required for ADB Android -> PC transfer".to_string())?;
    let remote_path = record.source_path.clone();
    let local_root = PathBuf::from(&record.target_root);
    let task_id = record.task_id.clone();
    let registry_handle = registry.0.clone();
    let engine_handle = engine.0.clone();
    let pause_requested = Arc::new(AtomicBool::new(false));
    let view = AdbTransferCard::new(&task_id, &serial, &record.target_root);

    {
        let mut transfers = registry_handle.lock().map_err(|err| err.to_string())?;
        if let Some(existing) = transfers.get(&task_id) {
            if existing.view.state == "Running" || existing.view.state == "Pausing" {
                return Ok(existing.view.clone());
            }
        }

        transfers.insert(
            task_id.clone(),
            AdbTransferEntry {
                view: view.clone(),
                pause_requested: pause_requested.clone(),
            },
        );
    }

    let worker_task_id = task_id.clone();
    let worker_serial = serial.clone();
    let worker_registry = registry_handle.clone();
    let worker_engine = engine_handle.clone();
    let worker_verify_enabled = record.verify_enabled;
    thread::spawn(move || {
        let result = if pause_requested.load(Ordering::Relaxed) {
            Err("transfer paused before adb pull".to_string())
        } else {
            run_adb_android_to_pc(
                &worker_serial,
                &worker_task_id,
                &remote_path,
                &local_root,
                worker_verify_enabled,
                worker_registry.clone(),
                worker_engine.clone(),
            )
        };

        settle_worker_result(
            &worker_registry,
            &worker_engine,
            &worker_task_id,
            &result,
            "completed",
        );
    });

    Ok(view)
}

fn start_wifi_transfer_from_record(
    record: TaskFileRecord,
    registry: State<'_, AdbTransferRegistry>,
    engine: State<'_, DesktopEngine>,
) -> Result<AdbTransferCard, String> {
    if parse_direction_label(&record.direction)? != Direction::PcToAndroid {
        return Err("Wi-Fi worker currently supports PC -> Android tasks".to_string());
    }

    let host = record
        .agent_host
        .clone()
        .ok_or_else(|| "agent_host is required for Wi-Fi transfer".to_string())?;
    let local_path = PathBuf::from(&record.source_path);
    let task_id = record.task_id.clone();
    let registry_handle = registry.0.clone();
    let engine_handle = engine.0.clone();
    let pause_requested = Arc::new(AtomicBool::new(false));
    let view = AdbTransferCard::new(&task_id, &format!("wifi:{host}"), &record.target_root);

    {
        let mut transfers = registry_handle.lock().map_err(|err| err.to_string())?;
        if let Some(existing) = transfers.get(&task_id) {
            if existing.view.state == "Running" || existing.view.state == "Pausing" {
                return Ok(existing.view.clone());
            }
        }

        transfers.insert(
            task_id.clone(),
            AdbTransferEntry {
                view: view.clone(),
                pause_requested: pause_requested.clone(),
            },
        );
    }

    let manifest = collect_local_transfer_manifest(&local_path)?;
    let directories = manifest.directories;
    let files = manifest.files;
    let requested_chunk_size = record.chunk_size_bytes.unwrap_or(8 * 1024 * 1024);
    let worker_chunk_size = wifi_task_chunk_size_for_files(&files, requested_chunk_size);
    if record.chunk_size_bytes != Some(worker_chunk_size) {
        if let Ok(mut engine) = engine.0.lock() {
            let _ = engine.reconfigure_task_chunk_size(&record.task_id, worker_chunk_size);
        }
        let mut updated = record.clone();
        updated.chunk_size_bytes = Some(worker_chunk_size);
        let _ = updated.persist();
    }
    let worker_task_id = task_id.clone();
    let worker_host = host.clone();
    let worker_registry = registry_handle.clone();
    let worker_engine = engine_handle.clone();
    thread::spawn(move || {
        let result = run_wifi_pc_to_android(
            &worker_host,
            &worker_task_id,
            &record.target_root,
            directories,
            files,
            worker_chunk_size,
            record.verify_enabled,
            pause_requested,
            worker_registry.clone(),
            worker_engine.clone(),
        );

        settle_worker_result(
            &worker_registry,
            &worker_engine,
            &worker_task_id,
            &result,
            "completed",
        );
    });

    Ok(view)
}

fn start_wifi_pull_from_record(
    record: TaskFileRecord,
    registry: State<'_, AdbTransferRegistry>,
    engine: State<'_, DesktopEngine>,
) -> Result<AdbTransferCard, String> {
    let host = record
        .agent_host
        .clone()
        .ok_or_else(|| "agent_host is required for Android -> PC transfer".to_string())?;
    let task_id = record.task_id.clone();
    let source_relative = record.source_path.clone();
    let target_root = PathBuf::from(&record.target_root);
    fs::create_dir_all(&target_root).map_err(|err| err.to_string())?;
    let registry_handle = registry.0.clone();
    let engine_handle = engine.0.clone();
    let pause_requested = Arc::new(AtomicBool::new(false));
    let view = AdbTransferCard::new(&task_id, &format!("wifi:{host}"), &record.target_root);

    {
        let mut transfers = registry_handle.lock().map_err(|err| err.to_string())?;
        if let Some(existing) = transfers.get(&task_id) {
            if existing.view.state == "Running" || existing.view.state == "Pausing" {
                return Ok(existing.view.clone());
            }
        }

        transfers.insert(
            task_id.clone(),
            AdbTransferEntry {
                view: view.clone(),
                pause_requested: pause_requested.clone(),
            },
        );
    }

    let worker_task_id = task_id.clone();
    let worker_host = host.clone();
    let worker_registry = registry_handle.clone();
    let worker_engine = engine_handle.clone();
    let requested_chunk_size = record.chunk_size_bytes.unwrap_or(8 * 1024 * 1024);
    let worker_chunk_size =
        wifi_transfer_chunk_size(record.source_size_bytes.unwrap_or(0), requested_chunk_size);
    if record.chunk_size_bytes != Some(worker_chunk_size) {
        if let Ok(mut engine) = engine.0.lock() {
            let _ = engine.reconfigure_task_chunk_size(&record.task_id, worker_chunk_size);
        }
        let mut updated = record.clone();
        updated.chunk_size_bytes = Some(worker_chunk_size);
        let _ = updated.persist();
    }
    thread::spawn(move || {
        let result = run_wifi_android_to_pc(
            &worker_host,
            &worker_task_id,
            &source_relative,
            &target_root,
            worker_chunk_size,
            record.verify_enabled,
            pause_requested,
            worker_registry.clone(),
            worker_engine.clone(),
        );

        settle_worker_result(
            &worker_registry,
            &worker_engine,
            &worker_task_id,
            &result,
            "completed",
        );
    });

    Ok(view)
}

fn run_wifi_pc_to_android(
    host: &str,
    task_id: &str,
    target_root: &str,
    directories: Vec<LocalTransferDirectory>,
    files: Vec<LocalTransferFile>,
    chunk_size: u64,
    verify_enabled: bool,
    pause_requested: Arc<AtomicBool>,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
) -> Result<String, String> {
    agent::start_task(host, task_id).map_err(|err| err.to_string())?;
    agent::set_target_root(host, target_root).map_err(|err| err.to_string())?;
    let created_directories = push_directory_bundle(host, task_id, directories)?;
    let checkpoint = load_task_checkpoint(&engine, task_id);
    let total_files = files.len();
    let mut pushed_files = 0usize;
    let mut skipped_files = 0usize;
    let mut pushed_chunks = 0u64;
    let mut skipped_chunks = 0u64;
    let mut bytes_scanned = 0u64;
    let mut bytes_pushed = 0u64;

    for (file_index, file) in files.into_iter().enumerate() {
        if pause_requested.load(Ordering::Relaxed) {
            return Err("transfer paused at chunk boundary".to_string());
        }
        bytes_scanned += file.size_bytes;
        let relative_path = file.relative_path.to_string_lossy().replace('\\', "/");
        let completed_chunks = completed_chunks_for_file(checkpoint.as_ref(), file_index);
        let all_chunks_completed =
            is_file_fully_checkpointed(file.size_bytes, chunk_size, &completed_chunks);
        if all_chunks_completed {
            skipped_files += 1;
            skipped_chunks += completed_chunks.len() as u64;
            agent::complete_file(host, &relative_path).map_err(|err| err.to_string())?;
            if let Ok(mut engine) = engine.lock() {
                let _ = engine.record_real_file_complete(
                    task_id,
                    file_index,
                    LaneAssignment::Wifi,
                    true,
                );
            }
            if let Ok(mut transfers) = registry.lock() {
                if let Some(entry) = transfers.get_mut(task_id) {
                    entry.view.state = "Running".to_string();
                    entry.view.current_file = file_index + 1;
                    entry.view.total_files = total_files;
                    entry.view.pushed_files = pushed_files;
                    entry.view.skipped_files = skipped_files;
                    entry.view.pushed_chunks = pushed_chunks;
                    entry.view.skipped_chunks = skipped_chunks;
                    entry.view.bytes_scanned = bytes_scanned;
                    entry.view.bytes_pushed = bytes_pushed;
                    entry.view.relative_path = relative_path.clone();
                    entry.view.remote_path = host.to_string();
                    entry.view.last_event = "wifi-file-skipped".to_string();
                    entry.view.last_message =
                        format!("Wi-Fi file {} resumed from checkpoint", relative_path);
                }
            }
            continue;
        }
        agent::start_file(host, &relative_path, file.size_bytes).map_err(|err| err.to_string())?;
        let mut input = fs::File::open(&file.local_path).map_err(|err| err.to_string())?;
        let mut buffer = vec![0u8; chunk_size as usize];
        let total_chunks = chunk_count_for_size(file.size_bytes, chunk_size);

        for chunk_index in 0..total_chunks {
            if pause_requested.load(Ordering::Relaxed) {
                return Err("transfer paused at chunk boundary".to_string());
            }
            let offset = chunk_index as u64 * chunk_size;
            let chunk_length = chunk_length_for(file.size_bytes, chunk_size, chunk_index);
            if completed_chunks.contains(&chunk_index) {
                skipped_chunks += 1;
                if let Ok(mut transfers) = registry.lock() {
                    if let Some(entry) = transfers.get_mut(task_id) {
                        entry.view.state = "Running".to_string();
                        entry.view.current_file = file_index + 1;
                        entry.view.total_files = total_files;
                        entry.view.pushed_files = pushed_files;
                        entry.view.skipped_files = skipped_files;
                        entry.view.pushed_chunks = pushed_chunks;
                        entry.view.skipped_chunks = skipped_chunks;
                        entry.view.bytes_scanned = bytes_scanned;
                        entry.view.bytes_pushed = bytes_pushed;
                        entry.view.relative_path = relative_path.clone();
                        entry.view.remote_path = host.to_string();
                        entry.view.last_event = "wifi-chunk-skipped".to_string();
                        entry.view.last_message =
                            format!("Wi-Fi chunk {chunk_index} resumed from checkpoint");
                    }
                }
                continue;
            }

            input
                .seek(SeekFrom::Start(offset))
                .map_err(|err| err.to_string())?;
            let read = input
                .read(&mut buffer[..chunk_length as usize])
                .map_err(|err| err.to_string())?;
            if read == 0 && !(file.size_bytes == 0 && chunk_index == 0) {
                break;
            }
            let payload = &buffer[..read];
            let recovered_status =
                push_wifi_chunk_with_recovery(host, &relative_path, chunk_index, offset, payload)?;
            let chunk_length = read as u64;
            pushed_chunks += 1;
            bytes_pushed += chunk_length;

            if let Ok(mut engine) = engine.lock() {
                let _ = engine.record_real_chunk_commit(
                    task_id,
                    ChunkDescriptor {
                        file_index,
                        chunk_index,
                        offset,
                        length: chunk_length,
                    },
                    LaneAssignment::Wifi,
                    false,
                );
            }

            if let Ok(mut transfers) = registry.lock() {
                if let Some(entry) = transfers.get_mut(task_id) {
                    entry.view.state = "Running".to_string();
                    entry.view.current_file = file_index + 1;
                    entry.view.total_files = total_files;
                    entry.view.pushed_files = pushed_files;
                    entry.view.skipped_files = skipped_files;
                    entry.view.pushed_chunks = pushed_chunks;
                    entry.view.skipped_chunks = skipped_chunks;
                    entry.view.bytes_scanned = bytes_scanned;
                    entry.view.bytes_pushed = bytes_pushed;
                    entry.view.relative_path = relative_path.clone();
                    entry.view.remote_path = host.to_string();
                    entry.view.last_event = recovered_status.event_name().to_string();
                    entry.view.last_message = recovered_status.message(chunk_index);
                }
            }
        }

        pushed_files += 1;
        agent::complete_file(host, &relative_path).map_err(|err| err.to_string())?;
        if let Ok(mut engine) = engine.lock() {
            let _ =
                engine.record_real_file_complete(task_id, file_index, LaneAssignment::Wifi, false);
        }

        if verify_enabled {
            verify_android_remote_file(host, &relative_path, &file.local_path, file.size_bytes)
                .map_err(|message| format!("{message}"))?;
        }
    }

    Ok(format!(
        "Wi-Fi transfer completed: directories={created_directories} pushed_files={pushed_files} skipped_files={skipped_files} pushed_chunks={pushed_chunks} skipped_chunks={skipped_chunks} bytes_pushed={bytes_pushed}"
    ))
}

fn run_dual_adb_pc_to_android_files(
    serial: &str,
    task_id: &str,
    target_root: &str,
    files: Vec<LocalTransferFile>,
    chunk_size: u64,
    verify_enabled: bool,
    control: adb::AdbTransferControl,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
    aggregate: Arc<Mutex<DualTransferAggregate>>,
) -> Result<String, String> {
    let mut outputs = Vec::new();
    for file in files {
        let remote_file = adb_like_remote_join(target_root, &file.relative_path);
        note_dual_file_started(
            &registry,
            task_id,
            &aggregate,
            &file,
            &remote_file,
            "adb-file-started",
        );
        let file_index = file.file_index;
        let file_relative = file.relative_path.to_string_lossy().to_string();
        let result = adb::push_path_to_device_with_control(
            serial,
            &file.local_path,
            &remote_file,
            chunk_size,
            Some(control.clone()),
            |progress| {
                note_dual_adb_progress(
                    &registry,
                    task_id,
                    &aggregate,
                    &file_relative,
                    &progress.remote_path,
                    &progress.event,
                    &progress.message,
                    progress.chunk_length,
                    progress.event == "chunk-skipped",
                    progress.event == "chunk-pushed",
                );

                if let Ok(mut engine) = engine.lock() {
                    match progress.event.as_str() {
                        "chunk-pushed" | "chunk-skipped" => {
                            if let Some(chunk_index) = progress.chunk_index {
                                let _ = engine.record_real_chunk_commit(
                                    task_id,
                                    ChunkDescriptor {
                                        file_index,
                                        chunk_index,
                                        offset: chunk_index as u64 * chunk_size,
                                        length: progress.chunk_length,
                                    },
                                    LaneAssignment::Adb,
                                    progress.event == "chunk-skipped",
                                );
                            }
                        }
                        "file-skipped" => {
                            let _ = engine.record_real_file_complete(
                                task_id,
                                file_index,
                                LaneAssignment::Adb,
                                true,
                            );
                            note_dual_file_finished(
                                &registry,
                                task_id,
                                &aggregate,
                                &file_relative,
                                &remote_file,
                                true,
                            );
                        }
                        "file-completed" => {
                            let _ = engine.record_real_file_complete(
                                task_id,
                                file_index,
                                LaneAssignment::Adb,
                                false,
                            );
                            note_dual_file_finished(
                                &registry,
                                task_id,
                                &aggregate,
                                &file_relative,
                                &remote_file,
                                false,
                            );
                        }
                        _ => {}
                    }
                }
            },
        )
        .map_err(|err| err.to_string())?;

        if verify_enabled {
            let local_digest = blake3_digest_file(&file.local_path)?;
            let remote_digest = adb::blake3_digest_remote_file(serial, &remote_file)
                .map_err(|err| err.to_string())?
                .ok_or_else(|| format!("remote file not found for verify: {remote_file}"))?;
            if local_digest != remote_digest {
                return Err(format!(
                    "ADB verify failed for {}: local={} remote={}",
                    file.relative_path.to_string_lossy(),
                    local_digest,
                    remote_digest
                ));
            }
        }

        outputs.push(result);
    }

    Ok(format!("ADB lane completed {} file(s)", outputs.len()))
}

fn run_dual_wifi_pc_to_android_files(
    host: &str,
    task_id: &str,
    target_root: &str,
    directories: Vec<LocalTransferDirectory>,
    files: Vec<LocalTransferFile>,
    chunk_size: u64,
    verify_enabled: bool,
    pause_requested: Arc<AtomicBool>,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
    aggregate: Arc<Mutex<DualTransferAggregate>>,
) -> Result<String, String> {
    agent::start_task(host, task_id).map_err(|err| err.to_string())?;
    agent::set_target_root(host, target_root).map_err(|err| err.to_string())?;
    let created_directories = push_directory_bundle(host, task_id, directories)?;

    let mut bundle_index = 0usize;
    let mut bundle = new_small_file_bundle(task_id, bundle_index);
    let mut bundled_files = 0usize;
    let mut direct_files = 0usize;

    for file in files {
        if pause_requested.load(Ordering::Relaxed) {
            flush_small_file_bundle(
                host,
                task_id,
                target_root,
                &mut bundle,
                verify_enabled,
                &registry,
                &engine,
                &aggregate,
            )?;
            return Err("transfer paused at chunk boundary".to_string());
        }
        if is_small_file_bundle_candidate(&file) && !small_file_bundle_can_accept(&bundle, &file) {
            bundled_files += flush_small_file_bundle(
                host,
                task_id,
                target_root,
                &mut bundle,
                verify_enabled,
                &registry,
                &engine,
                &aggregate,
            )?;
            bundle_index += 1;
            bundle = new_small_file_bundle(task_id, bundle_index);
        }
        if is_small_file_bundle_candidate(&file) {
            add_file_to_small_file_bundle(&mut bundle, file)?;
            continue;
        }

        bundled_files += flush_small_file_bundle(
            host,
            task_id,
            target_root,
            &mut bundle,
            verify_enabled,
            &registry,
            &engine,
            &aggregate,
        )?;
        bundle_index += 1;
        bundle = new_small_file_bundle(task_id, bundle_index);
        run_dual_wifi_single_file(
            host,
            task_id,
            target_root,
            file,
            chunk_size,
            verify_enabled,
            pause_requested.clone(),
            registry.clone(),
            engine.clone(),
            aggregate.clone(),
        )?;
        direct_files += 1;
    }

    bundled_files += flush_small_file_bundle(
        host,
        task_id,
        target_root,
        &mut bundle,
        verify_enabled,
        &registry,
        &engine,
        &aggregate,
    )?;

    Ok(format!(
        "Wi-Fi lane completed directory_entries={created_directories}, bundled_files={bundled_files}, direct_files={direct_files}"
    ))
}

fn run_dual_wifi_single_file(
    host: &str,
    task_id: &str,
    target_root: &str,
    file: LocalTransferFile,
    chunk_size: u64,
    verify_enabled: bool,
    pause_requested: Arc<AtomicBool>,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
    aggregate: Arc<Mutex<DualTransferAggregate>>,
) -> Result<(), String> {
    if pause_requested.load(Ordering::Relaxed) {
        return Err("transfer paused at chunk boundary".to_string());
    }
    let relative_path = file.relative_path.to_string_lossy().replace('\\', "/");
    note_dual_file_started(
        &registry,
        task_id,
        &aggregate,
        &file,
        target_root,
        "wifi-file-started",
    );
    agent::start_file(host, &relative_path, file.size_bytes).map_err(|err| err.to_string())?;
    let mut input = fs::File::open(&file.local_path).map_err(|err| err.to_string())?;
    let mut offset = 0u64;
    let mut chunk_index = 0u32;
    let mut buffer = vec![0u8; chunk_size as usize];

    loop {
        if pause_requested.load(Ordering::Relaxed) {
            return Err("transfer paused at chunk boundary".to_string());
        }
        let read = input.read(&mut buffer).map_err(|err| err.to_string())?;
        if read == 0 && !(file.size_bytes == 0 && chunk_index == 0) {
            break;
        }
        let payload = &buffer[..read];
        let recovered_status =
            push_wifi_chunk_with_recovery(host, &relative_path, chunk_index, offset, payload)?;
        let chunk_length = read as u64;

        note_dual_adb_progress(
            &registry,
            task_id,
            &aggregate,
            &relative_path,
            target_root,
            recovered_status.event_name(),
            &recovered_status.message(chunk_index),
            chunk_length,
            false,
            true,
        );

        if let Ok(mut engine) = engine.lock() {
            let _ = engine.record_real_chunk_commit(
                task_id,
                ChunkDescriptor {
                    file_index: file.file_index,
                    chunk_index,
                    offset,
                    length: chunk_length,
                },
                LaneAssignment::Wifi,
                false,
            );
        }

        offset += chunk_length;
        chunk_index += 1;
        if read == 0 {
            break;
        }
    }

    agent::complete_file(host, &relative_path).map_err(|err| err.to_string())?;
    if let Ok(mut engine) = engine.lock() {
        let _ =
            engine.record_real_file_complete(task_id, file.file_index, LaneAssignment::Wifi, false);
    }
    note_dual_file_finished(
        &registry,
        task_id,
        &aggregate,
        &relative_path,
        target_root,
        false,
    );

    if verify_enabled {
        verify_android_remote_file(host, &relative_path, &file.local_path, file.size_bytes)?;
    }

    Ok(())
}

fn small_file_bundle_max_bytes() -> u64 {
    64 * 1024 * 1024
}

fn small_file_bundle_max_files() -> usize {
    4096
}

fn is_small_file_bundle_candidate(file: &LocalTransferFile) -> bool {
    file.size_bytes <= small_file_bundle_max_bytes()
}

fn new_small_file_bundle(task_id: &str, index: usize) -> SmallFileBundle {
    SmallFileBundle {
        bundle_id: format!("{task_id}-bundle-{index:06}"),
        entries: Vec::new(),
        manifest: String::new(),
        payload: Vec::new(),
    }
}

fn push_directory_bundle(
    host: &str,
    task_id: &str,
    directories: Vec<LocalTransferDirectory>,
) -> Result<usize, String> {
    if directories.is_empty() {
        return Ok(0);
    }

    let mut manifest = String::new();
    let mut count = 0usize;
    for directory in directories {
        let relative_path = sanitize_agent_relative_path(&directory.relative_path);
        if relative_path.is_empty() {
            continue;
        }
        manifest.push_str(&format!("D\t{}\n", agent::encode_path_arg(&relative_path)));
        count += 1;
    }
    if count == 0 {
        return Ok(0);
    }

    let reply =
        agent::push_file_bundle_binary(host, &format!("{task_id}-dirs"), manifest.as_bytes(), &[])
            .map_err(|err| err.to_string())?;
    ensure_chunk_response_status(&reply.payload, &["bundle_written"])?;
    Ok(count)
}

fn small_file_bundle_can_accept(bundle: &SmallFileBundle, file: &LocalTransferFile) -> bool {
    if bundle.entries.is_empty() {
        return true;
    }
    bundle.entries.len() < small_file_bundle_max_files()
        && (bundle.payload.len() as u64 + file.size_bytes) <= small_file_bundle_max_bytes()
}

fn add_file_to_small_file_bundle(
    bundle: &mut SmallFileBundle,
    file: LocalTransferFile,
) -> Result<(), String> {
    let relative_path = sanitize_agent_relative_path(&file.relative_path);
    if relative_path.is_empty() {
        return Err(format!(
            "bundle relative path is not safe: {}",
            file.relative_path.display()
        ));
    }
    let metadata = fs::metadata(&file.local_path).map_err(|err| err.to_string())?;
    let modified_at_epoch_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_millis() as u64)
        .unwrap_or_else(|| epoch_ms() as u64);
    let size_bytes = metadata.len();
    if size_bytes != file.size_bytes {
        return Err(format!(
            "file size changed while bundling {}: expected {} actual {}",
            file.local_path.display(),
            file.size_bytes,
            size_bytes
        ));
    }

    bundle.manifest.push_str(&format!(
        "F\t{}\t{}\t{}\n",
        agent::encode_path_arg(&relative_path),
        size_bytes,
        modified_at_epoch_ms
    ));
    if size_bytes > 0 {
        let mut input = fs::File::open(&file.local_path).map_err(|err| err.to_string())?;
        input
            .read_to_end(&mut bundle.payload)
            .map_err(|err| err.to_string())?;
    }
    bundle.entries.push(SmallFileBundleEntry {
        file,
        relative_path,
        size_bytes,
    });
    Ok(())
}

fn flush_small_file_bundle(
    host: &str,
    task_id: &str,
    target_root: &str,
    bundle: &mut SmallFileBundle,
    verify_enabled: bool,
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: &Arc<Mutex<TransferEngine>>,
    aggregate: &Arc<Mutex<DualTransferAggregate>>,
) -> Result<usize, String> {
    if bundle.entries.is_empty() {
        return Ok(0);
    }
    let reply = agent::push_file_bundle_binary(
        host,
        &bundle.bundle_id,
        bundle.manifest.as_bytes(),
        &bundle.payload,
    )
    .map_err(|err| err.to_string())?;
    ensure_chunk_response_status(&reply.payload, &["bundle_written"])?;

    for entry in &bundle.entries {
        note_dual_file_started(
            registry,
            task_id,
            aggregate,
            &entry.file,
            target_root,
            "wifi-bundle-file-started",
        );
        note_dual_adb_progress(
            registry,
            task_id,
            aggregate,
            &entry.relative_path,
            target_root,
            "wifi-bundle-file-pushed",
            &format!("bundled file committed via {}", bundle.bundle_id),
            entry.size_bytes,
            false,
            true,
        );
        if let Ok(mut engine) = engine.lock() {
            let _ = engine.record_real_chunk_commit(
                task_id,
                ChunkDescriptor {
                    file_index: entry.file.file_index,
                    chunk_index: 0,
                    offset: 0,
                    length: entry.size_bytes,
                },
                LaneAssignment::Wifi,
                false,
            );
            let _ = engine.record_real_file_complete(
                task_id,
                entry.file.file_index,
                LaneAssignment::Wifi,
                false,
            );
        }
        note_dual_file_finished(
            registry,
            task_id,
            aggregate,
            &entry.relative_path,
            target_root,
            false,
        );
        if verify_enabled {
            verify_android_remote_file(
                host,
                &entry.relative_path,
                &entry.file.local_path,
                entry.size_bytes,
            )?;
        }
    }

    let completed = bundle.entries.len();
    bundle.entries.clear();
    bundle.manifest.clear();
    bundle.payload.clear();
    Ok(completed)
}

fn run_dual_same_file_pc_to_android_files(
    serial: &str,
    host: &str,
    task_id: &str,
    target_root: &str,
    files: Vec<LocalTransferFile>,
    chunk_size: u64,
    verify_enabled: bool,
    pause_requested: Arc<AtomicBool>,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
    aggregate: Arc<Mutex<DualTransferAggregate>>,
) -> Result<String, String> {
    agent::start_task(host, task_id).map_err(|err| err.to_string())?;
    let target_reply = agent::set_target_root(host, target_root).map_err(|err| err.to_string())?;
    let resolved_target_root = json_string_field(&target_reply.payload, "target_root")
        .unwrap_or_else(|| {
            // Older agents may not echo target_root; absolute target roots still converge safely.
            target_root.to_string()
        });
    let checkpoint = load_task_checkpoint(&engine, task_id);
    let mut completed_files = 0usize;
    let lane_plan = match files.first() {
        Some(file) => plan_same_file_lanes(
            serial,
            host,
            task_id,
            &resolved_target_root,
            file,
            chunk_size,
        ),
        None => SameFileLanePlan::fallback("no same-file payload"),
    };
    // Calibration uses the agent file path intentionally; reset the task so Android snapshots
    // count only the real user payload.
    agent::start_task(host, task_id).map_err(|err| err.to_string())?;
    agent::set_target_root(host, &resolved_target_root).map_err(|err| err.to_string())?;

    for file in files {
        if pause_requested.load(Ordering::Relaxed) {
            return Err("transfer paused at chunk boundary".to_string());
        }
        let relative_path = sanitize_agent_relative_path(&file.relative_path);
        if relative_path.is_empty() {
            return Err(format!(
                "same-file Dual relative path is not safe: {}",
                file.relative_path.display()
            ));
        }
        let remote_temp_file = adb::remote_path_join(
            &resolved_target_root,
            &PathBuf::from(format!("{relative_path}.nekotrans-tmp")),
        );
        let remote_final_file =
            adb::remote_path_join(&resolved_target_root, &PathBuf::from(&relative_path));
        note_dual_file_started(
            &registry,
            task_id,
            &aggregate,
            &file,
            &remote_temp_file,
            "dual-same-file-started",
        );
        agent::start_file(host, &relative_path, file.size_bytes).map_err(|err| err.to_string())?;

        let file_chunk_size = dual_same_file_chunk_size(file.size_bytes, chunk_size);
        let wifi_stride = lane_plan.wifi_stride;
        let total_chunks = chunk_count_for_size(file.size_bytes, file_chunk_size);
        let completed_chunks = completed_chunks_for_file(checkpoint.as_ref(), file.file_index);
        if completed_chunks.is_empty() {
            let _ = adb::remove_remote_path(serial, &remote_temp_file);
            let _ = adb::remove_remote_path(serial, &remote_final_file);
        }
        let completed_chunks = prepare_same_file_resume_chunks(
            host,
            task_id,
            &file.local_path,
            &relative_path,
            file_chunk_size,
            total_chunks,
            wifi_stride,
            completed_chunks,
            &registry,
            &aggregate,
        )?;
        let adb_result = run_dual_same_file_lane(
            DualSameFileLane::Adb,
            serial,
            host,
            task_id,
            &relative_path,
            &remote_temp_file,
            &file,
            file_chunk_size,
            total_chunks,
            wifi_stride,
            lane_plan.wifi_batch_chunks,
            completed_chunks.as_slice(),
            pause_requested.clone(),
            registry.clone(),
            engine.clone(),
            aggregate.clone(),
        );
        let wifi_result = run_dual_same_file_lane(
            DualSameFileLane::Wifi,
            serial,
            host,
            task_id,
            &relative_path,
            &remote_temp_file,
            &file,
            file_chunk_size,
            total_chunks,
            wifi_stride,
            lane_plan.wifi_batch_chunks,
            completed_chunks.as_slice(),
            pause_requested.clone(),
            registry.clone(),
            engine.clone(),
            aggregate.clone(),
        );

        let (adb_result, wifi_result) = thread::scope(|scope| {
            let adb = scope.spawn(|| adb_result());
            let wifi = scope.spawn(|| wifi_result());
            (
                adb.join()
                    .unwrap_or_else(|_| Err("same-file ADB lane panicked".to_string())),
                wifi.join()
                    .unwrap_or_else(|_| Err("same-file Wi-Fi lane panicked".to_string())),
            )
        });
        adb_result?;
        wifi_result?;

        note_dual_stage(
            &registry,
            task_id,
            &aggregate,
            "dual-finalizing",
            "正在让手机端合并临时文件",
        );
        agent::complete_file(host, &relative_path).map_err(|err| err.to_string())?;
        note_dual_stage(
            &registry,
            task_id,
            &aggregate,
            "dual-remote-stat",
            "正在读取手机端文件大小",
        );
        let remote_size = wait_for_remote_file_size(host, &relative_path, file.size_bytes)?;
        if remote_size != file.size_bytes {
            return Err(format!(
                "same-file Dual size mismatch for {relative_path}: local={} remote={remote_size}",
                file.size_bytes
            ));
        }
        if verify_enabled {
            note_dual_stage(
                &registry,
                task_id,
                &aggregate,
                "dual-remote-verify",
                "正在快速校验手机端目标文件",
            );
            verify_android_remote_file(host, &relative_path, &file.local_path, file.size_bytes)
                .map_err(|message| {
                    format!("same-file Dual verify failed for {relative_path}: {message}")
                })?;
        }
        if let Ok(mut engine) = engine.lock() {
            let _ = engine.record_real_file_complete(
                task_id,
                file.file_index,
                LaneAssignment::Wifi,
                false,
            );
        }
        note_dual_file_finished(
            &registry,
            task_id,
            &aggregate,
            &relative_path,
            &remote_temp_file,
            false,
        );
        completed_files += 1;
    }

    Ok(format!(
        "same-file Dual completed {completed_files} large file(s); {}",
        lane_plan.summary()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DualSameFileLane {
    Adb,
    Wifi,
}

struct PendingWifiChunk {
    chunk: ChunkDescriptor,
    offset: u64,
    length: u64,
    payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct SameFileLanePlan {
    wifi_stride: u32,
    wifi_batch_chunks: usize,
    source: String,
    calibration: Option<SameFileLaneCalibration>,
}

#[derive(Debug, Clone)]
struct SameFileLaneCalibration {
    sample_bytes: u64,
    adb_bytes_per_second: f64,
    wifi_bytes_per_second: f64,
}

struct SmallFileBundleEntry {
    file: LocalTransferFile,
    relative_path: String,
    size_bytes: u64,
}

struct SmallFileBundle {
    bundle_id: String,
    entries: Vec<SmallFileBundleEntry>,
    manifest: String,
    payload: Vec<u8>,
}

impl DualSameFileLane {
    fn assignment(self) -> LaneAssignment {
        match self {
            Self::Adb => LaneAssignment::Adb,
            Self::Wifi => LaneAssignment::Wifi,
        }
    }

    fn owns_chunk(self, chunk_index: u32, wifi_stride: u32) -> bool {
        let wifi_owns = chunk_index % wifi_stride.max(2) == 1;
        match self {
            Self::Adb => !wifi_owns,
            Self::Wifi => wifi_owns,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Adb => "ADB",
            Self::Wifi => "Wi-Fi",
        }
    }
}

fn run_dual_same_file_lane<'a>(
    lane: DualSameFileLane,
    serial: &'a str,
    host: &'a str,
    task_id: &'a str,
    relative_path: &'a str,
    remote_temp_file: &'a str,
    file: &'a LocalTransferFile,
    chunk_size: u64,
    total_chunks: u32,
    wifi_stride: u32,
    wifi_batch_chunks: usize,
    completed_chunks: &'a [u32],
    pause_requested: Arc<AtomicBool>,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
    aggregate: Arc<Mutex<DualTransferAggregate>>,
) -> impl FnOnce() -> Result<(), String> + 'a {
    move || {
        let mut input = fs::File::open(&file.local_path).map_err(|err| err.to_string())?;
        let buffer_len = usize::try_from(chunk_size.max(1))
            .map_err(|_| "same-file Dual chunk size is too large for this platform".to_string())?;
        let mut buffer = vec![0u8; buffer_len];
        let mut pending_wifi_chunks: Vec<PendingWifiChunk> = Vec::new();

        for chunk_index in 0..total_chunks {
            if !lane.owns_chunk(chunk_index, wifi_stride) {
                continue;
            }
            if pause_requested.load(Ordering::Relaxed) {
                return Err("transfer paused at chunk boundary".to_string());
            }
            let offset = chunk_index as u64 * chunk_size;
            let chunk_length = chunk_length_for(file.size_bytes, chunk_size, chunk_index);
            let chunk = ChunkDescriptor {
                file_index: file.file_index,
                chunk_index,
                offset,
                length: chunk_length,
            };
            if completed_chunks.contains(&chunk_index) {
                continue;
            }

            match lane {
                DualSameFileLane::Adb => {
                    if let Err(error) = adb::write_file_chunk_at_offset(
                        serial,
                        &file.local_path,
                        remote_temp_file,
                        chunk_index,
                        offset,
                        chunk_length,
                    ) {
                        pause_requested.store(true, Ordering::Relaxed);
                        return Err(lane_interrupted_message(lane.label(), error));
                    }
                }
                DualSameFileLane::Wifi => {
                    input
                        .seek(SeekFrom::Start(offset))
                        .map_err(|err| err.to_string())?;
                    let read = input
                        .read(&mut buffer[..chunk_length as usize])
                        .map_err(|err| err.to_string())?;
                    pending_wifi_chunks.push(PendingWifiChunk {
                        chunk,
                        offset,
                        length: chunk_length,
                        payload: buffer[..read].to_vec(),
                    });
                    if pending_wifi_chunks.len() >= wifi_batch_chunks {
                        if let Err(error) = flush_same_file_wifi_batch(
                            host,
                            task_id,
                            relative_path,
                            remote_temp_file,
                            &mut pending_wifi_chunks,
                            &registry,
                            &engine,
                            &aggregate,
                        ) {
                            pause_requested.store(true, Ordering::Relaxed);
                            return Err(lane_interrupted_message(lane.label(), error));
                        }
                    }
                    continue;
                }
            }

            note_dual_adb_progress(
                &registry,
                task_id,
                &aggregate,
                relative_path,
                remote_temp_file,
                "dual-same-file-chunk-pushed",
                &format!("{} chunk {chunk_index} committed", lane.label()),
                chunk_length,
                false,
                true,
            );
            if let Ok(mut engine) = engine.lock() {
                let _ = engine.record_real_chunk_commit_pending(
                    task_id,
                    chunk,
                    lane.assignment(),
                    false,
                );
            }
        }

        if lane == DualSameFileLane::Wifi {
            if let Err(error) = flush_same_file_wifi_batch(
                host,
                task_id,
                relative_path,
                remote_temp_file,
                &mut pending_wifi_chunks,
                &registry,
                &engine,
                &aggregate,
            ) {
                pause_requested.store(true, Ordering::Relaxed);
                return Err(lane_interrupted_message(lane.label(), error));
            }
        }

        Ok(())
    }
}

fn same_file_wifi_batch_chunks() -> usize {
    std::env::var("NEKOTRANS_DUAL_WIFI_BATCH_CHUNKS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(1, 8))
        .unwrap_or(4)
}

fn same_file_wifi_stride(_file_size: u64) -> u32 {
    // Current hardware smoke shows ADB is about 2.5x faster than direct Wi-Fi on
    // the test phone, so fixed 50/50 chunk ownership leaves Wi-Fi as the tail.
    // This should become a per-device calibration result; 4 is the stable
    // ADB-heavy fallback validated by the 8.54GB fixture.
    4
}

impl SameFileLanePlan {
    fn fallback(source: impl Into<String>) -> Self {
        Self {
            wifi_stride: same_file_wifi_stride(0),
            wifi_batch_chunks: same_file_wifi_batch_chunks(),
            source: source.into(),
            calibration: None,
        }
    }

    fn summary(&self) -> String {
        match &self.calibration {
            Some(calibration) => format!(
                "lane_plan={} wifi_stride={} wifi_batch_chunks={} sample={} adb={} wifi={}",
                self.source,
                self.wifi_stride,
                self.wifi_batch_chunks,
                calibration.sample_bytes,
                format_bytes_per_second(calibration.adb_bytes_per_second),
                format_bytes_per_second(calibration.wifi_bytes_per_second)
            ),
            None => format!(
                "lane_plan={} wifi_stride={} wifi_batch_chunks={}",
                self.source, self.wifi_stride, self.wifi_batch_chunks
            ),
        }
    }
}

fn plan_same_file_lanes(
    serial: &str,
    host: &str,
    task_id: &str,
    resolved_target_root: &str,
    file: &LocalTransferFile,
    requested_chunk_size: u64,
) -> SameFileLanePlan {
    if let Some(wifi_stride) = configured_same_file_wifi_stride() {
        return SameFileLanePlan {
            wifi_stride,
            wifi_batch_chunks: same_file_wifi_batch_chunks(),
            source: "env".to_string(),
            calibration: None,
        };
    }

    match calibrate_same_file_lanes(
        serial,
        host,
        task_id,
        resolved_target_root,
        file,
        requested_chunk_size,
    ) {
        Ok(calibration) => SameFileLanePlan {
            wifi_stride: same_file_wifi_stride_from_rates(
                calibration.adb_bytes_per_second,
                calibration.wifi_bytes_per_second,
            ),
            wifi_batch_chunks: same_file_wifi_batch_chunks(),
            source: "startup-calibration".to_string(),
            calibration: Some(calibration),
        },
        Err(err) => SameFileLanePlan::fallback(format!("fallback({err})")),
    }
}

fn configured_same_file_wifi_stride() -> Option<u32> {
    std::env::var("NEKOTRANS_DUAL_WIFI_STRIDE")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .map(|value| value.clamp(2, 16))
}

fn calibrate_same_file_lanes(
    serial: &str,
    host: &str,
    task_id: &str,
    resolved_target_root: &str,
    file: &LocalTransferFile,
    requested_chunk_size: u64,
) -> Result<SameFileLaneCalibration, String> {
    let file_chunk_size = dual_same_file_chunk_size(file.size_bytes, requested_chunk_size);
    let sample_bytes = same_file_calibration_sample_bytes(file.size_bytes, file_chunk_size);
    if sample_bytes == 0 {
        return Err("empty calibration sample".to_string());
    }

    let calibration_root = adb::remote_path_join(
        resolved_target_root,
        &PathBuf::from(".nekotrans-calibration"),
    );
    let _ = adb::remove_remote_path(serial, &calibration_root);
    let result = (|| {
        let calibration_id = sanitize_task_id(&format!("{task_id}-{}", epoch_ms()));
        let adb_relative =
            PathBuf::from(format!(".nekotrans-calibration/{calibration_id}-adb.bin"));
        let adb_remote_file = adb::remote_path_join(resolved_target_root, &adb_relative);
        let adb_started = Instant::now();
        adb::write_file_chunk_at_offset(
            serial,
            &file.local_path,
            &adb_remote_file,
            0,
            0,
            sample_bytes,
        )
        .map_err(|err| err.to_string())?;
        let adb_elapsed = adb_started.elapsed();

        let wifi_relative = format!(".nekotrans-calibration/{calibration_id}-wifi.bin");
        let mut input = fs::File::open(&file.local_path).map_err(|err| err.to_string())?;
        let mut payload = vec![0u8; sample_bytes as usize];
        agent::start_file(host, &wifi_relative, sample_bytes).map_err(|err| err.to_string())?;
        let wifi_started = Instant::now();
        input
            .read_exact(&mut payload)
            .map_err(|err| err.to_string())?;
        let reply = agent::push_chunk_binary(host, &wifi_relative, 0, 0, &payload)
            .map_err(|err| err.to_string())?;
        ensure_chunk_response_status(&reply.payload, &["written", "already_committed"])?;
        let wifi_elapsed = wifi_started.elapsed();

        Ok(SameFileLaneCalibration {
            sample_bytes,
            adb_bytes_per_second: bytes_per_second(sample_bytes, adb_elapsed)?,
            wifi_bytes_per_second: bytes_per_second(sample_bytes, wifi_elapsed)?,
        })
    })();
    let _ = adb::remove_remote_path(serial, &calibration_root);
    result
}

fn same_file_calibration_sample_bytes(file_size: u64, chunk_size: u64) -> u64 {
    let preferred = chunk_size.clamp(8 * 1024 * 1024, 64 * 1024 * 1024);
    file_size.min(preferred)
}

fn same_file_wifi_stride_from_rates(adb_bytes_per_second: f64, wifi_bytes_per_second: f64) -> u32 {
    if !adb_bytes_per_second.is_finite()
        || !wifi_bytes_per_second.is_finite()
        || adb_bytes_per_second <= 0.0
        || wifi_bytes_per_second <= 0.0
    {
        return same_file_wifi_stride(0);
    }
    ((adb_bytes_per_second + wifi_bytes_per_second) / wifi_bytes_per_second)
        .ceil()
        .clamp(2.0, 16.0) as u32
}

fn bytes_per_second(bytes: u64, elapsed: Duration) -> Result<f64, String> {
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return Err("calibration timer returned zero duration".to_string());
    }
    Ok(bytes as f64 / seconds)
}

fn format_bytes_per_second(bytes_per_second: f64) -> String {
    if bytes_per_second <= 0.0 || !bytes_per_second.is_finite() {
        return "n/a".to_string();
    }
    format!("{:.2}MB/s", bytes_per_second / 1024.0 / 1024.0)
}

fn flush_same_file_wifi_batch(
    host: &str,
    task_id: &str,
    relative_path: &str,
    remote_temp_file: &str,
    pending: &mut Vec<PendingWifiChunk>,
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: &Arc<Mutex<TransferEngine>>,
    aggregate: &Arc<Mutex<DualTransferAggregate>>,
) -> Result<(), String> {
    if pending.is_empty() {
        return Ok(());
    }

    let batch_result = {
        let frames = pending
            .iter()
            .map(|chunk| agent::BinaryChunkFrame {
                chunk_index: chunk.chunk.chunk_index,
                offset: chunk.offset,
                payload: chunk.payload.as_slice(),
            })
            .collect::<Vec<_>>();
        agent::push_chunk_batch_binary(host, relative_path, &frames)
            .map_err(|err| err.to_string())
            .and_then(|reply| ensure_chunk_response_status(&reply.payload, &["batch_written"]))
    };

    if let Err(batch_error) = batch_result {
        for chunk in pending.iter() {
            push_wifi_chunk_with_recovery(
                host,
                relative_path,
                chunk.chunk.chunk_index,
                chunk.offset,
                &chunk.payload,
            )
            .map_err(|fallback_error| {
                format!("Wi-Fi batch failed: {batch_error}; fallback failed: {fallback_error}")
            })?;
        }
    }

    for chunk in pending.drain(..) {
        note_dual_adb_progress(
            registry,
            task_id,
            aggregate,
            relative_path,
            remote_temp_file,
            "dual-same-file-chunk-pushed",
            &format!(
                "Wi-Fi chunk {} committed via batch",
                chunk.chunk.chunk_index
            ),
            chunk.length,
            false,
            true,
        );
        if let Ok(mut engine) = engine.lock() {
            let _ = engine.record_real_chunk_commit_pending(
                task_id,
                chunk.chunk,
                LaneAssignment::Wifi,
                false,
            );
        }
    }

    Ok(())
}

fn partition_dual_transfer_files(
    files: Vec<LocalTransferFile>,
) -> (
    Vec<LocalTransferFile>,
    Vec<LocalTransferFile>,
    Vec<LocalTransferFile>,
) {
    let mut adb_files = Vec::new();
    let mut wifi_files = Vec::new();
    let mut same_file_dual_files = Vec::new();
    let mut send_small_to_wifi = true;

    for file in files {
        if is_large_file(file.size_bytes, 32 * 1024 * 1024) {
            same_file_dual_files.push(file);
        } else if send_small_to_wifi {
            wifi_files.push(file);
            send_small_to_wifi = false;
        } else {
            adb_files.push(file);
            send_small_to_wifi = true;
        }
    }

    (adb_files, wifi_files, same_file_dual_files)
}

#[derive(Default)]
struct DualTransferAggregate {
    total_files: usize,
    total_bytes: u64,
    pushed_files: usize,
    skipped_files: usize,
    pushed_chunks: u64,
    skipped_chunks: u64,
    bytes_scanned: u64,
    bytes_pushed: u64,
    relative_path: String,
    remote_path: String,
    last_event: String,
    last_message: String,
}

impl DualTransferAggregate {
    fn new(total_files: usize, total_bytes: u64) -> Self {
        Self {
            total_files,
            total_bytes,
            ..Self::default()
        }
    }
}

fn note_dual_file_started(
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    task_id: &str,
    aggregate: &Arc<Mutex<DualTransferAggregate>>,
    file: &LocalTransferFile,
    remote_path: &str,
    event: &str,
) {
    if let Ok(mut aggregate) = aggregate.lock() {
        aggregate.bytes_scanned =
            (aggregate.bytes_scanned + file.size_bytes).min(aggregate.total_bytes);
        aggregate.relative_path = file.relative_path.to_string_lossy().to_string();
        aggregate.remote_path = remote_path.to_string();
        aggregate.last_event = event.to_string();
        aggregate.last_message = format!("starting {}", aggregate.relative_path);
        sync_dual_card_from_aggregate(registry, task_id, &aggregate, "Running");
    }
}

fn note_dual_adb_progress(
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    task_id: &str,
    aggregate: &Arc<Mutex<DualTransferAggregate>>,
    relative_path: &str,
    remote_path: &str,
    event: &str,
    message: &str,
    chunk_length: u64,
    was_skipped: bool,
    was_pushed: bool,
) {
    if let Ok(mut aggregate) = aggregate.lock() {
        aggregate.relative_path = relative_path.to_string();
        aggregate.remote_path = remote_path.to_string();
        aggregate.last_event = event.to_string();
        aggregate.last_message = message.to_string();
        if was_skipped {
            aggregate.skipped_chunks += 1;
        }
        if was_pushed {
            aggregate.pushed_chunks += 1;
            aggregate.bytes_pushed =
                (aggregate.bytes_pushed + chunk_length).min(aggregate.total_bytes);
        }
        sync_dual_card_from_aggregate(registry, task_id, &aggregate, "Running");
    }
}

fn note_dual_file_finished(
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    task_id: &str,
    aggregate: &Arc<Mutex<DualTransferAggregate>>,
    relative_path: &str,
    remote_path: &str,
    was_skipped: bool,
) {
    if let Ok(mut aggregate) = aggregate.lock() {
        aggregate.relative_path = relative_path.to_string();
        aggregate.remote_path = remote_path.to_string();
        aggregate.last_event = if was_skipped {
            "dual-file-skipped".to_string()
        } else {
            "dual-file-completed".to_string()
        };
        aggregate.last_message = if was_skipped {
            format!("{relative_path} skipped")
        } else {
            format!("{relative_path} completed")
        };
        if was_skipped {
            aggregate.skipped_files += 1;
        } else {
            aggregate.pushed_files += 1;
        }
        sync_dual_card_from_aggregate(registry, task_id, &aggregate, "Running");
    }
}

fn note_dual_stage(
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    task_id: &str,
    aggregate: &Arc<Mutex<DualTransferAggregate>>,
    event: &str,
    message: &str,
) {
    if let Ok(mut aggregate) = aggregate.lock() {
        aggregate.last_event = event.to_string();
        aggregate.last_message = message.to_string();
        sync_dual_card_from_aggregate(registry, task_id, &aggregate, "Running");
    }
}

fn sync_dual_card_from_aggregate(
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    task_id: &str,
    aggregate: &DualTransferAggregate,
    state: &str,
) {
    if let Ok(mut transfers) = registry.lock() {
        if let Some(entry) = transfers.get_mut(task_id) {
            entry.view.state = state.to_string();
            entry.view.current_file = (aggregate.pushed_files + aggregate.skipped_files + 1)
                .min(aggregate.total_files.max(1));
            entry.view.total_files = aggregate.total_files;
            entry.view.pushed_files = aggregate.pushed_files;
            entry.view.skipped_files = aggregate.skipped_files;
            entry.view.pushed_chunks = aggregate.pushed_chunks;
            entry.view.skipped_chunks = aggregate.skipped_chunks;
            entry.view.bytes_scanned = aggregate.bytes_scanned;
            entry.view.bytes_pushed = aggregate.bytes_pushed;
            entry.view.relative_path = aggregate.relative_path.clone();
            entry.view.remote_path = aggregate.remote_path.clone();
            entry.view.last_event = aggregate.last_event.clone();
            entry.view.last_message = aggregate.last_message.clone();
        }
    }
}

fn run_adb_android_to_pc(
    serial: &str,
    task_id: &str,
    remote_path: &str,
    local_root: &Path,
    verify_enabled: bool,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
) -> Result<String, String> {
    let size_bytes = adb::stat_remote_file_size(serial, remote_path)
        .map_err(|err| err.to_string())?
        .unwrap_or(0);

    if let Ok(mut transfers) = registry.lock() {
        if let Some(entry) = transfers.get_mut(task_id) {
            entry.view.state = "Running".to_string();
            entry.view.current_file = 1;
            entry.view.total_files = 1;
            entry.view.bytes_scanned = size_bytes;
            entry.view.relative_path = remote_path.to_string();
            entry.view.remote_path = local_root.to_string_lossy().to_string();
            entry.view.last_event = "adb-pull-started".to_string();
            entry.view.last_message = "ADB pull started".to_string();
        }
    }

    let output = adb::pull_path_from_device(serial, remote_path, local_root)
        .map_err(|err| err.to_string())?;

    let local_target = resolve_adb_pull_local_target(local_root, remote_path)?;
    if verify_enabled {
        verify_adb_android_to_pc_transfer(serial, remote_path, &local_target)?;
    }

    if let Ok(mut engine) = engine.lock() {
        let _ = engine.record_real_file_complete(task_id, 0, LaneAssignment::Adb, false);
    }

    if let Ok(mut transfers) = registry.lock() {
        if let Some(entry) = transfers.get_mut(task_id) {
            entry.view.pushed_files = 1;
            entry.view.bytes_pushed = size_bytes;
            entry.view.last_event = "adb-pull-completed".to_string();
            entry.view.last_message = if verify_enabled {
                format!("{output}\nADB verify completed")
            } else {
                output.clone()
            };
        }
    }

    Ok(format!(
        "ADB Android -> PC pull completed: remote={remote_path} local={} bytes={size_bytes}\n{output}",
        local_root.display()
    ))
}

fn verify_adb_pc_to_android_transfer(
    serial: &str,
    local_path: &Path,
    remote_root: &str,
) -> Result<(), String> {
    let files = collect_local_transfer_files(local_path)?;
    let local_is_dir = local_path.is_dir();
    for file in files {
        let remote_file = if local_is_dir {
            adb_like_remote_join(remote_root, &file.relative_path)
        } else {
            remote_root.to_string()
        };
        let local_digest = blake3_digest_file(&file.local_path)?;
        let remote_digest = adb::blake3_digest_remote_file(serial, &remote_file)
            .map_err(|err| err.to_string())?
            .ok_or_else(|| format!("remote file not found for verify: {remote_file}"))?;
        if local_digest != remote_digest {
            return Err(format!(
                "ADB verify failed for {}: local={} remote={}",
                file.relative_path.to_string_lossy(),
                local_digest,
                remote_digest
            ));
        }
    }
    Ok(())
}

fn verify_adb_android_to_pc_transfer(
    serial: &str,
    remote_path: &str,
    local_target: &Path,
) -> Result<(), String> {
    let local_digest = blake3_digest_file(local_target)?;
    let remote_digest = adb::blake3_digest_remote_file(serial, remote_path)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("remote file not found for verify: {remote_path}"))?;
    if local_digest != remote_digest {
        return Err(format!(
            "ADB verify failed for {remote_path}: local={local_digest} remote={remote_digest}"
        ));
    }
    Ok(())
}

fn resolve_adb_pull_local_target(local_root: &Path, remote_path: &str) -> Result<PathBuf, String> {
    let normalized = remote_path.replace('\\', "/");
    let remote_name = normalized
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .ok_or_else(|| format!("remote path has no file name: {remote_path}"))?;
    Ok(local_root.join(remote_name))
}

fn adb_like_remote_join(remote_root: &str, relative_path: &Path) -> String {
    let cleaned = relative_path
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    let base = remote_root.trim_end_matches('/');
    if base.is_empty() {
        cleaned
    } else if cleaned.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{cleaned}")
    }
}

fn run_wifi_android_to_pc(
    host: &str,
    task_id: &str,
    source_relative: &str,
    target_root: &Path,
    chunk_size: u64,
    verify_enabled: bool,
    pause_requested: Arc<AtomicBool>,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
) -> Result<String, String> {
    agent::start_task(host, task_id).map_err(|err| err.to_string())?;
    let source = resolve_agent_pull_source(source_relative)?;
    if let Some(root) = &source.agent_root {
        agent::set_target_root(host, root).map_err(|err| err.to_string())?;
    }
    let checkpoint = load_task_checkpoint(&engine, task_id);
    let stat =
        agent::stat_file(host, &source.agent_relative_path).map_err(|err| err.to_string())?;
    let size_bytes = json_u64_field(&stat.payload, "size_bytes")
        .ok_or_else(|| format!("agent stat did not include size_bytes: {}", stat.payload))?;
    let target_file = safe_local_join(target_root, &source.local_relative_path)?;
    if let Some(parent) = target_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let mut output = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&target_file)
        .map_err(|err| err.to_string())?;
    let completed_chunks = completed_chunks_for_file(checkpoint.as_ref(), 0);
    let all_chunks_completed =
        is_file_fully_checkpointed(size_bytes, chunk_size, &completed_chunks);
    if all_chunks_completed {
        if let Ok(mut engine) = engine.lock() {
            let _ = engine.record_real_file_complete(task_id, 0, LaneAssignment::Wifi, true);
        }
        return Ok(format!(
            "Wi-Fi Android -> PC transfer resumed from checkpoint: bytes_pulled={} chunks={}",
            size_bytes,
            completed_chunks.len()
        ));
    }

    let mut pulled_chunks = 0u64;
    let mut skipped_chunks = 0u64;
    let total_chunks = chunk_count_for_size(size_bytes, chunk_size);

    for chunk_index in 0..total_chunks {
        if pause_requested.load(Ordering::Relaxed) {
            return Err("transfer paused at chunk boundary".to_string());
        }
        let offset = chunk_index as u64 * chunk_size;
        let length = chunk_length_for(size_bytes, chunk_size, chunk_index);
        if completed_chunks.contains(&chunk_index) {
            skipped_chunks += 1;
            continue;
        }
        let (bytes, pull_path) =
            pull_wifi_chunk_bytes(host, &source.agent_relative_path, offset, length)?;
        output
            .seek(SeekFrom::Start(offset))
            .map_err(|err| err.to_string())?;
        output.write_all(&bytes).map_err(|err| err.to_string())?;
        let chunk_length = bytes.len() as u64;
        pulled_chunks += 1;

        if let Ok(mut engine) = engine.lock() {
            let _ = engine.record_real_chunk_commit(
                task_id,
                ChunkDescriptor {
                    file_index: 0,
                    chunk_index,
                    offset,
                    length: chunk_length,
                },
                LaneAssignment::Wifi,
                false,
            );
        }

        if let Ok(mut transfers) = registry.lock() {
            if let Some(entry) = transfers.get_mut(task_id) {
                entry.view.state = "Running".to_string();
                entry.view.current_file = 1;
                entry.view.total_files = 1;
                entry.view.pushed_chunks = pulled_chunks;
                entry.view.skipped_chunks = skipped_chunks;
                entry.view.bytes_scanned = size_bytes;
                entry.view.bytes_pushed = offset + chunk_length;
                entry.view.relative_path = source_relative.to_string();
                entry.view.remote_path = target_file.to_string_lossy().to_string();
                entry.view.last_event = "wifi-chunk-pulled".to_string();
                entry.view.last_message =
                    format!("Wi-Fi chunk {chunk_index} pulled via {pull_path}");
            }
        }
    }

    if let Ok(mut engine) = engine.lock() {
        let _ = engine.record_real_file_complete(task_id, 0, LaneAssignment::Wifi, false);
    }

    if verify_enabled {
        verify_android_remote_file(host, &source.agent_relative_path, &target_file, size_bytes)?;
    }

    Ok(format!(
        "Wi-Fi Android -> PC transfer completed: bytes_pulled={} chunks={pulled_chunks} skipped_chunks={skipped_chunks}",
        size_bytes
    ))
}

fn pull_wifi_chunk_bytes(
    host: &str,
    source_relative: &str,
    offset: u64,
    length: u64,
) -> Result<(Vec<u8>, &'static str), String> {
    match agent::pull_chunk_binary(host, source_relative, offset, length) {
        Ok(reply) => {
            let header_length = json_u64_field(&reply.header, "length").ok_or_else(|| {
                format!(
                    "binary pull header did not include length: {}",
                    reply.header
                )
            })?;
            if header_length != reply.payload.len() as u64 {
                return Err(format!(
                    "binary pull length mismatch: header={} payload={}",
                    header_length,
                    reply.payload.len()
                ));
            }
            if header_length != length {
                return Err(format!(
                    "binary pull returned short chunk for {source_relative}: expected={length} actual={header_length}"
                ));
            }
            Ok((reply.payload, "binary"))
        }
        Err(binary_error) => {
            let reply = agent::pull_chunk_payload(host, source_relative, offset, length).map_err(
                |fallback_error| {
                    format!(
                        "binary pull failed: {binary_error}; base64 fallback failed: {fallback_error}"
                    )
                },
            )?;
            let payload = json_string_field(&reply.payload, "payload").ok_or_else(|| {
                format!(
                    "binary pull failed: {binary_error}; base64 chunk response did not include payload: {}",
                    reply.payload
                )
            })?;
            let bytes = base64_decode(&payload)?;
            if bytes.len() as u64 != length {
                return Err(format!(
                    "base64 pull returned short chunk for {source_relative}: expected={length} actual={}",
                    bytes.len()
                ));
            }
            Ok((bytes, "base64-fallback"))
        }
    }
}

struct AgentPullSource {
    agent_root: Option<String>,
    agent_relative_path: String,
    local_relative_path: String,
}

fn resolve_agent_pull_source(source_path: &str) -> Result<AgentPullSource, String> {
    let normalized = source_path.replace('\\', "/");
    if normalized.starts_with('/') {
        let trimmed = normalized.trim_end_matches('/');
        let (parent, file_name) = trimmed
            .rsplit_once('/')
            .ok_or_else(|| format!("remote path has no file name: {source_path}"))?;
        let file_name = file_name.trim();
        if file_name.is_empty() {
            return Err(format!("remote path has no file name: {source_path}"));
        }
        let agent_root = if parent.is_empty() {
            "/".to_string()
        } else {
            parent.to_string()
        };
        return Ok(AgentPullSource {
            agent_root: Some(agent_root),
            agent_relative_path: file_name.to_string(),
            local_relative_path: file_name.to_string(),
        });
    }

    let agent_relative_path = normalized
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != "." && *segment != "..")
        .map(|segment| {
            segment
                .chars()
                .filter(|character| !character.is_control() && *character != '\0')
                .collect::<String>()
        })
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if agent_relative_path.is_empty() {
        return Err("source path did not contain a safe Android file name".to_string());
    }

    Ok(AgentPullSource {
        agent_root: None,
        local_relative_path: agent_relative_path.clone(),
        agent_relative_path,
    })
}

#[tauri::command]
fn pause_adb_transfer(
    task_id: String,
    registry: State<'_, AdbTransferRegistry>,
) -> Result<AdbTransferCard, String> {
    let mut transfers = registry.0.lock().map_err(|err| err.to_string())?;
    let entry = transfers
        .get_mut(&task_id)
        .ok_or_else(|| format!("unknown adb transfer {task_id}"))?;
    entry.pause_requested.store(true, Ordering::Relaxed);
    entry.view.state = "Pausing".to_string();
    entry.view.last_event = "pause-requested".to_string();
    entry.view.last_message = "pause requested; waiting for current chunk boundary".to_string();
    Ok(entry.view.clone())
}

#[tauri::command]
fn resume_adb_transfer(
    task_id: String,
    registry: State<'_, AdbTransferRegistry>,
    engine: State<'_, DesktopEngine>,
) -> Result<AdbTransferCard, String> {
    if let Ok(transfers) = registry.0.lock() {
        if let Some(entry) = transfers.get(&task_id) {
            if entry.view.state == "Running" || entry.view.state == "Pausing" {
                return Ok(entry.view.clone());
            }
        }
    }

    if let Ok(mut engine) = engine.0.lock() {
        let _ = engine.resume_task(&task_id);
    }

    let record = TaskFileRecord::load(&task_id)?;
    start_adb_transfer_from_record(record, registry, engine)
}

#[tauri::command]
fn list_adb_transfers(
    registry: State<'_, AdbTransferRegistry>,
) -> Result<Vec<AdbTransferCard>, String> {
    let transfers = registry.0.lock().map_err(|err| err.to_string())?;
    Ok(transfers.values().map(|entry| entry.view.clone()).collect())
}

#[tauri::command]
fn probe_wifi_agent(host: String) -> Result<AgentProbeCard, String> {
    let hello = agent::hello(&host).map_err(|err| err.to_string())?;
    let ping = agent::ping(&host).map_err(|err| err.to_string())?;
    let snapshot = agent::task_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        hello_payload: hello.payload,
        ping_payload: ping.payload,
        snapshot_payload: snapshot.payload,
    })
}

#[tauri::command]
fn start_wifi_agent_task(host: String, task_id: String) -> Result<AgentTaskProbeCard, String> {
    let start = agent::start_task(&host, &task_id).map_err(|err| err.to_string())?;
    let snapshot = agent::task_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentTaskProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        task_id,
        start_payload: start.payload,
        snapshot_payload: snapshot.payload,
    })
}

#[tauri::command]
fn pause_wifi_agent_task(host: String) -> Result<AgentStateProbeCard, String> {
    let pause = agent::pause_task(&host).map_err(|err| err.to_string())?;
    let snapshot = agent::task_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentStateProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        action_payload: pause.payload,
        snapshot_payload: snapshot.payload,
    })
}

#[tauri::command]
fn resume_wifi_agent_task(host: String) -> Result<AgentStateProbeCard, String> {
    let resume = agent::resume_task(&host).map_err(|err| err.to_string())?;
    let snapshot = agent::task_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentStateProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        action_payload: resume.payload,
        snapshot_payload: snapshot.payload,
    })
}

#[tauri::command]
fn start_wifi_agent_file(
    host: String,
    relative_path: String,
    size_bytes: u64,
) -> Result<AgentFileProbeCard, String> {
    let start =
        agent::start_file(&host, &relative_path, size_bytes).map_err(|err| err.to_string())?;
    let snapshot = agent::file_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentFileProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        relative_path,
        action_payload: start.payload,
        snapshot_payload: snapshot.payload,
    })
}

#[tauri::command]
fn ack_wifi_agent_chunk(host: String, chunk_index: u32) -> Result<AgentFileProbeCard, String> {
    let ack = agent::ack_chunk(&host, chunk_index).map_err(|err| err.to_string())?;
    let snapshot = agent::file_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentFileProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        relative_path: String::new(),
        action_payload: ack.payload,
        snapshot_payload: snapshot.payload,
    })
}

#[tauri::command]
fn push_wifi_agent_sample_chunk(host: String) -> Result<AgentFileProbeCard, String> {
    let relative_path = "nekotrans-wifi-smoke.txt".to_string();
    let payload = b"Nekotrans Wi-Fi payload smoke\n";
    let _ = agent::start_task(&host, "wifi-payload-smoke").map_err(|err| err.to_string())?;
    let _ = agent::start_file(&host, &relative_path, payload.len() as u64)
        .map_err(|err| err.to_string())?;
    let push = agent::push_chunk_payload(&host, &relative_path, 0, 0, payload)
        .map_err(|err| err.to_string())?;
    let snapshot = agent::file_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentFileProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        relative_path,
        action_payload: push.payload,
        snapshot_payload: snapshot.payload,
    })
}

#[tauri::command]
fn verify_wifi_agent_file(
    host: String,
    relative_path: String,
) -> Result<AgentFileProbeCard, String> {
    let verify = agent::verify_file(&host, &relative_path).map_err(|err| err.to_string())?;
    let snapshot = agent::file_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentFileProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        relative_path,
        action_payload: verify.payload,
        snapshot_payload: snapshot.payload,
    })
}

#[tauri::command]
fn pull_wifi_agent_chunk(
    host: String,
    relative_path: String,
    offset: u64,
    length: u64,
) -> Result<AgentFileProbeCard, String> {
    let pull = agent::pull_chunk_payload(&host, &relative_path, offset, length)
        .map_err(|err| err.to_string())?;
    let snapshot = agent::file_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentFileProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        relative_path,
        action_payload: pull.payload,
        snapshot_payload: snapshot.payload,
    })
}

#[tauri::command]
fn fetch_wifi_agent_logs(host: String) -> Result<AgentStateProbeCard, String> {
    let logs = agent::log_snapshot(&host).map_err(|err| err.to_string())?;
    persist_agent_log_batch(&host, &logs.payload)?;
    let snapshot = agent::task_snapshot(&host).map_err(|err| err.to_string())?;

    Ok(AgentStateProbeCard {
        host,
        port: agent::DEFAULT_AGENT_PORT,
        action_payload: logs.payload,
        snapshot_payload: snapshot.payload,
    })
}

fn sanitize_task_id(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn parse_direction_label(value: &str) -> Result<Direction, String> {
    match value {
        "PC -> Android" => Ok(Direction::PcToAndroid),
        "Android -> PC" => Ok(Direction::AndroidToPc),
        other => Err(format!("unsupported direction: {other}")),
    }
}

fn parse_transport_label(value: &str) -> Result<TransportMode, String> {
    match value {
        "ADB-only" => Ok(TransportMode::AdbOnly),
        "Wi-Fi-only" => Ok(TransportMode::WifiOnly),
        "Dual Track" => Ok(TransportMode::Dual),
        other => Err(format!("unsupported transport mode: {other}")),
    }
}

fn default_docs_path() -> Result<PathBuf, String> {
    let current_dir = std::env::current_dir().map_err(|err| err.to_string())?;
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        current_dir.join("docs"),
        manifest_dir.join("..").join("..").join("docs"),
        manifest_dir.join("..").join("..").join("..").join("docs"),
    ];

    for candidate in &candidates {
        if candidate.is_dir() {
            return Ok(candidate.clone());
        }
    }

    let tried = candidates
        .iter()
        .map(|path| format!("  - {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "Demo docs directory was not found. Tried:\n{tried}"
    ))
}

fn nekotrans_state_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join(".nekotrans")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DesktopWorkerSmokeMode {
    Adb,
    Wifi,
    Dual,
}

impl DesktopWorkerSmokeMode {
    fn label(self) -> &'static str {
        match self {
            Self::Adb => "adb",
            Self::Wifi => "wifi",
            Self::Dual => "dual",
        }
    }

    fn transport_mode(self) -> TransportMode {
        match self {
            Self::Adb => TransportMode::AdbOnly,
            Self::Wifi => TransportMode::WifiOnly,
            Self::Dual => TransportMode::Dual,
        }
    }

    fn chunk_size(self) -> u64 {
        match self {
            Self::Wifi => 256 * 1024,
            Self::Adb | Self::Dual => 8 * 1024 * 1024,
        }
    }
}

pub fn run_desktop_worker_smoke(
    serial: &str,
    agent_host: Option<&str>,
    mode_labels: &[String],
    cleanup: bool,
) -> Result<String, String> {
    let serial = serial.trim();
    if serial.is_empty() {
        return Err("desktop worker smoke requires an ADB serial".to_string());
    }

    let modes = parse_desktop_worker_smoke_modes(mode_labels)?;
    let needs_agent = modes.iter().any(|mode| {
        matches!(
            mode,
            DesktopWorkerSmokeMode::Wifi | DesktopWorkerSmokeMode::Dual
        )
    });
    let host = if needs_agent {
        Some(resolve_desktop_worker_smoke_host(serial, agent_host)?)
    } else {
        None
    };

    let smoke_id = format!("desktop-worker-smoke-{}", epoch_ms());
    let local_root = nekotrans_state_root()
        .join("desktop-worker-smoke")
        .join(&smoke_id);
    let checkpoint_root = local_root.join("checkpoints");
    let remote_base = format!("/sdcard/Download/NekotransDesktopSmoke/{smoke_id}");
    fs::create_dir_all(&local_root).map_err(|err| err.to_string())?;

    let engine = Arc::new(Mutex::new(TransferEngine::new(&checkpoint_root)));
    let registry = Arc::new(Mutex::new(BTreeMap::<String, AdbTransferEntry>::new()));
    let mut summaries = Vec::new();
    let result = (|| {
        for mode in modes {
            let source_root = local_root.join(format!("source-{}", mode.label()));
            prepare_desktop_worker_smoke_fixture(
                &source_root,
                mode == DesktopWorkerSmokeMode::Dual,
            )?;
            let task_id = format!("{}-{}", smoke_id, mode.label());
            let remote_root = format!("{}/{}", remote_base, mode.label());
            create_desktop_worker_smoke_task(&engine, &task_id, mode, &source_root, &remote_root)?;
            insert_smoke_transfer_card(
                &registry,
                &task_id,
                match mode {
                    DesktopWorkerSmokeMode::Adb => serial.to_string(),
                    DesktopWorkerSmokeMode::Wifi => {
                        format!("wifi:{}", host.as_deref().unwrap_or(""))
                    }
                    DesktopWorkerSmokeMode::Dual => {
                        format!("dual:{serial}+wifi:{}", host.as_deref().unwrap_or(""))
                    }
                },
                &remote_root,
            )?;

            let started = Instant::now();
            match mode {
                DesktopWorkerSmokeMode::Adb => run_desktop_worker_smoke_adb(
                    serial,
                    &task_id,
                    &source_root,
                    &remote_root,
                    mode.chunk_size(),
                    registry.clone(),
                    engine.clone(),
                )?,
                DesktopWorkerSmokeMode::Wifi => run_desktop_worker_smoke_wifi(
                    host.as_deref()
                        .ok_or_else(|| "agent host is required".to_string())?,
                    &task_id,
                    &source_root,
                    &remote_root,
                    mode.chunk_size(),
                    registry.clone(),
                    engine.clone(),
                )?,
                DesktopWorkerSmokeMode::Dual => run_desktop_worker_smoke_dual(
                    serial,
                    host.as_deref()
                        .ok_or_else(|| "agent host is required".to_string())?,
                    &task_id,
                    &source_root,
                    &remote_root,
                    mode.chunk_size(),
                    registry.clone(),
                    engine.clone(),
                )?,
            }
            let snapshot = assert_smoke_task_completed(&engine, &task_id)?;
            summaries.push(format!(
                "{} ok: files={} bytes={} elapsed={:.2}s",
                mode.label(),
                snapshot.total_files,
                snapshot.total_bytes,
                started.elapsed().as_secs_f64()
            ));
        }
        Ok(summaries.join("\n"))
    })();

    if cleanup {
        let _ = adb::remove_remote_path(serial, &remote_base);
        let _ = fs::remove_dir_all(&local_root);
    }

    result.map(|summary| {
        let host_line = host
            .map(|value| format!("\nagent_host={value}"))
            .unwrap_or_default();
        format!("desktop worker smoke completed for {serial}{host_line}\n{summary}")
    })
}

fn parse_desktop_worker_smoke_modes(
    mode_labels: &[String],
) -> Result<Vec<DesktopWorkerSmokeMode>, String> {
    if mode_labels.is_empty() {
        return Ok(vec![
            DesktopWorkerSmokeMode::Adb,
            DesktopWorkerSmokeMode::Wifi,
            DesktopWorkerSmokeMode::Dual,
        ]);
    }

    let mut modes = Vec::new();
    for label in mode_labels {
        for part in label.split(',') {
            let mode = match part.trim().to_ascii_lowercase().as_str() {
                "adb" | "adb-only" => DesktopWorkerSmokeMode::Adb,
                "wifi" | "wi-fi" | "wifi-only" | "wi-fi-only" => DesktopWorkerSmokeMode::Wifi,
                "dual" | "dual-track" => DesktopWorkerSmokeMode::Dual,
                "" => continue,
                other => return Err(format!("unknown desktop worker smoke mode: {other}")),
            };
            if !modes.contains(&mode) {
                modes.push(mode);
            }
        }
    }

    if modes.is_empty() {
        Err("desktop worker smoke mode list is empty".to_string())
    } else {
        Ok(modes)
    }
}

fn resolve_desktop_worker_smoke_host(
    serial: &str,
    agent_host: Option<&str>,
) -> Result<String, String> {
    if let Some(host) = agent_host.map(str::trim).filter(|value| !value.is_empty()) {
        if host.contains(':') {
            return Err(
                "agent host should be a host/IP without port; the desktop agent client uses 38997"
                    .to_string(),
            );
        }
        agent::hello(host).map_err(|err| err.to_string())?;
        return Ok(host.to_string());
    }

    let probes = adb::probe_adb_devices().map_err(|err| err.to_string())?;
    let device = probes
        .iter()
        .find(|device| device.discovered.serial == serial)
        .ok_or_else(|| format!("ADB device {serial} was not found"))?;
    let ip = device
        .wifi_agent_ip
        .as_deref()
        .ok_or_else(|| format!("ADB device {serial} did not report a Wi-Fi agent IP"))?;
    agent::hello(ip).map_err(|err| {
        format!(
            "direct LAN agent probe failed at {ip}: {err}; pass --host <agent-ip> after confirming the phone is reachable on port 38997"
        )
    })?;
    Ok(ip.to_string())
}

fn prepare_desktop_worker_smoke_fixture(
    source_root: &Path,
    include_large: bool,
) -> Result<(), String> {
    fs::create_dir_all(source_root.join("nested").join("empty")).map_err(|err| err.to_string())?;
    fs::write(
        source_root.join("hello.txt"),
        b"Nekotrans desktop worker smoke\n",
    )
    .map_err(|err| err.to_string())?;
    fs::write(
        source_root.join("nested").join("note.txt"),
        b"small file routed through the desktop worker\n",
    )
    .map_err(|err| err.to_string())?;
    if include_large {
        write_deterministic_smoke_file(&source_root.join("large-dual.bin"), 33 * 1024 * 1024, 17)?;
    }
    Ok(())
}

fn write_deterministic_smoke_file(path: &Path, size: usize, seed: u8) -> Result<(), String> {
    let mut file = fs::File::create(path).map_err(|err| err.to_string())?;
    let mut written = 0usize;
    let mut buffer = vec![0u8; 1024 * 1024];
    while written < size {
        let count = (size - written).min(buffer.len());
        for (index, byte) in buffer[..count].iter_mut().enumerate() {
            *byte = seed.wrapping_add(((written + index) % 251) as u8);
        }
        file.write_all(&buffer[..count])
            .map_err(|err| err.to_string())?;
        written += count;
    }
    Ok(())
}

fn create_desktop_worker_smoke_task(
    engine: &Arc<Mutex<TransferEngine>>,
    task_id: &str,
    mode: DesktopWorkerSmokeMode,
    source_root: &Path,
    remote_root: &str,
) -> Result<(), String> {
    let mut config = TaskConfig::new(
        task_id,
        Direction::PcToAndroid,
        mode.transport_mode(),
        true,
        source_root.to_path_buf(),
        remote_root,
    );
    config.chunk_size_bytes = mode.chunk_size();
    let mut engine = engine.lock().map_err(|err| err.to_string())?;
    engine
        .create_or_recover_task_from_paths(config, &[PathBuf::from(".")])
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn insert_smoke_transfer_card(
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    task_id: &str,
    lane_label: String,
    remote_root: &str,
) -> Result<(), String> {
    registry.lock().map_err(|err| err.to_string())?.insert(
        task_id.to_string(),
        AdbTransferEntry {
            view: AdbTransferCard::new(task_id, &lane_label, remote_root),
            pause_requested: Arc::new(AtomicBool::new(false)),
        },
    );
    Ok(())
}

fn run_desktop_worker_smoke_adb(
    serial: &str,
    task_id: &str,
    source_root: &Path,
    remote_root: &str,
    chunk_size: u64,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
) -> Result<(), String> {
    let task_id_for_progress = task_id.to_string();
    let registry_for_progress = registry.clone();
    let engine_for_progress = engine.clone();
    let result = adb::push_path_to_device_with_control(
        serial,
        source_root,
        remote_root,
        chunk_size,
        Some(adb::AdbTransferControl::new(Arc::new(AtomicBool::new(
            false,
        )))),
        move |progress| {
            if let Ok(mut transfers) = registry_for_progress.lock() {
                if let Some(entry) = transfers.get_mut(&task_id_for_progress) {
                    entry.view.state = "Running".to_string();
                    entry.view.current_file = progress.current_file;
                    entry.view.total_files = progress.total_files;
                    entry.view.pushed_files = progress.pushed_files;
                    entry.view.skipped_files = progress.skipped_files;
                    entry.view.pushed_chunks = progress.pushed_chunks;
                    entry.view.skipped_chunks = progress.skipped_chunks;
                    entry.view.bytes_scanned = progress.bytes_scanned;
                    entry.view.bytes_pushed = progress.bytes_pushed;
                    entry.view.last_event = progress.event.clone();
                    entry.view.last_message = progress.message.clone();
                    entry.view.remote_path = progress.remote_path.clone();
                    entry.view.relative_path = progress.relative_path.clone();
                }
            }

            if let Ok(mut engine) = engine_for_progress.lock() {
                match progress.event.as_str() {
                    "chunk-pushed" | "chunk-skipped" => {
                        if let Some(chunk_index) = progress.chunk_index {
                            let _ = engine.record_real_chunk_commit(
                                &task_id_for_progress,
                                ChunkDescriptor {
                                    file_index: progress.file_index,
                                    chunk_index,
                                    offset: chunk_index as u64 * chunk_size,
                                    length: progress.chunk_length,
                                },
                                LaneAssignment::Adb,
                                progress.event == "chunk-skipped",
                            );
                        }
                    }
                    "file-skipped" => {
                        let _ = engine.record_real_file_complete(
                            &task_id_for_progress,
                            progress.file_index,
                            LaneAssignment::Adb,
                            true,
                        );
                    }
                    "file-completed" => {
                        let _ = engine.record_real_file_complete(
                            &task_id_for_progress,
                            progress.file_index,
                            LaneAssignment::Adb,
                            false,
                        );
                    }
                    _ => {}
                }
            }
        },
    )
    .map_err(|err| err.to_string())
    .and_then(|_| verify_adb_pc_to_android_transfer(serial, source_root, remote_root));

    finish_smoke_transfer(task_id, result, registry, engine)
}

fn run_desktop_worker_smoke_wifi(
    host: &str,
    task_id: &str,
    source_root: &Path,
    remote_root: &str,
    chunk_size: u64,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
) -> Result<(), String> {
    let manifest = collect_local_transfer_manifest(source_root)?;
    let result = run_wifi_pc_to_android(
        host,
        task_id,
        remote_root,
        manifest.directories,
        manifest.files,
        chunk_size,
        true,
        Arc::new(AtomicBool::new(false)),
        registry.clone(),
        engine.clone(),
    );
    finish_smoke_transfer(task_id, result.map(|_| ()), registry, engine)
}

fn run_desktop_worker_smoke_dual(
    serial: &str,
    host: &str,
    task_id: &str,
    source_root: &Path,
    remote_root: &str,
    chunk_size: u64,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
) -> Result<(), String> {
    let manifest = collect_local_transfer_manifest(source_root)?;
    let directories = manifest.directories;
    let (adb_files, wifi_files, same_file_dual_files) =
        partition_dual_transfer_files(manifest.files);
    let total_files = adb_files.len() + wifi_files.len() + same_file_dual_files.len();
    let total_bytes = adb_files.iter().map(|file| file.size_bytes).sum::<u64>()
        + wifi_files.iter().map(|file| file.size_bytes).sum::<u64>()
        + same_file_dual_files
            .iter()
            .map(|file| file.size_bytes)
            .sum::<u64>();
    let aggregate = Arc::new(Mutex::new(DualTransferAggregate::new(
        total_files,
        total_bytes,
    )));
    let pause_requested = Arc::new(AtomicBool::new(false));

    let adb_serial = serial.to_string();
    let adb_task_id = task_id.to_string();
    let adb_remote_root = remote_root.to_string();
    let adb_registry = registry.clone();
    let adb_engine = engine.clone();
    let adb_aggregate = aggregate.clone();
    let adb_pause = adb::AdbTransferControl::new(pause_requested.clone());
    let adb_handle = thread::spawn(move || {
        run_dual_adb_pc_to_android_files(
            &adb_serial,
            &adb_task_id,
            &adb_remote_root,
            adb_files,
            chunk_size,
            true,
            adb_pause,
            adb_registry,
            adb_engine,
            adb_aggregate,
        )
    });

    let wifi_host = host.to_string();
    let wifi_task_id = task_id.to_string();
    let wifi_remote_root = remote_root.to_string();
    let wifi_registry = registry.clone();
    let wifi_engine = engine.clone();
    let wifi_aggregate = aggregate.clone();
    let wifi_pause = pause_requested.clone();
    let wifi_handle = thread::spawn(move || {
        run_dual_wifi_pc_to_android_files(
            &wifi_host,
            &wifi_task_id,
            &wifi_remote_root,
            directories,
            wifi_files,
            dual_wifi_chunk_size(chunk_size),
            true,
            wifi_pause,
            wifi_registry,
            wifi_engine,
            wifi_aggregate,
        )
    });

    let adb_result = adb_handle
        .join()
        .unwrap_or_else(|_| Err("ADB dual smoke worker panicked".to_string()));
    let wifi_result = wifi_handle
        .join()
        .unwrap_or_else(|_| Err("Wi-Fi dual smoke worker panicked".to_string()));

    let result = match (adb_result, wifi_result) {
        (Ok(_), Ok(_)) => {
            if same_file_dual_files.is_empty() {
                Ok("same-file Dual lane skipped".to_string())
            } else {
                run_dual_same_file_pc_to_android_files(
                    serial,
                    host,
                    task_id,
                    remote_root,
                    same_file_dual_files,
                    chunk_size,
                    true,
                    pause_requested,
                    registry.clone(),
                    engine.clone(),
                    aggregate,
                )
            }
        }
        (Err(err), _) => Err(err),
        (_, Err(err)) => Err(err),
    };

    finish_smoke_transfer(task_id, result.map(|_| ()), registry, engine)
}

fn finish_smoke_transfer(
    task_id: &str,
    result: Result<(), String>,
    registry: Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    engine: Arc<Mutex<TransferEngine>>,
) -> Result<(), String> {
    if let Ok(mut transfers) = registry.lock() {
        if let Some(entry) = transfers.get_mut(task_id) {
            match &result {
                Ok(()) => {
                    entry.view.state = "Completed".to_string();
                    entry.view.last_event = "completed".to_string();
                    entry.view.last_message = "desktop worker smoke completed".to_string();
                }
                Err(message) => {
                    entry.view.state = "Failed".to_string();
                    entry.view.last_event = "failed".to_string();
                    entry.view.last_message = message.clone();
                }
            }
        }
    }

    if let Err(message) = &result {
        if let Ok(mut engine) = engine.lock() {
            let _ = engine.record_task_failure(task_id, message.clone());
        }
    }
    result
}

fn assert_smoke_task_completed(
    engine: &Arc<Mutex<TransferEngine>>,
    task_id: &str,
) -> Result<EngineTaskSnapshot, String> {
    let engine = engine.lock().map_err(|err| err.to_string())?;
    let snapshot = engine
        .snapshots()
        .into_iter()
        .find(|snapshot| snapshot.task_id == task_id)
        .ok_or_else(|| format!("desktop worker smoke task {task_id} was not found"))?;
    if snapshot.state != TaskState::Completed {
        return Err(format!(
            "desktop worker smoke task {task_id} ended in {:?}: {:?}",
            snapshot.state, snapshot.last_error
        ));
    }
    Ok(snapshot)
}

fn collect_local_transfer_files(local_path: &Path) -> Result<Vec<LocalTransferFile>, String> {
    Ok(collect_local_transfer_manifest(local_path)?.files)
}

fn collect_local_transfer_manifest(local_path: &Path) -> Result<LocalTransferManifest, String> {
    if local_path.is_file() {
        let metadata = fs::metadata(local_path).map_err(|err| err.to_string())?;
        return Ok(LocalTransferManifest {
            directories: Vec::new(),
            files: vec![LocalTransferFile {
                file_index: 0,
                local_path: local_path.to_path_buf(),
                relative_path: local_path
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("file")),
                size_bytes: metadata.len(),
            }],
        });
    }

    if !local_path.is_dir() {
        return Err(format!(
            "local path does not exist: {}",
            local_path.display()
        ));
    }

    let mut manifest = LocalTransferManifest::default();
    collect_local_transfer_dir(local_path, local_path, &mut manifest)?;
    let files = &mut manifest.files;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    for (file_index, file) in files.iter_mut().enumerate() {
        file.file_index = file_index;
    }
    manifest
        .directories
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    manifest
        .directories
        .dedup_by(|left, right| left.relative_path == right.relative_path);
    Ok(manifest)
}

fn collect_local_transfer_dir(
    root: &Path,
    current: &Path,
    manifest: &mut LocalTransferManifest,
) -> Result<(), String> {
    for entry in fs::read_dir(current).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|err| err.to_string())?;
        if metadata.is_dir() {
            manifest.directories.push(LocalTransferDirectory {
                relative_path: path
                    .strip_prefix(root)
                    .map_err(|err| err.to_string())?
                    .to_path_buf(),
            });
            collect_local_transfer_dir(root, &path, manifest)?;
        } else if metadata.is_file() {
            manifest.files.push(LocalTransferFile {
                file_index: 0,
                local_path: path.clone(),
                relative_path: path
                    .strip_prefix(root)
                    .map_err(|err| err.to_string())?
                    .to_path_buf(),
                size_bytes: metadata.len(),
            });
        }
    }
    Ok(())
}

fn load_task_checkpoint(
    engine: &Arc<Mutex<TransferEngine>>,
    task_id: &str,
) -> Option<CheckpointEntry> {
    engine
        .lock()
        .ok()
        .and_then(|engine| engine.checkpoint_entry(task_id).ok())
}

fn completed_chunks_for_file(checkpoint: Option<&CheckpointEntry>, file_index: usize) -> Vec<u32> {
    checkpoint
        .and_then(|entry| entry.checkpoint.files.get(file_index))
        .map(|file| file.completed_chunks.clone())
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn prepare_same_file_resume_chunks(
    host: &str,
    task_id: &str,
    local_path: &Path,
    relative_path: &str,
    chunk_size: u64,
    total_chunks: u32,
    wifi_stride: u32,
    completed_chunks: Vec<u32>,
    registry: &Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>,
    aggregate: &Arc<Mutex<DualTransferAggregate>>,
) -> Result<Vec<u32>, String> {
    let mut chunks = completed_chunks
        .into_iter()
        .filter(|chunk_index| *chunk_index < total_chunks)
        .collect::<BTreeSet<_>>();
    if chunks.is_empty() {
        return Ok(Vec::new());
    }

    note_dual_stage(
        registry,
        task_id,
        aggregate,
        "dual-same-file-resume-check",
        &format!(
            "quick resume guard: checking {} of {} checkpointed chunks",
            same_file_resume_guard_chunks(&chunks, wifi_stride).len(),
            chunks.len()
        ),
    );

    let mut guards_ok = true;
    for chunk_index in same_file_resume_guard_chunks(&chunks, wifi_stride) {
        let offset = chunk_index as u64 * chunk_size;
        let length = chunk_length_for(
            local_path.metadata().map_err(|err| err.to_string())?.len(),
            chunk_size,
            chunk_index,
        );
        if !verify_same_file_checkpoint_chunk(host, local_path, relative_path, offset, length)? {
            guards_ok = false;
            break;
        }
    }

    if guards_ok {
        note_dual_stage(
            registry,
            task_id,
            aggregate,
            "dual-same-file-resume-check",
            "resume guard passed; trusting checkpoint and continuing transfer",
        );
        return Ok(chunks.into_iter().collect());
    }

    note_dual_stage(
        registry,
        task_id,
        aggregate,
        "dual-same-file-resume-scan",
        "resume guard mismatch; scanning checkpoint chunks before rewrite",
    );
    let mut verified = BTreeSet::new();
    for chunk_index in chunks.iter().copied() {
        let offset = chunk_index as u64 * chunk_size;
        let length = chunk_length_for(
            local_path.metadata().map_err(|err| err.to_string())?.len(),
            chunk_size,
            chunk_index,
        );
        if verify_same_file_checkpoint_chunk(host, local_path, relative_path, offset, length)? {
            verified.insert(chunk_index);
        }
    }
    chunks = verified;
    Ok(chunks.into_iter().collect())
}

fn same_file_resume_guard_chunks(completed_chunks: &BTreeSet<u32>, wifi_stride: u32) -> Vec<u32> {
    let mut guards = BTreeSet::new();
    if let Some(first) = completed_chunks.iter().next() {
        guards.insert(*first);
    }
    if let Some(last) = completed_chunks.iter().next_back() {
        guards.insert(*last);
    }

    for lane in [DualSameFileLane::Adb, DualSameFileLane::Wifi] {
        if let Some(first) = completed_chunks
            .iter()
            .copied()
            .find(|chunk_index| lane.owns_chunk(*chunk_index, wifi_stride))
        {
            guards.insert(first);
        }
        if let Some(last) = completed_chunks
            .iter()
            .copied()
            .rev()
            .find(|chunk_index| lane.owns_chunk(*chunk_index, wifi_stride))
        {
            guards.insert(last);
        }
    }

    completed_chunks
        .iter()
        .copied()
        .rev()
        .take(2)
        .for_each(|chunk_index| {
            guards.insert(chunk_index);
        });
    guards.into_iter().collect()
}

fn verify_same_file_checkpoint_chunk(
    host: &str,
    local_path: &Path,
    relative_path: &str,
    offset: u64,
    length: u64,
) -> Result<bool, String> {
    if length == 0 {
        return Ok(true);
    }
    let remote = agent::pull_chunk_binary(host, relative_path, offset, length)
        .map_err(|err| err.to_string())?;
    if remote.payload.len() != length as usize {
        return Ok(false);
    }
    let local_digest = blake3_digest_file_range(local_path, offset, length)?;
    let remote_digest = blake3::hash(&remote.payload).to_hex().to_string();
    Ok(local_digest == remote_digest)
}

fn wait_for_remote_file_size(
    host: &str,
    relative_path: &str,
    expected_size: u64,
) -> Result<u64, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_size = None;
    while Instant::now() <= deadline {
        let stat = agent::stat_file(host, relative_path).map_err(|err| err.to_string())?;
        let remote_size = json_u64_field(&stat.payload, "size_bytes")
            .ok_or_else(|| format!("agent stat did not include size_bytes: {}", stat.payload))?;
        if remote_size == expected_size {
            return Ok(remote_size);
        }
        last_size = Some(remote_size);
        thread::sleep(Duration::from_millis(250));
    }
    Ok(last_size.unwrap_or(0))
}

fn verify_android_remote_file(
    host: &str,
    relative_path: &str,
    local_path: &Path,
    expected_size: u64,
) -> Result<(), String> {
    let remote_size = wait_for_remote_file_size(host, relative_path, expected_size)?;
    if remote_size != expected_size {
        return Err(format!(
            "size mismatch for {relative_path}: local={expected_size} remote={remote_size}"
        ));
    }

    if should_sample_android_verify(expected_size) {
        verify_android_remote_file_samples(host, relative_path, local_path, expected_size)
    } else {
        let local_digest = blake3_digest_file(local_path)?;
        let verify = agent::verify_file(host, relative_path).map_err(|err| err.to_string())?;
        ensure_verify_algorithm(&verify.payload, "BLAKE3")?;
        let remote_digest = json_string_field(&verify.payload, "digest")
            .ok_or_else(|| format!("agent verify did not include digest: {}", verify.payload))?;
        if local_digest == remote_digest {
            Ok(())
        } else {
            Err(format!(
                "verify failed for {relative_path}: local={local_digest} remote={remote_digest}"
            ))
        }
    }
}

fn should_sample_android_verify(size_bytes: u64) -> bool {
    is_large_file(size_bytes, 128 * 1024 * 1024)
}

fn verify_android_remote_file_samples(
    host: &str,
    relative_path: &str,
    local_path: &Path,
    size_bytes: u64,
) -> Result<(), String> {
    for (offset, length) in android_verify_sample_ranges(size_bytes) {
        let remote = agent::pull_chunk_binary(host, relative_path, offset, length)
            .map_err(|err| err.to_string())?;
        if remote.payload.len() != length as usize {
            return Err(format!(
                "sample verify short read for {relative_path}: offset={offset} expected={length} actual={}",
                remote.payload.len()
            ));
        }
        let local_digest = blake3_digest_file_range(local_path, offset, length)?;
        let remote_digest = blake3::hash(&remote.payload).to_hex().to_string();
        if local_digest != remote_digest {
            return Err(format!(
                "sample verify failed for {relative_path}: offset={offset} length={length}"
            ));
        }
    }
    Ok(())
}

fn android_verify_sample_ranges(size_bytes: u64) -> Vec<(u64, u64)> {
    let sample_len = size_bytes.min(256 * 1024);
    if sample_len == 0 {
        return vec![(0, 0)];
    }

    let mut offsets = BTreeSet::new();
    offsets.insert(0);
    offsets.insert(size_bytes.saturating_sub(sample_len));
    offsets.insert(((size_bytes.saturating_sub(sample_len)) / 2).min(size_bytes - sample_len));

    offsets
        .into_iter()
        .map(|offset| (offset, sample_len))
        .collect()
}

fn chunk_count_for_size(size_bytes: u64, chunk_size: u64) -> u32 {
    if size_bytes == 0 {
        return 1;
    }
    size_bytes.div_ceil(chunk_size.max(1)) as u32
}

fn chunk_length_for(size_bytes: u64, chunk_size: u64, chunk_index: u32) -> u64 {
    if size_bytes == 0 {
        return 0;
    }
    let offset = chunk_index as u64 * chunk_size.max(1);
    size_bytes.saturating_sub(offset).min(chunk_size.max(1))
}

fn dual_wifi_chunk_size(requested: u64) -> u64 {
    wifi_transfer_chunk_size(4 * 1024 * 1024 * 1024, requested).min(4 * 1024 * 1024)
}

fn dual_same_file_chunk_size(file_size: u64, requested: u64) -> u64 {
    let adaptive = if file_size >= 4 * 1024 * 1024 * 1024 {
        64 * 1024 * 1024
    } else if file_size >= 1024 * 1024 * 1024 {
        32 * 1024 * 1024
    } else {
        8 * 1024 * 1024
    };
    requested.max(adaptive).clamp(1, 64 * 1024 * 1024)
}

fn wifi_task_chunk_size_for_files(files: &[LocalTransferFile], requested: u64) -> u64 {
    files
        .iter()
        .map(|file| wifi_transfer_chunk_size(file.size_bytes, requested))
        .max()
        .unwrap_or_else(|| wifi_transfer_chunk_size(0, requested))
}

fn wifi_transfer_chunk_size(file_size: u64, requested: u64) -> u64 {
    let requested = requested.clamp(64 * 1024, 8 * 1024 * 1024);
    let adaptive = if file_size >= 4 * 1024 * 1024 * 1024 {
        8 * 1024 * 1024
    } else if file_size >= 1024 * 1024 * 1024 {
        4 * 1024 * 1024
    } else if file_size >= 128 * 1024 * 1024 {
        2 * 1024 * 1024
    } else if file_size >= 16 * 1024 * 1024 {
        1024 * 1024
    } else if file_size >= 1024 * 1024 {
        512 * 1024
    } else if file_size > 0 {
        256 * 1024
    } else {
        1024 * 1024
    };
    requested.max(adaptive).clamp(64 * 1024, 8 * 1024 * 1024)
}

fn is_file_fully_checkpointed(size_bytes: u64, chunk_size: u64, completed_chunks: &[u32]) -> bool {
    completed_chunks.len() >= chunk_count_for_size(size_bytes, chunk_size) as usize
}

fn safe_local_join(root: &Path, relative_path: &str) -> Result<PathBuf, String> {
    let mut output = root.to_path_buf();
    for segment in relative_path.replace('\\', "/").split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            continue;
        }
        let clean = segment
            .chars()
            .filter(|character| !character.is_control() && *character != '\0')
            .collect::<String>();
        if !clean.is_empty() {
            output.push(clean);
        }
    }
    if output == root {
        return Err("relative path did not contain a safe file name".to_string());
    }
    Ok(output)
}

fn sanitize_agent_relative_path(relative_path: &Path) -> String {
    relative_path
        .to_string_lossy()
        .replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != "." && *segment != "..")
        .map(|segment| {
            segment
                .chars()
                .filter(|character| !character.is_control() && *character != '\0')
                .collect::<String>()
        })
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn json_u64_field(payload: &str, key: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| value.get(key).and_then(serde_json::Value::as_u64))
}

fn json_string_field(payload: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

fn ensure_chunk_response_status(payload: &str, accepted_statuses: &[&str]) -> Result<(), String> {
    let status = json_string_field(payload, "status")
        .ok_or_else(|| format!("agent chunk response did not include status: {payload}"))?;
    if accepted_statuses.iter().any(|accepted| *accepted == status) {
        Ok(())
    } else {
        Err(format!(
            "agent chunk response status was {status}: {payload}"
        ))
    }
}

const WIFI_CHUNK_PUSH_MAX_ATTEMPTS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WifiChunkPushOutcome {
    Pushed,
    RetriedAfterChunkStatus,
    RecoveredByChunkStatus,
    RecoveredByStat,
}

impl WifiChunkPushOutcome {
    fn event_name(self) -> &'static str {
        match self {
            Self::Pushed => "wifi-chunk-pushed",
            Self::RetriedAfterChunkStatus => "wifi-chunk-retried",
            Self::RecoveredByChunkStatus | Self::RecoveredByStat => "wifi-chunk-recovered",
        }
    }

    fn message(self, chunk_index: u32) -> String {
        match self {
            Self::Pushed => format!("Wi-Fi chunk {chunk_index} pushed"),
            Self::RetriedAfterChunkStatus => {
                format!(
                    "Wi-Fi chunk {chunk_index} retried after CHUNK_STATUS reported not_committed"
                )
            }
            Self::RecoveredByChunkStatus => {
                format!("Wi-Fi chunk {chunk_index} confirmed by CHUNK_STATUS after push error")
            }
            Self::RecoveredByStat => {
                format!("Wi-Fi chunk {chunk_index} confirmed by STAT_FILE after push error")
            }
        }
    }
}

fn push_wifi_chunk_with_recovery(
    host: &str,
    relative_path: &str,
    chunk_index: u32,
    offset: u64,
    payload: &[u8],
) -> Result<WifiChunkPushOutcome, String> {
    let mut retry_after_not_committed = false;
    let mut first_push_error: Option<String> = None;

    for attempt in 0..WIFI_CHUNK_PUSH_MAX_ATTEMPTS {
        match push_wifi_chunk_once(host, relative_path, chunk_index, offset, payload) {
            Ok(()) => {
                return Ok(if retry_after_not_committed {
                    WifiChunkPushOutcome::RetriedAfterChunkStatus
                } else {
                    WifiChunkPushOutcome::Pushed
                });
            }
            Err(push_err) => {
                if first_push_error.is_none() {
                    first_push_error = Some(push_err.to_string());
                }

                if let Ok(status_reply) = agent::chunk_status(
                    host,
                    relative_path,
                    chunk_index,
                    offset,
                    payload.len() as u64,
                ) {
                    match parse_chunk_status_state(
                        &status_reply.payload,
                        relative_path,
                        chunk_index,
                    ) {
                        ChunkStatusState::Committed => {
                            return Ok(WifiChunkPushOutcome::RecoveredByChunkStatus);
                        }
                        ChunkStatusState::NotCommitted => {
                            if attempt + 1 < WIFI_CHUNK_PUSH_MAX_ATTEMPTS {
                                retry_after_not_committed = true;
                                continue;
                            }
                        }
                        ChunkStatusState::PathMismatch => {
                            return Err(format!(
                                "agent reported path_mismatch for chunk {chunk_index} on {relative_path}"
                            ));
                        }
                        ChunkStatusState::Unknown => {}
                    }
                }

                let expected_size = offset + payload.len() as u64;
                let stat = agent::stat_file(host, relative_path).map_err(|_| {
                    first_push_error
                        .clone()
                        .unwrap_or_else(|| push_err.to_string())
                })?;
                let size_bytes = json_u64_field(&stat.payload, "size_bytes").ok_or_else(|| {
                    first_push_error
                        .clone()
                        .unwrap_or_else(|| push_err.to_string())
                })?;
                if remote_size_confirms_chunk_commit(size_bytes, expected_size) {
                    return Ok(WifiChunkPushOutcome::RecoveredByStat);
                }

                if attempt + 1 >= WIFI_CHUNK_PUSH_MAX_ATTEMPTS {
                    return Err(first_push_error.unwrap_or_else(|| push_err.to_string()));
                }
            }
        }
    }

    Err("wifi chunk push exhausted retry attempts".to_string())
}

fn push_wifi_chunk_once(
    host: &str,
    relative_path: &str,
    chunk_index: u32,
    offset: u64,
    payload: &[u8],
) -> Result<(), String> {
    match agent::push_chunk_binary(host, relative_path, chunk_index, offset, payload) {
        Ok(reply) => {
            match ensure_chunk_response_status(&reply.payload, &["written", "already_committed"]) {
                Ok(()) => Ok(()),
                Err(binary_status_error) => {
                    let fallback = agent::push_chunk_payload(
                    host,
                    relative_path,
                    chunk_index,
                    offset,
                    payload,
                )
                .map_err(|err| {
                    format!("binary push failed status and base64 fallback failed: {binary_status_error}; {err}")
                })?;
                    ensure_chunk_response_status(
                        &fallback.payload,
                        &["written", "already_committed"],
                    )
                }
            }
        }
        Err(binary_error) => {
            let fallback =
                agent::push_chunk_payload(host, relative_path, chunk_index, offset, payload)
                    .map_err(|err| {
                        format!(
                            "binary push failed and base64 fallback failed: {binary_error}; {err}"
                        )
                    })?;
            ensure_chunk_response_status(&fallback.payload, &["written", "already_committed"])
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkStatusState {
    Committed,
    NotCommitted,
    PathMismatch,
    Unknown,
}

fn parse_chunk_status_state(
    payload: &str,
    relative_path: &str,
    chunk_index: u32,
) -> ChunkStatusState {
    let status = json_string_field(payload, "status");
    let payload_path = json_string_field(payload, "relative_path");
    let payload_chunk = json_u64_field(payload, "chunk_index");
    if payload_path.as_deref() != Some(relative_path) || payload_chunk != Some(chunk_index as u64) {
        return ChunkStatusState::Unknown;
    }
    match status.as_deref() {
        Some("committed") | Some("committed_on_disk") => ChunkStatusState::Committed,
        Some("not_committed") => ChunkStatusState::NotCommitted,
        Some("path_mismatch") => ChunkStatusState::PathMismatch,
        _ => ChunkStatusState::Unknown,
    }
}

fn remote_size_confirms_chunk_commit(remote_size: u64, expected_size: u64) -> bool {
    remote_size >= expected_size
}

fn persist_agent_log_batch(host: &str, payload: &str) -> Result<(), String> {
    let root = nekotrans_state_root().join("logs");
    fs::create_dir_all(&root).map_err(|err| err.to_string())?;
    let path = root.join(format!("android-{}.jsonl", sanitize_task_id(host)));
    let value =
        serde_json::from_str::<serde_json::Value>(payload).map_err(|err| err.to_string())?;
    let Some(records) = value.get("records").and_then(serde_json::Value::as_array) else {
        return Ok(());
    };
    let fetched_at_epoch_ms = epoch_ms() as u64;
    let mut existing = if path.exists() {
        fs::read_to_string(&path)
            .map_err(|err| err.to_string())?
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .map(|value| agent_log_dedupe_key(&value))
                    .unwrap_or_else(|_| line.to_string())
            })
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| err.to_string())?;
    for record in records {
        let value = augment_agent_log_record(record, host, fetched_at_epoch_ms)?;
        let dedupe_key = agent_log_dedupe_key(&value);
        let line = value.to_string();
        if existing.insert(dedupe_key) {
            writeln!(file, "{line}").map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

fn augment_agent_log_record(
    record: &serde_json::Value,
    host: &str,
    fetched_at_epoch_ms: u64,
) -> Result<serde_json::Value, String> {
    let mut object = record
        .as_object()
        .cloned()
        .ok_or_else(|| "agent log record was not a JSON object".to_string())?;
    object.insert(
        "device_host".to_string(),
        serde_json::Value::String(host.to_string()),
    );
    object.insert(
        "fetched_at_epoch_ms".to_string(),
        serde_json::Value::Number(serde_json::Number::from(fetched_at_epoch_ms)),
    );
    if !object.contains_key("ts_epoch_ms") {
        object.insert(
            "ts_epoch_ms".to_string(),
            serde_json::Value::Number(serde_json::Number::from(fetched_at_epoch_ms)),
        );
    }
    Ok(serde_json::Value::Object(object))
}

fn agent_log_dedupe_key(record: &serde_json::Value) -> String {
    let mut normalized = record.clone();
    if let Some(object) = normalized.as_object_mut() {
        object.remove("fetched_at_epoch_ms");
    }
    normalized.to_string()
}

fn read_persisted_agent_logs(
    wanted_device_host: Option<&str>,
    from_epoch_ms: Option<u64>,
    to_epoch_ms: Option<u64>,
    wanted_level: Option<&str>,
    wanted_text: Option<&str>,
) -> Result<Vec<String>, String> {
    let root = nekotrans_state_root().join("logs");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut lines = Vec::new();
    for entry in fs::read_dir(root).map_err(|err| err.to_string())? {
        let path = entry.map_err(|err| err.to_string())?.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !file_name.starts_with("android-")
            || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
        {
            continue;
        }
        for line in fs::read_to_string(path)
            .map_err(|err| err.to_string())?
            .lines()
        {
            if !log_line_matches_filters(
                line,
                None,
                wanted_device_host,
                from_epoch_ms,
                to_epoch_ms,
                wanted_level,
                wanted_text,
            ) {
                continue;
            }
            lines.push(line.to_string());
        }
    }
    Ok(lines)
}

fn log_line_matches_filters(
    line: &str,
    wanted_task_id: Option<&str>,
    wanted_device_host: Option<&str>,
    from_epoch_ms: Option<u64>,
    to_epoch_ms: Option<u64>,
    wanted_level: Option<&str>,
    wanted_text: Option<&str>,
) -> bool {
    let parsed = serde_json::from_str::<serde_json::Value>(line).ok();

    if let Some(task_id) = wanted_task_id {
        let Some(actual_task_id) = parsed
            .as_ref()
            .and_then(|value| value.get("task_id"))
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        if actual_task_id != task_id {
            return false;
        }
    }

    if let Some(device_host) = wanted_device_host {
        let Some(actual_device_host) = parsed
            .as_ref()
            .and_then(|value| value.get("device_host"))
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        if actual_device_host != device_host {
            return false;
        }
    }

    if from_epoch_ms.is_some() || to_epoch_ms.is_some() {
        let Some(ts_epoch_ms) = parsed
            .as_ref()
            .and_then(|value| value.get("ts_epoch_ms"))
            .and_then(serde_json::Value::as_u64)
        else {
            return false;
        };
        if let Some(from_epoch_ms) = from_epoch_ms {
            if ts_epoch_ms < from_epoch_ms {
                return false;
            }
        }
        if let Some(to_epoch_ms) = to_epoch_ms {
            if ts_epoch_ms > to_epoch_ms {
                return false;
            }
        }
    }

    if let Some(level) = wanted_level {
        if !line.contains(level) {
            return false;
        }
    }
    if let Some(text) = wanted_text {
        if !line.contains(text) {
            return false;
        }
    }

    true
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = [0u8; 4];
    let mut buffer_len = 0usize;

    for byte in input.bytes().filter(|value| !value.is_ascii_whitespace()) {
        buffer[buffer_len] = decode_base64_byte(byte)?;
        buffer_len += 1;
        if buffer_len == 4 {
            output.push((buffer[0] << 2) | (buffer[1] >> 4));
            if buffer[2] != 64 {
                output.push((buffer[1] << 4) | (buffer[2] >> 2));
            }
            if buffer[3] != 64 {
                output.push((buffer[2] << 6) | buffer[3]);
            }
            buffer_len = 0;
        }
    }

    if buffer_len != 0 {
        return Err("invalid base64 length".to_string());
    }
    Ok(output)
}

fn decode_base64_byte(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        b'=' => Ok(64),
        other => Err(format!("invalid base64 byte {other}")),
    }
}

fn blake3_digest_file(path: &Path) -> Result<String, String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = fs::File::open(path).map_err(|err| err.to_string())?;
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes_since_yield = 0usize;
    loop {
        let read = file.read(&mut buffer).map_err(|err| err.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes_since_yield += read;
        if bytes_since_yield >= 8 * 1024 * 1024 {
            bytes_since_yield = 0;
            thread::yield_now();
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn blake3_digest_file_range(path: &Path, offset: u64, length: u64) -> Result<String, String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = fs::File::open(path).map_err(|err| err.to_string())?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|err| err.to_string())?;
    let mut remaining = length;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let wanted = remaining.min(buffer.len() as u64) as usize;
        let read = file
            .read(&mut buffer[..wanted])
            .map_err(|err| err.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
        thread::yield_now();
    }
    if remaining == 0 {
        Ok(hasher.finalize().to_hex().to_string())
    } else {
        Err(format!(
            "short local read while hashing range: offset={offset} length={length}"
        ))
    }
}

fn ensure_verify_algorithm(payload: &str, expected_algorithm: &str) -> Result<(), String> {
    let actual_algorithm = json_string_field(payload, "algorithm")
        .ok_or_else(|| format!("agent verify did not include algorithm: {payload}"))?;
    if actual_algorithm.eq_ignore_ascii_case(expected_algorithm) {
        Ok(())
    } else {
        Err(format!(
            "agent verify algorithm mismatch: expected {expected_algorithm}, got {actual_algorithm}"
        ))
    }
}

fn validate_task_draft_inputs(
    draft: &TaskCreateDraft,
    direction: Direction,
    transport_mode: TransportMode,
) -> Result<(), String> {
    let has_serial = draft
        .device_serial
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let has_agent_host = draft
        .agent_host
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);

    match transport_mode {
        TransportMode::AdbOnly => {
            if !has_serial {
                return Err("ADB-only tasks require an ADB serial".to_string());
            }
        }
        TransportMode::WifiOnly => {
            if !has_agent_host {
                return Err("Wi-Fi-only tasks require an agent host".to_string());
            }
        }
        TransportMode::Dual => {
            if !has_serial && !has_agent_host {
                return Err(
                    "Dual Track tasks currently require at least one usable lane: ADB serial or agent host"
                        .to_string(),
                );
            }
        }
    }

    if direction == Direction::AndroidToPc {
        let target_root = PathBuf::from(draft.target_root.trim());
        if target_root.as_os_str().is_empty() {
            return Err("Android -> PC tasks require a local target folder".to_string());
        }
    }

    if direction == Direction::PcToAndroid && draft.target_root.contains('\\') {
        return Err(
            "PC -> Android target root should be an Android-style path like /sdcard/Nekotrans"
                .to_string(),
        );
    }

    Ok(())
}

fn same_file_dual_task_chunk_size(
    source_path: &Path,
    requested_chunk_size: u64,
) -> Result<Option<u64>, String> {
    let manifest = collect_local_transfer_manifest(source_path)?;
    Ok(manifest
        .files
        .into_iter()
        .filter(|file| is_large_file(file.size_bytes, 32 * 1024 * 1024))
        .map(|file| dual_same_file_chunk_size(file.size_bytes, requested_chunk_size))
        .max())
}

#[tauri::command]
fn list_task_logs_filtered(
    task_id: Option<String>,
    device_host: Option<String>,
    from_epoch_ms: Option<u64>,
    to_epoch_ms: Option<u64>,
    level: Option<String>,
    text: Option<String>,
    engine: State<'_, DesktopEngine>,
) -> Result<Vec<String>, String> {
    let engine = engine.0.lock().map_err(|err| err.to_string())?;
    let wanted_task = task_id.filter(|value| !value.trim().is_empty());
    let wanted_device_host = device_host.filter(|value| !value.trim().is_empty());
    let wanted_level = level
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("\"level\":\"{}\"", value.trim().to_ascii_lowercase()));
    let wanted_text = text.filter(|value| !value.trim().is_empty());
    let snapshots = engine.snapshots();
    let mut lines = Vec::new();

    for snapshot in snapshots {
        for line in engine
            .log_lines(&snapshot.task_id)
            .map_err(|err| err.to_string())?
        {
            if !log_line_matches_filters(
                &line,
                wanted_task.as_deref(),
                wanted_device_host.as_deref(),
                from_epoch_ms,
                to_epoch_ms,
                wanted_level.as_deref(),
                wanted_text.as_deref(),
            ) {
                continue;
            }
            lines.push(line);
        }
    }
    lines.extend(read_persisted_agent_logs(
        wanted_device_host.as_deref(),
        from_epoch_ms,
        to_epoch_ms,
        wanted_level.as_deref(),
        wanted_text.as_deref(),
    )?);

    Ok(lines)
}

#[tauri::command]
fn export_logs(
    task_id: Option<String>,
    device_host: Option<String>,
    from_epoch_ms: Option<u64>,
    to_epoch_ms: Option<u64>,
    level: Option<String>,
    text: Option<String>,
    engine: State<'_, DesktopEngine>,
) -> Result<String, String> {
    let lines = list_task_logs_filtered(
        task_id,
        device_host,
        from_epoch_ms,
        to_epoch_ms,
        level,
        text,
        engine,
    )?;
    let root = nekotrans_state_root().join("logs");
    fs::create_dir_all(&root).map_err(|err| err.to_string())?;
    let path = root.join(format!("export-{}.jsonl", epoch_ms()));
    let mut file = fs::File::create(&path).map_err(|err| err.to_string())?;
    for line in lines {
        writeln!(file, "{line}").map_err(|err| err.to_string())?;
    }
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
fn scan_wifi_agents() -> Result<Vec<AgentProbeCard>, String> {
    let mut probes = Vec::new();
    for device in adb::probe_adb_devices().map_err(|err| err.to_string())? {
        if let Some(host) = device.wifi_agent_ip {
            if let Ok(card) = probe_wifi_agent(host) {
                probes.push(card);
            }
        }
    }
    Ok(probes)
}

#[tauri::command]
fn pick_source_path(app: tauri::AppHandle, pick_directory: bool) -> Result<Option<String>, String> {
    let dialog = app.dialog().file();
    let selected = if pick_directory {
        dialog.blocking_pick_folder()
    } else {
        dialog.blocking_pick_file()
    };
    selected.map(dialog_path_to_string).transpose()
}

#[tauri::command]
fn pick_target_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    app.dialog()
        .file()
        .blocking_pick_folder()
        .map(dialog_path_to_string)
        .transpose()
}

fn dialog_path_to_string(path: FilePath) -> Result<String, String> {
    path.into_path()
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|err| err.to_string())
}

#[derive(Clone, serde::Serialize)]
struct DashboardState {
    app_name: String,
    transport_modes: Vec<String>,
    devices: Vec<DeviceCard>,
    tasks: Vec<TaskCard>,
    recoverable_tasks: Vec<String>,
    sample_logs: Vec<String>,
}

#[derive(Clone, serde::Serialize)]
struct DeviceCard {
    id: String,
    label: String,
    lane_mode: String,
    agent_host: Option<String>,
    adb_ready: bool,
    wifi_ready: bool,
    transfer_ready: bool,
    protocol_version: String,
    status_text: String,
    platform_text: String,
    preflight_checks: Vec<DeviceCheckView>,
}

#[derive(Clone, serde::Serialize)]
struct DeviceCheckView {
    label: String,
    passed: bool,
    detail: String,
}

#[derive(Clone, serde::Serialize)]
struct TaskDraft {
    task_id: String,
    direction: String,
    transport_mode: String,
    verify_enabled: bool,
    chunk_size_mb: u32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct TaskCreateDraft {
    source_path: String,
    target_root: String,
    direction: String,
    transport_mode: String,
    verify_enabled: bool,
    source_size_bytes: Option<u64>,
    chunk_size_bytes: Option<u64>,
    max_in_flight_chunks_per_lane: Option<usize>,
    device_serial: Option<String>,
    agent_host: Option<String>,
    target_path_policy: Option<String>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct TaskFileRecord {
    task_id: String,
    source_path: String,
    target_root: String,
    direction: String,
    transport_mode: String,
    verify_enabled: bool,
    source_size_bytes: Option<u64>,
    chunk_size_bytes: Option<u64>,
    max_in_flight_chunks_per_lane: Option<usize>,
    device_serial: Option<String>,
    agent_host: Option<String>,
    target_path_policy: Option<String>,
    created_at_epoch_ms: u128,
}

impl TaskFileRecord {
    fn from_draft(draft: &TaskCreateDraft, task_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            source_path: draft.source_path.clone(),
            target_root: draft.target_root.clone(),
            direction: draft.direction.clone(),
            transport_mode: draft.transport_mode.clone(),
            verify_enabled: draft.verify_enabled,
            source_size_bytes: draft.source_size_bytes,
            chunk_size_bytes: draft.chunk_size_bytes,
            max_in_flight_chunks_per_lane: draft.max_in_flight_chunks_per_lane,
            device_serial: draft
                .device_serial
                .clone()
                .filter(|value| !value.trim().is_empty()),
            agent_host: draft
                .agent_host
                .clone()
                .filter(|value| !value.trim().is_empty()),
            target_path_policy: draft.target_path_policy.clone(),
            created_at_epoch_ms: epoch_ms(),
        }
    }

    fn persist(&self) -> Result<(), String> {
        let root = nekotrans_state_root().join("tasks");
        fs::create_dir_all(&root).map_err(|err| err.to_string())?;
        let path = root.join(format!("{}.json", self.task_id));
        let payload = serde_json::to_string_pretty(self).map_err(|err| err.to_string())?;
        fs::write(path, payload).map_err(|err| err.to_string())
    }

    fn load(task_id: &str) -> Result<Self, String> {
        let path = nekotrans_state_root()
            .join("tasks")
            .join(format!("{task_id}.json"));
        let payload = fs::read_to_string(path).map_err(|err| err.to_string())?;
        serde_json::from_str(&payload).map_err(|err| err.to_string())
    }

    fn delete(task_id: &str) -> Result<(), String> {
        let path = nekotrans_state_root()
            .join("tasks")
            .join(format!("{task_id}.json"));
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    }

    fn list_ids() -> Result<Vec<String>, String> {
        let root = nekotrans_state_root().join("tasks");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in fs::read_dir(root).map_err(|err| err.to_string())? {
            let path = entry.map_err(|err| err.to_string())?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Some(task_id) = path.file_stem().and_then(|value| value.to_str()) {
                ids.push(task_id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }
}

#[derive(Clone, serde::Serialize)]
struct TaskCard {
    task_id: String,
    state: String,
    direction: String,
    transport_mode: String,
    verify_enabled: bool,
    total_files: usize,
    total_bytes: u64,
    committed_bytes: u64,
    progress_percent: u32,
    adb_bytes: u64,
    wifi_bytes: u64,
    completed_chunks: u64,
    last_error: Option<String>,
}

#[derive(Clone, serde::Serialize)]
struct AgentProbeCard {
    host: String,
    port: u16,
    hello_payload: String,
    ping_payload: String,
    snapshot_payload: String,
}

#[derive(Clone, serde::Serialize)]
struct AgentTaskProbeCard {
    host: String,
    port: u16,
    task_id: String,
    start_payload: String,
    snapshot_payload: String,
}

#[derive(Clone, serde::Serialize)]
struct AgentStateProbeCard {
    host: String,
    port: u16,
    action_payload: String,
    snapshot_payload: String,
}

#[derive(Clone, serde::Serialize)]
struct AgentFileProbeCard {
    host: String,
    port: u16,
    relative_path: String,
    action_payload: String,
    snapshot_payload: String,
}

struct DesktopEngine(Arc<Mutex<TransferEngine>>);

struct AdbTransferRegistry(Arc<Mutex<BTreeMap<String, AdbTransferEntry>>>);

struct AdbTransferEntry {
    view: AdbTransferCard,
    pause_requested: Arc<AtomicBool>,
}

#[derive(Clone)]
struct LocalTransferManifest {
    files: Vec<LocalTransferFile>,
    directories: Vec<LocalTransferDirectory>,
}

impl Default for LocalTransferManifest {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            directories: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct LocalTransferFile {
    file_index: usize,
    local_path: PathBuf,
    relative_path: PathBuf,
    size_bytes: u64,
}

#[derive(Clone)]
struct LocalTransferDirectory {
    relative_path: PathBuf,
}

#[derive(Clone, serde::Serialize)]
struct AdbTransferCard {
    task_id: String,
    serial: String,
    state: String,
    current_file: usize,
    total_files: usize,
    pushed_files: usize,
    skipped_files: usize,
    pushed_chunks: u64,
    skipped_chunks: u64,
    bytes_scanned: u64,
    bytes_pushed: u64,
    relative_path: String,
    remote_path: String,
    last_event: String,
    last_message: String,
}

impl AdbTransferCard {
    fn new(task_id: &str, serial: &str, remote_path: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            serial: serial.to_string(),
            state: "Running".to_string(),
            current_file: 0,
            total_files: 0,
            pushed_files: 0,
            skipped_files: 0,
            pushed_chunks: 0,
            skipped_chunks: 0,
            bytes_scanned: 0,
            bytes_pushed: 0,
            relative_path: String::new(),
            remote_path: remote_path.to_string(),
            last_event: "started".to_string(),
            last_message: "ADB transfer worker started".to_string(),
        }
    }
}

fn discover_device_cards() -> (Vec<DeviceCard>, Vec<String>) {
    match adb::probe_adb_devices() {
        Ok(devices) if devices.is_empty() => (
            Vec::new(),
            vec![
                LogRecord::new(
                    LogLevel::Info,
                    LogScope::Device,
                    "adb discovery completed with no attached devices",
                )
                .to_json_line(),
            ],
        ),
        Ok(devices) => (
            devices
                .into_iter()
                .map(DeviceCard::from_adb_probe)
                .collect(),
            vec![
                LogRecord::new(LogLevel::Info, LogScope::Device, "adb discovery completed")
                    .to_json_line(),
            ],
        ),
        Err(err) => (
            Vec::new(),
            vec![
                LogRecord::new(
                    LogLevel::Warn,
                    LogScope::Device,
                    format!("adb discovery failed: {err}"),
                )
                .to_json_line(),
            ],
        ),
    }
}

impl DeviceCard {
    fn from_adb_probe(device: adb::ProbedAdbDevice) -> Self {
        let label = device
            .discovered
            .model
            .clone()
            .or(device.discovered.device_name.clone())
            .unwrap_or_else(|| device.discovered.serial.clone());
        let adb_ready = device.shell_ready;
        let wifi_ready = device.wifi_agent_capability.is_some()
            || device.discovered.serial.contains(':')
            || device
                .adb_tcp_port
                .as_deref()
                .map(|value| value != "-1" && value != "0")
                .unwrap_or(false);
        let lane_mode = if let Some(ip) = &device.wifi_agent_ip {
            if device.wifi_agent_capability.is_some() {
                format!("{} + Agent {ip}:38997", device.discovered.transport_hint)
            } else {
                format!("{} + LAN {ip}", device.discovered.transport_hint)
            }
        } else if device.discovered.serial.contains(':') {
            "ADB (TCP)".to_string()
        } else if wifi_ready {
            format!("{} + TCP Candidate", device.discovered.transport_hint)
        } else {
            device.discovered.transport_hint.clone()
        };
        let status_text = if let Some(error) = &device.probe_error {
            error.clone()
        } else if adb_ready {
            "adb shell ready".to_string()
        } else {
            format!("device status is {}", device.discovered.status)
        };
        let android_release = device.android_release.unwrap_or_else(|| "?".to_string());
        let sdk_level = device.sdk_level.unwrap_or_else(|| "?".to_string());
        let manufacturer = device
            .manufacturer
            .unwrap_or_else(|| "Unknown OEM".to_string());
        let platform_text = format!("{manufacturer} / Android {android_release} / SDK {sdk_level}");

        Self {
            id: device.discovered.serial,
            label,
            lane_mode,
            agent_host: device.wifi_agent_ip,
            adb_ready,
            wifi_ready,
            transfer_ready: adb_ready,
            protocol_version: if device.wifi_agent_capability.is_some() {
                "agent-capability".to_string()
            } else {
                "adb-preflight".to_string()
            },
            status_text,
            platform_text,
            preflight_checks: device
                .preflight_checks
                .into_iter()
                .map(|check| DeviceCheckView {
                    label: check.label.to_string(),
                    passed: check.passed,
                    detail: check.detail,
                })
                .collect(),
        }
    }
}

impl From<EngineTaskSnapshot> for TaskCard {
    fn from(snapshot: EngineTaskSnapshot) -> Self {
        let progress_percent = if snapshot.total_bytes == 0 {
            100
        } else {
            ((snapshot.committed_bytes.saturating_mul(100)) / snapshot.total_bytes) as u32
        };

        Self {
            task_id: snapshot.task_id,
            state: display_state(snapshot.state),
            direction: display_direction(snapshot.direction),
            transport_mode: display_transport_mode(snapshot.transport_mode),
            verify_enabled: snapshot.verify_enabled,
            total_files: snapshot.total_files,
            total_bytes: snapshot.total_bytes,
            committed_bytes: snapshot.committed_bytes,
            progress_percent,
            adb_bytes: snapshot.adb_bytes,
            wifi_bytes: snapshot.wifi_bytes,
            completed_chunks: snapshot.completed_chunks,
            last_error: snapshot.last_error,
        }
    }
}

fn display_state(state: TaskState) -> String {
    match state {
        TaskState::Pending => "Pending",
        TaskState::Running => "Running",
        TaskState::Paused => "Paused",
        TaskState::Completed => "Completed",
        TaskState::Failed => "Failed",
        TaskState::Cancelled => "Cancelled",
    }
    .to_string()
}

fn display_direction(direction: transfer_core::Direction) -> String {
    match direction {
        transfer_core::Direction::PcToAndroid => "PC -> Android",
        transfer_core::Direction::AndroidToPc => "Android -> PC",
    }
    .to_string()
}

fn display_transport_mode(mode: transfer_core::TransportMode) -> String {
    match mode {
        transfer_core::TransportMode::AdbOnly => "ADB-only",
        transfer_core::TransportMode::WifiOnly => "Wi-Fi-only",
        transfer_core::TransportMode::Dual => "Dual Track",
    }
    .to_string()
}

pub fn run() {
    let checkpoint_root = nekotrans_state_root().join("checkpoints");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(DesktopEngine(Arc::new(Mutex::new(TransferEngine::new(
            checkpoint_root,
        )))))
        .manage(AdbTransferRegistry(Arc::new(Mutex::new(BTreeMap::new()))))
        .invoke_handler(tauri::generate_handler![
            bootstrap_dashboard,
            sample_task_template,
            tick_demo_task,
            pause_demo_task,
            resume_demo_task,
            list_task_logs,
            list_task_logs_filtered,
            export_logs,
            list_tasks,
            create_demo_local_task,
            create_task_from_draft,
            create_transfer_task,
            start_transfer_task,
            pause_transfer_task,
            resume_transfer_task,
            cancel_transfer_task,
            retry_transfer_task,
            recover_task,
            delete_transfer_task,
            install_agent,
            start_adb_docs_push,
            pause_adb_transfer,
            resume_adb_transfer,
            list_adb_transfers,
            probe_wifi_agent,
            start_wifi_agent_task,
            pause_wifi_agent_task,
            resume_wifi_agent_task,
            start_wifi_agent_file,
            ack_wifi_agent_chunk,
            push_wifi_agent_sample_chunk,
            verify_wifi_agent_file,
            pull_wifi_agent_chunk,
            fetch_wifi_agent_logs,
            scan_wifi_agents,
            pick_source_path,
            pick_target_folder
        ])
        .setup(|app| {
            let window = app.get_webview_window("main").expect("main window");
            let _ = window.set_title("Nekotrans");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run desktop shell");
}

#[cfg(test)]
mod tests {
    use super::{
        ChunkStatusState, LocalTransferFile, base64_decode, parse_chunk_status_state,
        partition_dual_transfer_files, remote_size_confirms_chunk_commit, safe_local_join,
        sanitize_agent_relative_path, small_file_bundle_can_accept, small_file_bundle_max_bytes,
        small_file_bundle_max_files,
    };
    use crate::{
        collect_local_transfer_manifest, dual_same_file_chunk_size, dual_wifi_chunk_size,
        is_small_file_bundle_candidate, new_small_file_bundle,
    };
    use std::fs;
    use std::path::{Path, PathBuf};

    fn local_file(index: usize, name: &str, size_bytes: u64) -> LocalTransferFile {
        LocalTransferFile {
            file_index: index,
            local_path: PathBuf::from(name),
            relative_path: PathBuf::from(name),
            size_bytes,
        }
    }

    #[test]
    fn decodes_base64_payloads() {
        assert_eq!(base64_decode("").expect("empty"), b"");
        assert_eq!(base64_decode("Zg==").expect("f"), b"f");
        assert_eq!(base64_decode("Zm8=").expect("fo"), b"fo");
        assert_eq!(base64_decode("Zm9v").expect("foo"), b"foo");
        assert_eq!(base64_decode("aGVsbG8=").expect("hello"), b"hello");
    }

    #[test]
    fn safe_local_join_removes_parent_segments() {
        let root = Path::new("C:/target");
        let joined = safe_local_join(root, "../bad/./file.txt").expect("safe path");
        assert_eq!(joined, Path::new("C:/target/bad/file.txt"));
    }

    #[test]
    fn safe_path_helpers_preserve_real_fixture_names() {
        let relative = Path::new(".minecraft/versions/L_Ender's Cataclysm 1.21.1-3.16.jar");
        assert_eq!(
            sanitize_agent_relative_path(relative),
            ".minecraft/versions/L_Ender's Cataclysm 1.21.1-3.16.jar"
        );

        let root = Path::new("C:/target");
        let joined = safe_local_join(root, ".minecraft/versions/坏 name's.jar")
            .expect("safe join should preserve normal unicode names");
        assert_eq!(
            joined,
            Path::new("C:/target/.minecraft/versions/坏 name's.jar")
        );
    }

    #[test]
    fn local_manifest_preserves_empty_directories() {
        let root =
            std::env::temp_dir().join(format!("nekotrans-empty-dir-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("empty").join("nested")).expect("create empty dir");
        fs::create_dir_all(root.join("with-file")).expect("create file dir");
        fs::write(root.join("with-file").join("a.txt"), b"hello").expect("write file");

        let manifest = collect_local_transfer_manifest(&root).expect("collect manifest");
        let directories = manifest
            .directories
            .iter()
            .map(|dir| dir.relative_path.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>();
        let files = manifest
            .files
            .iter()
            .map(|file| file.relative_path.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>();

        assert!(directories.contains(&"empty".to_string()));
        assert!(directories.contains(&"empty/nested".to_string()));
        assert!(files.contains(&"with-file/a.txt".to_string()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_size_confirms_committed_chunk_at_expected_boundary() {
        assert!(remote_size_confirms_chunk_commit(1024, 1024));
        assert!(remote_size_confirms_chunk_commit(2048, 1024));
    }

    #[test]
    fn remote_size_does_not_confirm_short_chunk_commit() {
        assert!(!remote_size_confirms_chunk_commit(1023, 1024));
    }

    #[test]
    fn parses_committed_chunk_status() {
        let payload = r#"{"type":"ChunkAck","relative_path":"docs/file.bin","chunk_index":3,"status":"committed"}"#;
        assert_eq!(
            parse_chunk_status_state(payload, "docs/file.bin", 3),
            ChunkStatusState::Committed
        );
    }

    #[test]
    fn parses_not_committed_chunk_status() {
        let payload = r#"{"type":"ChunkAck","relative_path":"docs/file.bin","chunk_index":3,"status":"not_committed"}"#;
        assert_eq!(
            parse_chunk_status_state(payload, "docs/file.bin", 3),
            ChunkStatusState::NotCommitted
        );
    }

    #[test]
    fn parses_committed_on_disk_chunk_status() {
        let payload = r#"{"type":"ChunkAck","relative_path":"docs/file.bin","chunk_index":3,"status":"committed_on_disk"}"#;
        assert_eq!(
            parse_chunk_status_state(payload, "docs/file.bin", 3),
            ChunkStatusState::Committed
        );
    }

    #[test]
    fn parses_path_mismatch_chunk_status() {
        let payload = r#"{"type":"ChunkAck","relative_path":"docs/file.bin","chunk_index":3,"status":"path_mismatch"}"#;
        assert_eq!(
            parse_chunk_status_state(payload, "docs/file.bin", 3),
            ChunkStatusState::PathMismatch
        );
    }

    #[test]
    fn dual_partition_routes_large_files_to_same_file_dual_lane() {
        let (adb_files, wifi_files, same_file_dual_files) = partition_dual_transfer_files(vec![
            local_file(0, "movie.bin", 33 * 1024 * 1024),
            local_file(1, "note.txt", 128),
        ]);

        assert!(adb_files.is_empty());
        assert_eq!(wifi_files.len(), 1);
        assert_eq!(wifi_files[0].relative_path, PathBuf::from("note.txt"));
        assert_eq!(same_file_dual_files.len(), 1);
        assert_eq!(
            same_file_dual_files[0].relative_path,
            PathBuf::from("movie.bin")
        );
    }

    #[test]
    fn dual_partition_alternates_small_files_between_lanes() {
        let (adb_files, wifi_files, same_file_dual_files) = partition_dual_transfer_files(vec![
            local_file(0, "a.txt", 100),
            local_file(1, "b.txt", 100),
            local_file(2, "c.txt", 100),
            local_file(3, "d.txt", 100),
        ]);

        assert!(same_file_dual_files.is_empty());
        assert_eq!(
            wifi_files
                .iter()
                .map(|file| file.relative_path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("a.txt"), PathBuf::from("c.txt")]
        );
        assert_eq!(
            adb_files
                .iter()
                .map(|file| file.relative_path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("b.txt"), PathBuf::from("d.txt")]
        );
    }

    #[test]
    fn dual_same_file_keeps_large_real_fixture_chunk_size() {
        assert_eq!(dual_wifi_chunk_size(8 * 1024 * 1024), 256 * 1024);
        assert_eq!(
            dual_same_file_chunk_size(64 * 1024 * 1024, 8 * 1024 * 1024),
            8 * 1024 * 1024
        );
        assert_eq!(
            dual_same_file_chunk_size(1024 * 1024 * 1024, 8 * 1024 * 1024),
            32 * 1024 * 1024
        );
        assert_eq!(
            dual_same_file_chunk_size(8 * 1024 * 1024 * 1024, 8 * 1024 * 1024),
            64 * 1024 * 1024
        );
    }

    #[test]
    fn small_file_bundle_flush_thresholds_are_decision_complete() {
        let bundle = new_small_file_bundle("task", 0);
        assert!(is_small_file_bundle_candidate(&local_file(
            0, "tiny.txt", 1
        )));
        assert!(is_small_file_bundle_candidate(&local_file(
            1,
            "empty.txt",
            0
        )));
        assert!(!is_small_file_bundle_candidate(&local_file(
            2,
            "large.bin",
            small_file_bundle_max_bytes() + 1
        )));
        assert!(small_file_bundle_can_accept(
            &bundle,
            &local_file(3, "final.txt", 1)
        ));

        let mut full_by_count = new_small_file_bundle("task", 1);
        for index in 0..small_file_bundle_max_files() {
            full_by_count.entries.push(super::SmallFileBundleEntry {
                file: local_file(index, "x.txt", 1),
                relative_path: format!("x-{index}.txt"),
                size_bytes: 1,
            });
        }
        assert!(!small_file_bundle_can_accept(
            &full_by_count,
            &local_file(4, "overflow.txt", 1)
        ));

        let mut nearly_full = new_small_file_bundle("task", 2);
        nearly_full
            .payload
            .resize(small_file_bundle_max_bytes() as usize, 0);
        nearly_full.entries.push(super::SmallFileBundleEntry {
            file: local_file(5, "filled.txt", 1),
            relative_path: "filled.txt".to_string(),
            size_bytes: 1,
        });
        assert!(!small_file_bundle_can_accept(
            &nearly_full,
            &local_file(6, "overflow.bin", 1)
        ));
    }

    #[test]
    fn same_file_wifi_stride_uses_adb_heavy_distribution() {
        assert_eq!(super::same_file_wifi_stride(8 * 1024 * 1024 * 1024), 4);
        assert!(super::DualSameFileLane::Wifi.owns_chunk(1, 4));
        assert!(super::DualSameFileLane::Wifi.owns_chunk(5, 4));
        assert!(super::DualSameFileLane::Adb.owns_chunk(0, 4));
        assert!(super::DualSameFileLane::Adb.owns_chunk(2, 4));
        assert!(super::DualSameFileLane::Adb.owns_chunk(3, 4));
    }

    #[test]
    fn same_file_wifi_stride_can_be_calibrated_from_lane_rates() {
        assert_eq!(super::same_file_wifi_stride_from_rates(100.0, 100.0), 2);
        assert_eq!(super::same_file_wifi_stride_from_rates(132.0, 52.0), 4);
        assert_eq!(super::same_file_wifi_stride_from_rates(300.0, 50.0), 7);
        assert_eq!(super::same_file_wifi_stride_from_rates(0.0, 50.0), 4);
    }

    #[test]
    fn same_file_calibration_sample_is_bounded() {
        assert_eq!(
            super::same_file_calibration_sample_bytes(1024 * 1024, 64 * 1024 * 1024),
            1024 * 1024
        );
        assert_eq!(
            super::same_file_calibration_sample_bytes(128 * 1024 * 1024, 4 * 1024 * 1024),
            8 * 1024 * 1024
        );
        assert_eq!(
            super::same_file_calibration_sample_bytes(1024 * 1024 * 1024, 256 * 1024 * 1024),
            64 * 1024 * 1024
        );
    }

    #[test]
    fn android_verify_samples_cover_edges_without_duplicates() {
        let ranges = super::android_verify_sample_ranges(5 * 1024 * 1024 * 1024);
        assert!(ranges.len() >= 3);
        assert_eq!(ranges.first().copied(), Some((0, 256 * 1024)));
        assert_eq!(
            ranges.last().copied(),
            Some((5 * 1024 * 1024 * 1024 - 256 * 1024, 256 * 1024))
        );

        let unique = ranges
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), ranges.len());
    }

    #[test]
    fn wifi_transfer_chunk_size_scales_with_file_size() {
        assert_eq!(
            super::wifi_transfer_chunk_size(512 * 1024, 64 * 1024),
            256 * 1024
        );
        assert_eq!(
            super::wifi_transfer_chunk_size(32 * 1024 * 1024, 512 * 1024),
            1024 * 1024
        );
        assert_eq!(
            super::wifi_transfer_chunk_size(2 * 1024 * 1024 * 1024, 1024 * 1024),
            4 * 1024 * 1024
        );
        assert_eq!(
            super::wifi_transfer_chunk_size(8 * 1024 * 1024 * 1024, 16 * 1024 * 1024),
            8 * 1024 * 1024
        );
    }
}
