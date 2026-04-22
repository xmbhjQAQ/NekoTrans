use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const AGENT_CAPABILITY_PORT: u16 = 38997;
const AGENT_CONNECT_TIMEOUT_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredAdbDevice {
    pub serial: String,
    pub status: String,
    pub model: Option<String>,
    pub product: Option<String>,
    pub device_name: Option<String>,
    pub transport_id: Option<String>,
    pub transport_hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedAdbDevice {
    pub discovered: DiscoveredAdbDevice,
    pub adb_state: Option<String>,
    pub manufacturer: Option<String>,
    pub android_release: Option<String>,
    pub sdk_level: Option<String>,
    pub cpu_abi: Option<String>,
    pub adb_tcp_port: Option<String>,
    pub agent_package_path: Option<String>,
    pub wifi_agent_ip: Option<String>,
    pub wifi_agent_capability: Option<String>,
    pub remote_sdcard_ready: bool,
    pub shell_ready: bool,
    pub probe_error: Option<String>,
    pub preflight_checks: Vec<PreflightCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightCheck {
    pub key: &'static str,
    pub label: &'static str,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCapabilityProbe {
    pub ip: Option<String>,
    pub capability_json: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct AdbTransferControl {
    pause_requested: Arc<AtomicBool>,
}

impl AdbTransferControl {
    pub fn new(pause_requested: Arc<AtomicBool>) -> Self {
        Self { pause_requested }
    }

    pub fn is_paused(&self) -> bool {
        self.pause_requested.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbPushProgress {
    pub event: String,
    pub relative_path: String,
    pub remote_path: String,
    pub file_index: usize,
    pub chunk_index: Option<u32>,
    pub chunk_length: u64,
    pub current_file: usize,
    pub total_files: usize,
    pub pushed_files: usize,
    pub skipped_files: usize,
    pub pushed_chunks: u64,
    pub skipped_chunks: u64,
    pub bytes_scanned: u64,
    pub bytes_pushed: u64,
    pub message: String,
}

#[derive(Debug)]
pub enum AdbDiscoveryError {
    CommandFailed(String),
    Paused(String),
    Utf8(String),
}

impl std::fmt::Display for AdbDiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CommandFailed(message) => write!(f, "{message}"),
            Self::Paused(message) => write!(f, "{message}"),
            Self::Utf8(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for AdbDiscoveryError {}

pub fn discover_adb_devices() -> Result<Vec<DiscoveredAdbDevice>, AdbDiscoveryError> {
    let output = Command::new("adb")
        .args(["devices", "-l"])
        .output()
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8(output.stderr)
            .map_err(|err| AdbDiscoveryError::Utf8(err.to_string()))?;
        return Err(AdbDiscoveryError::CommandFailed(stderr.trim().to_string()));
    }

    let stdout =
        String::from_utf8(output.stdout).map_err(|err| AdbDiscoveryError::Utf8(err.to_string()))?;
    Ok(parse_adb_devices_output(&stdout))
}

pub fn probe_adb_devices() -> Result<Vec<ProbedAdbDevice>, AdbDiscoveryError> {
    let devices = discover_adb_devices()?;
    let mut probed = Vec::with_capacity(devices.len());

    for device in devices {
        probed.push(probe_single_device(device));
    }

    Ok(probed)
}

pub fn install_agent_apk(serial: &str, apk_path: &str) -> Result<String, AdbDiscoveryError> {
    run_adb_for_serial(serial, &["install", "-r", apk_path]).map(|output| output.trim().to_string())
}

pub fn stat_remote_file_size(
    serial: &str,
    remote_path: &str,
) -> Result<Option<u64>, AdbDiscoveryError> {
    remote_file_size(serial, remote_path)
}

pub fn blake3_digest_remote_file(
    serial: &str,
    remote_path: &str,
) -> Result<Option<String>, AdbDiscoveryError> {
    if remote_file_size(serial, remote_path)?.is_none() {
        return Ok(None);
    }

    let script = format!("cat {}", shell_quote(remote_path));
    let mut command = Command::new("adb");
    command
        .arg("-s")
        .arg(serial)
        .args(["exec-out", "sh", "-c"])
        .arg(&script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        AdbDiscoveryError::CommandFailed("adb exec-out stdout unavailable".to_string())
    })?;
    let digest = blake3_digest_reader(&mut stdout)?;

    let mut stderr = String::new();
    if let Some(mut stderr_pipe) = child.stderr.take() {
        stderr_pipe
            .read_to_string(&mut stderr)
            .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    }

    let status = child
        .wait()
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    if !status.success() {
        let detail = stderr.trim();
        return Err(AdbDiscoveryError::CommandFailed(if detail.is_empty() {
            format!("adb exec-out cat failed for remote file: {remote_path}")
        } else {
            format!("adb exec-out cat failed for remote file {remote_path}: {detail}")
        }));
    }

    Ok(Some(digest))
}

pub fn pull_path_from_device(
    serial: &str,
    remote_path: &str,
    local_root: &Path,
) -> Result<String, AdbDiscoveryError> {
    fs::create_dir_all(local_root)
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    let local_root_string = local_root.to_string_lossy().to_string();
    run_adb_for_serial(serial, &["pull", remote_path, &local_root_string])
        .map(|output| output.trim().to_string())
}

pub fn remote_path_join(root: &str, relative_path: &Path) -> String {
    remote_join(root, relative_path)
}

pub fn write_file_chunk_at_offset(
    serial: &str,
    local_path: &Path,
    remote_file: &str,
    chunk_index: u32,
    offset: u64,
    length: u64,
) -> Result<String, AdbDiscoveryError> {
    let parent = remote_parent(remote_file);
    let script = if length == 0 {
        format!(
            "mkdir -p {parent} && touch {target}",
            parent = shell_quote(&parent),
            target = shell_quote(remote_file)
        )
    } else {
        let (block_size, seek_blocks) = if offset % 1_048_576 == 0 {
            (1_048_576u64, offset / 1_048_576)
        } else if offset % 4096 == 0 {
            (4096u64, offset / 4096)
        } else {
            (1u64, offset)
        };
        format!(
            "mkdir -p {parent} && dd of={target} bs={block_size} seek={seek_blocks} conv=notrunc status=none",
            parent = shell_quote(&parent),
            target = shell_quote(remote_file)
        )
    };
    write_local_range_to_adb_shell_stdin(serial, local_path, offset, length, &script)?;
    Ok(format!("chunk {chunk_index} streamed via adb stdin"))
}

pub fn remove_remote_path(serial: &str, remote_path: &str) -> Result<String, AdbDiscoveryError> {
    let script = format!("rm -rf {}", shell_quote(remote_path));
    run_adb_shell_script(serial, &script).map(|output| output.trim().to_string())
}

#[allow(dead_code)]
pub fn push_single_file_chunk_to_device(
    serial: &str,
    local_path: &Path,
    remote_file: &str,
    chunk_index: u32,
    offset: u64,
    length: u64,
) -> Result<String, AdbDiscoveryError> {
    let remote_chunk_dir = remote_chunk_dir(remote_file);
    let remote_chunk = remote_chunk_part(remote_file, chunk_index);
    let mkdir_script = format!(
        "mkdir -p {} {}",
        shell_quote(&remote_parent(remote_file)),
        shell_quote(&remote_chunk_dir)
    );
    run_adb_shell_script(serial, &mkdir_script)?;

    if remote_file_size(serial, &remote_chunk)? == Some(length) {
        return Ok(format!("chunk {chunk_index} already exists"));
    }

    let mut source =
        File::open(local_path).map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    skip_local_bytes_with_temp_buffer(&mut source, offset)?;
    let mut buffer = vec![0u8; length.min(1024 * 1024) as usize];
    let temp_chunk = write_temp_chunk(
        local_path,
        chunk_index as u64,
        length,
        &mut source,
        &mut buffer,
    )?;
    let temp_chunk_string = temp_chunk.to_string_lossy().to_string();
    let output = run_adb_for_serial(serial, &["push", &temp_chunk_string, &remote_chunk])?;
    let _ = fs::remove_file(&temp_chunk);
    Ok(output.trim().to_string())
}

#[allow(dead_code)]
pub fn assemble_remote_file_from_chunks(
    serial: &str,
    remote_file: &str,
) -> Result<(), AdbDiscoveryError> {
    assemble_remote_chunks(serial, remote_file, &remote_chunk_dir(remote_file))
}

#[allow(dead_code)]
pub fn push_path_to_device(
    serial: &str,
    local_path: &Path,
    remote_path: &str,
) -> Result<String, AdbDiscoveryError> {
    push_path_to_device_with_control(
        serial,
        local_path,
        remote_path,
        8 * 1024 * 1024,
        None,
        |_| {},
    )
}

pub fn push_path_to_device_with_control(
    serial: &str,
    local_path: &Path,
    remote_path: &str,
    chunk_size_bytes: u64,
    control: Option<AdbTransferControl>,
    mut progress: impl FnMut(AdbPushProgress),
) -> Result<String, AdbDiscoveryError> {
    let files = collect_push_files(local_path)?;
    let total_files = files.len();
    let mut pushed_files = 0usize;
    let mut skipped_files = 0usize;
    let mut pushed_chunks = 0u64;
    let mut skipped_chunks = 0u64;
    let mut bytes_scanned = 0u64;
    let mut bytes_pushed = 0u64;
    let mut outputs = Vec::new();

    for (file_index, file) in files.into_iter().enumerate() {
        check_pause(&control)?;
        bytes_scanned += file.size_bytes;
        let remote_file = if local_path.is_dir() {
            remote_join(remote_path, &file.relative_path)
        } else {
            remote_path.to_string()
        };

        if remote_file_size(serial, &remote_file)? == Some(file.size_bytes) {
            skipped_files += 1;
            progress(AdbPushProgress {
                event: "file-skipped".to_string(),
                relative_path: file.relative_path.to_string_lossy().to_string(),
                remote_path: remote_file,
                file_index,
                chunk_index: None,
                chunk_length: file.size_bytes,
                current_file: file_index + 1,
                total_files,
                pushed_files,
                skipped_files,
                pushed_chunks,
                skipped_chunks,
                bytes_scanned,
                bytes_pushed,
                message: "remote file already matches size".to_string(),
            });
            continue;
        }

        let chunk_result = push_file_chunks(
            serial,
            &file.local_path,
            &remote_file,
            file.size_bytes,
            chunk_size_bytes.max(1),
            control.as_ref(),
            |chunk_progress| {
                progress(AdbPushProgress {
                    event: chunk_progress.event,
                    relative_path: file.relative_path.to_string_lossy().to_string(),
                    remote_path: remote_file.clone(),
                    file_index,
                    chunk_index: Some(chunk_progress.chunk_index),
                    chunk_length: chunk_progress.chunk_length,
                    current_file: file_index + 1,
                    total_files,
                    pushed_files,
                    skipped_files,
                    pushed_chunks: pushed_chunks + chunk_progress.pushed_chunks,
                    skipped_chunks: skipped_chunks + chunk_progress.skipped_chunks,
                    bytes_scanned,
                    bytes_pushed: bytes_pushed + chunk_progress.bytes_pushed,
                    message: chunk_progress.message,
                });
            },
        )?;
        pushed_chunks += chunk_result.pushed_chunks;
        skipped_chunks += chunk_result.skipped_chunks;
        outputs.extend(chunk_result.outputs);
        pushed_files += 1;
        bytes_pushed += chunk_result.bytes_pushed;
        progress(AdbPushProgress {
            event: "file-completed".to_string(),
            relative_path: file.relative_path.to_string_lossy().to_string(),
            remote_path: remote_file,
            file_index,
            chunk_index: None,
            chunk_length: file.size_bytes,
            current_file: file_index + 1,
            total_files,
            pushed_files,
            skipped_files,
            pushed_chunks,
            skipped_chunks,
            bytes_scanned,
            bytes_pushed,
            message: "file completed".to_string(),
        });
    }

    Ok(format!(
        "ADB chunk push completed: scanned_files={scanned} pushed_files={pushed_files} skipped_files={skipped_files} pushed_chunks={pushed_chunks} skipped_chunks={skipped_chunks} bytes_scanned={bytes_scanned} bytes_pushed={bytes_pushed}\n{details}",
        scanned = pushed_files + skipped_files,
        details = outputs.join("\n")
    ))
}

pub fn parse_adb_devices_output(stdout: &str) -> Vec<DiscoveredAdbDevice> {
    stdout
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("List of devices attached"))
        .skip(1)
        .filter_map(parse_device_line)
        .collect()
}

pub fn parse_getprop_output(stdout: &str) -> BTreeMap<String, String> {
    let mut props = BTreeMap::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('[') {
            continue;
        }

        if let Some((key, value)) = trimmed.split_once("]: [") {
            let normalized_key = key.trim_start_matches('[').trim();
            let normalized_value = value.trim_end_matches(']').trim();
            props.insert(normalized_key.to_string(), normalized_value.to_string());
        }
    }

    props
}

fn parse_device_line(line: &str) -> Option<DiscoveredAdbDevice> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let serial = parts.next()?.to_string();
    let status = parts.next()?.to_string();
    let mut model = None;
    let mut product = None;
    let mut device_name = None;
    let mut transport_id = None;

    for token in parts {
        if let Some((key, value)) = token.split_once(':') {
            match key {
                "model" => model = Some(value.to_string()),
                "product" => product = Some(value.to_string()),
                "device" => device_name = Some(value.to_string()),
                "transport_id" => transport_id = Some(value.to_string()),
                _ => {}
            }
        }
    }

    let transport_hint = if serial.contains(':') {
        "ADB (TCP)".to_string()
    } else {
        "ADB (USB)".to_string()
    };

    Some(DiscoveredAdbDevice {
        serial,
        status,
        model,
        product,
        device_name,
        transport_id,
        transport_hint,
    })
}

fn probe_single_device(device: DiscoveredAdbDevice) -> ProbedAdbDevice {
    if device.status != "device" {
        let error = format!("device status is {}", device.status);
        return ProbedAdbDevice {
            discovered: device.clone(),
            adb_state: None,
            manufacturer: None,
            android_release: None,
            sdk_level: None,
            cpu_abi: None,
            adb_tcp_port: None,
            agent_package_path: None,
            wifi_agent_ip: None,
            wifi_agent_capability: None,
            remote_sdcard_ready: false,
            shell_ready: false,
            probe_error: Some(error.clone()),
            preflight_checks: build_preflight_checks(
                &device,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                false,
                false,
                Some(error),
            ),
        };
    }

    let adb_state = match run_adb_for_serial(&device.serial, &["get-state"]) {
        Ok(value) => Some(value.trim().to_string()),
        Err(err) => {
            let error = err.to_string();
            return ProbedAdbDevice {
                discovered: device.clone(),
                adb_state: None,
                manufacturer: None,
                android_release: None,
                sdk_level: None,
                cpu_abi: None,
                adb_tcp_port: None,
                agent_package_path: None,
                wifi_agent_ip: None,
                wifi_agent_capability: None,
                remote_sdcard_ready: false,
                shell_ready: false,
                probe_error: Some(error.clone()),
                preflight_checks: build_preflight_checks(
                    &device,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                    false,
                    Some(error),
                ),
            };
        }
    };

    let props = match run_adb_for_serial(&device.serial, &["shell", "getprop"]) {
        Ok(value) => parse_getprop_output(&value),
        Err(err) => {
            let error = err.to_string();
            return ProbedAdbDevice {
                discovered: device.clone(),
                adb_state: adb_state.clone(),
                manufacturer: None,
                android_release: None,
                sdk_level: None,
                cpu_abi: None,
                adb_tcp_port: None,
                agent_package_path: None,
                wifi_agent_ip: None,
                wifi_agent_capability: None,
                remote_sdcard_ready: false,
                shell_ready: false,
                probe_error: Some(error.clone()),
                preflight_checks: build_preflight_checks(
                    &device,
                    adb_state.as_deref(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                    false,
                    Some(error),
                ),
            };
        }
    };

    let manufacturer = props.get("ro.product.manufacturer").cloned();
    let android_release = props.get("ro.build.version.release").cloned();
    let sdk_level = props.get("ro.build.version.sdk").cloned();
    let cpu_abi = props.get("ro.product.cpu.abi").cloned();
    let adb_tcp_port = props.get("service.adb.tcp.port").cloned();
    let shell_ready = adb_state.as_deref() == Some("device");
    let agent_package_path = probe_agent_package_path(&device.serial)
        .ok()
        .filter(|value| !value.is_empty());
    let remote_sdcard_ready = probe_sdcard_ready(&device.serial).unwrap_or(false);
    let agent_capability = probe_agent_capability(&device.serial);

    ProbedAdbDevice {
        discovered: device.clone(),
        adb_state: adb_state.clone(),
        manufacturer: manufacturer.clone(),
        android_release: android_release.clone(),
        sdk_level: sdk_level.clone(),
        cpu_abi: cpu_abi.clone(),
        adb_tcp_port: adb_tcp_port.clone(),
        agent_package_path: agent_package_path.clone(),
        wifi_agent_ip: agent_capability.ip.clone(),
        wifi_agent_capability: agent_capability.capability_json.clone(),
        remote_sdcard_ready,
        shell_ready,
        probe_error: None,
        preflight_checks: build_preflight_checks(
            &device,
            adb_state.as_deref(),
            android_release.as_deref(),
            sdk_level.as_deref(),
            cpu_abi.as_deref(),
            adb_tcp_port.as_deref(),
            agent_package_path.as_deref(),
            Some(&agent_capability),
            remote_sdcard_ready,
            shell_ready,
            None,
        ),
    }
}

fn build_preflight_checks(
    device: &DiscoveredAdbDevice,
    adb_state: Option<&str>,
    android_release: Option<&str>,
    sdk_level: Option<&str>,
    cpu_abi: Option<&str>,
    adb_tcp_port: Option<&str>,
    agent_package_path: Option<&str>,
    agent_capability: Option<&AgentCapabilityProbe>,
    remote_sdcard_ready: bool,
    shell_ready: bool,
    probe_error: Option<String>,
) -> Vec<PreflightCheck> {
    let adb_link_ready = device.status == "device";
    let android_supported = sdk_level
        .and_then(|value| value.parse::<u32>().ok())
        .map(|value| value >= 26)
        .unwrap_or(false);
    let abi_known = cpu_abi.map(|value| !value.is_empty()).unwrap_or(false);
    let wifi_candidate = device.serial.contains(':')
        || adb_tcp_port
            .map(|value| value != "-1" && value != "0")
            .unwrap_or(false);

    vec![
        PreflightCheck {
            key: "adb_link",
            label: "ADB Link",
            passed: adb_link_ready,
            detail: format!("status={}", device.status),
        },
        PreflightCheck {
            key: "adb_shell",
            label: "Shell Ready",
            passed: shell_ready && adb_state == Some("device"),
            detail: adb_state.unwrap_or("unknown").to_string(),
        },
        PreflightCheck {
            key: "android_sdk",
            label: "Android SDK",
            passed: android_supported,
            detail: match (android_release, sdk_level) {
                (Some(release), Some(sdk)) => format!("Android {release} / SDK {sdk}"),
                _ => "unknown".to_string(),
            },
        },
        PreflightCheck {
            key: "cpu_abi",
            label: "CPU ABI",
            passed: abi_known,
            detail: cpu_abi.unwrap_or("unknown").to_string(),
        },
        PreflightCheck {
            key: "wifi_candidate",
            label: "Wi-Fi Candidate",
            passed: wifi_candidate,
            detail: adb_tcp_port.unwrap_or("disabled").to_string(),
        },
        PreflightCheck {
            key: "agent_package",
            label: "Agent Package",
            passed: agent_package_path.is_some(),
            detail: agent_package_path.unwrap_or("not installed").to_string(),
        },
        PreflightCheck {
            key: "agent_capability",
            label: "Agent Capability",
            passed: agent_capability
                .and_then(|probe| probe.capability_json.as_ref())
                .is_some(),
            detail: agent_capability
                .map(|probe| probe.detail.clone())
                .unwrap_or_else(|| "not probed".to_string()),
        },
        PreflightCheck {
            key: "remote_sdcard",
            label: "Remote /sdcard",
            passed: remote_sdcard_ready,
            detail: if remote_sdcard_ready {
                "reachable".to_string()
            } else {
                "not reachable".to_string()
            },
        },
        PreflightCheck {
            key: "probe_error",
            label: "Probe Error",
            passed: probe_error.is_none(),
            detail: probe_error.unwrap_or_else(|| "none".to_string()),
        },
    ]
}

fn remote_parent(remote_path: &str) -> String {
    let normalized = remote_path.trim_end_matches('/');
    match normalized.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((parent, _)) if !parent.is_empty() => parent.to_string(),
        _ => ".".to_string(),
    }
}

fn remote_join(root: &str, relative_path: &Path) -> String {
    let root = root.trim_end_matches('/');
    let mut output = if root.is_empty() {
        String::new()
    } else {
        root.to_string()
    };

    for component in relative_path.components() {
        let part = component.as_os_str().to_string_lossy();
        if part.is_empty() || part == "." {
            continue;
        }
        if !output.ends_with('/') {
            output.push('/');
        }
        output.push_str(&part.replace('\\', "/"));
    }

    if output.is_empty() {
        ".".to_string()
    } else {
        output
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChunkPushResult {
    pushed_chunks: u64,
    skipped_chunks: u64,
    bytes_pushed: u64,
    outputs: Vec<String>,
}

fn push_file_chunks(
    serial: &str,
    local_path: &Path,
    remote_file: &str,
    file_size: u64,
    chunk_size: u64,
    control: Option<&AdbTransferControl>,
    mut progress: impl FnMut(ChunkPushProgress),
) -> Result<ChunkPushResult, AdbDiscoveryError> {
    let chunk_size = chunk_size.max(1);
    let chunk_count = if file_size == 0 {
        1
    } else {
        file_size.div_ceil(chunk_size)
    };
    let remote_chunk_dir = remote_chunk_dir(remote_file);
    let mkdir_script = format!(
        "mkdir -p {} {}",
        shell_quote(&remote_parent(remote_file)),
        shell_quote(&remote_chunk_dir)
    );
    run_adb_shell_script(serial, &mkdir_script)?;

    let mut result = ChunkPushResult {
        pushed_chunks: 0,
        skipped_chunks: 0,
        bytes_pushed: 0,
        outputs: Vec::new(),
    };
    let mut source =
        File::open(local_path).map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    let mut buffer = vec![0u8; chunk_size.min(1024 * 1024) as usize];

    for chunk_index in 0..chunk_count {
        check_pause(&control.cloned())?;
        let remaining = file_size.saturating_sub(chunk_index * chunk_size);
        let chunk_length = if file_size == 0 {
            0
        } else {
            remaining.min(chunk_size)
        };
        let remote_chunk = remote_chunk_part(remote_file, chunk_index as u32);

        if remote_file_size(serial, &remote_chunk)? == Some(chunk_length) {
            skip_local_bytes(&mut source, chunk_length, &mut buffer)?;
            result.skipped_chunks += 1;
            progress(ChunkPushProgress {
                event: "chunk-skipped".to_string(),
                chunk_index: chunk_index as u32,
                chunk_length,
                pushed_chunks: result.pushed_chunks,
                skipped_chunks: result.skipped_chunks,
                bytes_pushed: result.bytes_pushed,
                message: format!("chunk {chunk_index} already exists"),
            });
            continue;
        }

        let temp_chunk = write_temp_chunk(
            local_path,
            chunk_index,
            chunk_length,
            &mut source,
            &mut buffer,
        )?;
        let temp_chunk_string = temp_chunk.to_string_lossy().to_string();
        let output = run_adb_for_serial(serial, &["push", &temp_chunk_string, &remote_chunk])?;
        let _ = fs::remove_file(&temp_chunk);
        result.outputs.push(output.trim().to_string());
        result.pushed_chunks += 1;
        result.bytes_pushed += chunk_length;
        progress(ChunkPushProgress {
            event: "chunk-pushed".to_string(),
            chunk_index: chunk_index as u32,
            chunk_length,
            pushed_chunks: result.pushed_chunks,
            skipped_chunks: result.skipped_chunks,
            bytes_pushed: result.bytes_pushed,
            message: format!("chunk {chunk_index} pushed"),
        });
    }

    check_pause(&control.cloned())?;
    assemble_remote_chunks(serial, remote_file, &remote_chunk_dir)?;
    Ok(result)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChunkPushProgress {
    event: String,
    chunk_index: u32,
    chunk_length: u64,
    pushed_chunks: u64,
    skipped_chunks: u64,
    bytes_pushed: u64,
    message: String,
}

fn check_pause(control: &Option<AdbTransferControl>) -> Result<(), AdbDiscoveryError> {
    if control
        .as_ref()
        .map(AdbTransferControl::is_paused)
        .unwrap_or(false)
    {
        return Err(AdbDiscoveryError::Paused(
            "transfer paused at chunk boundary".to_string(),
        ));
    }
    Ok(())
}

fn skip_local_bytes(
    source: &mut File,
    mut length: u64,
    buffer: &mut [u8],
) -> Result<(), AdbDiscoveryError> {
    while length > 0 {
        let read_len = (length as usize).min(buffer.len());
        source
            .read_exact(&mut buffer[..read_len])
            .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
        length -= read_len as u64;
    }
    Ok(())
}

#[allow(dead_code)]
fn skip_local_bytes_with_temp_buffer(
    source: &mut File,
    length: u64,
) -> Result<(), AdbDiscoveryError> {
    let mut buffer = vec![0u8; 1024 * 1024];
    skip_local_bytes(source, length, &mut buffer)
}

fn write_temp_chunk(
    local_path: &Path,
    chunk_index: u64,
    mut length: u64,
    source: &mut File,
    buffer: &mut [u8],
) -> Result<std::path::PathBuf, AdbDiscoveryError> {
    let temp_dir = std::env::temp_dir().join("nekotrans-adb-chunks");
    fs::create_dir_all(&temp_dir)
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    let file_name = local_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("chunk");
    let temp_path = temp_dir.join(format!(
        "{}-{}-{chunk_index:08}.part",
        std::process::id(),
        sanitize_remote_fragment(file_name)
    ));
    let mut output = File::create(&temp_path)
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;

    while length > 0 {
        let read_len = (length as usize).min(buffer.len());
        source
            .read_exact(&mut buffer[..read_len])
            .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
        output
            .write_all(&buffer[..read_len])
            .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
        length -= read_len as u64;
    }

    Ok(temp_path)
}

fn assemble_remote_chunks(
    serial: &str,
    remote_file: &str,
    remote_chunk_dir: &str,
) -> Result<(), AdbDiscoveryError> {
    let temp_file = format!("{remote_file}.nekotrans-tmp");
    let script = format!(
        "cat {chunk_dir}/part-* > {temp_file} && mv {temp_file} {remote_file} && rm -rf {chunk_dir}",
        chunk_dir = shell_quote(remote_chunk_dir),
        temp_file = shell_quote(&temp_file),
        remote_file = shell_quote(remote_file)
    );
    run_adb_shell_script(serial, &script)?;
    Ok(())
}

fn remote_chunk_dir(remote_file: &str) -> String {
    format!(
        "/sdcard/Nekotrans/.chunks/{:016x}-{}",
        stable_hash(remote_file),
        sanitize_remote_fragment(remote_file)
    )
}

fn remote_chunk_part(remote_file: &str, chunk_index: u32) -> String {
    format!("{}/part-{chunk_index:08}", remote_chunk_dir(remote_file))
}

fn sanitize_remote_fragment(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric()
            || character == '.'
            || character == '-'
            || character == '_'
        {
            output.push(character);
        } else {
            output.push('_');
        }
    }
    output.trim_matches('_').chars().take(80).collect()
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PushFile {
    local_path: std::path::PathBuf,
    relative_path: std::path::PathBuf,
    size_bytes: u64,
}

fn collect_push_files(local_path: &Path) -> Result<Vec<PushFile>, AdbDiscoveryError> {
    if local_path.is_file() {
        let metadata = fs::metadata(local_path)
            .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
        return Ok(vec![PushFile {
            local_path: local_path.to_path_buf(),
            relative_path: local_path
                .file_name()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(".")),
            size_bytes: metadata.len(),
        }]);
    }

    if !local_path.is_dir() {
        return Err(AdbDiscoveryError::CommandFailed(format!(
            "local path does not exist: {}",
            local_path.display()
        )));
    }

    let mut files = Vec::new();
    collect_directory_files(local_path, local_path, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn collect_directory_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<PushFile>,
) -> Result<(), AdbDiscoveryError> {
    for entry in
        fs::read_dir(current).map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?
    {
        let entry = entry.map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;

        if metadata.is_dir() {
            collect_directory_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?
                .to_path_buf();
            files.push(PushFile {
                local_path: path,
                relative_path,
                size_bytes: metadata.len(),
            });
        }
    }

    Ok(())
}

fn remote_file_size(serial: &str, remote_path: &str) -> Result<Option<u64>, AdbDiscoveryError> {
    let script = format!(
        "if [ -f {path} ]; then stat -c %s {path}; else echo missing; fi",
        path = shell_quote(remote_path)
    );
    let output = run_adb_shell_script(serial, &script)?;
    let trimmed = output.trim();
    if trimmed == "missing" || trimmed.is_empty() {
        return Ok(None);
    }

    trimmed
        .parse::<u64>()
        .map(Some)
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))
}

fn blake3_digest_reader(reader: &mut impl Read) -> Result<String, AdbDiscoveryError> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn run_adb_for_serial(serial: &str, args: &[&str]) -> Result<String, AdbDiscoveryError> {
    let mut command = Command::new("adb");
    command.arg("-s").arg(serial);
    for arg in args {
        command.arg(arg);
    }

    let output = command
        .output()
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8(output.stderr)
            .map_err(|err| AdbDiscoveryError::Utf8(err.to_string()))?;
        return Err(AdbDiscoveryError::CommandFailed(stderr.trim().to_string()));
    }

    String::from_utf8(output.stdout).map_err(|err| AdbDiscoveryError::Utf8(err.to_string()))
}

fn write_local_range_to_adb_shell_stdin(
    serial: &str,
    local_path: &Path,
    offset: u64,
    length: u64,
    script: &str,
) -> Result<String, AdbDiscoveryError> {
    let mut child = Command::new("adb")
        .arg("-s")
        .arg(serial)
        .arg("exec-in")
        .arg("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;

    if length > 0 {
        let mut source = File::open(local_path)
            .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
        source
            .seek(SeekFrom::Start(offset))
            .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| AdbDiscoveryError::CommandFailed("adb stdin unavailable".to_string()))?;
        let mut remaining = length;
        let mut buffer = vec![0u8; remaining.min(1024 * 1024).max(1) as usize];
        while remaining > 0 {
            let wanted = remaining.min(buffer.len() as u64) as usize;
            let read = source
                .read(&mut buffer[..wanted])
                .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
            if read == 0 {
                return Err(AdbDiscoveryError::CommandFailed(format!(
                    "local file ended before adb stdin chunk was complete: {}",
                    local_path.display()
                )));
            }
            stdin
                .write_all(&buffer[..read])
                .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
            remaining -= read as u64;
        }
    }
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AdbDiscoveryError::CommandFailed(if detail.is_empty() {
            "adb exec-in stdin write failed".to_string()
        } else {
            detail
        }));
    }
    String::from_utf8(output.stdout).map_err(|err| AdbDiscoveryError::Utf8(err.to_string()))
}

fn run_adb_shell_script(serial: &str, script: &str) -> Result<String, AdbDiscoveryError> {
    run_adb_for_serial(serial, &["shell", script])
}

fn probe_agent_package_path(serial: &str) -> Result<String, AdbDiscoveryError> {
    let output = run_adb_for_serial(serial, &["shell", "pm", "path", "com.nekotrans.agent"])?;
    Ok(output.trim().to_string())
}

fn probe_sdcard_ready(serial: &str) -> Result<bool, AdbDiscoveryError> {
    let output = run_adb_shell_script(
        serial,
        "if [ -d /sdcard ]; then echo ready; else echo missing; fi",
    )?;
    Ok(output.trim() == "ready")
}

fn probe_agent_capability(serial: &str) -> AgentCapabilityProbe {
    let route_output = match run_adb_shell_script(serial, "ip route get 8.8.8.8") {
        Ok(output) => output,
        Err(err) => {
            return match read_agent_capability_via_adb_tunnel(serial) {
                Ok(capability_json) => AgentCapabilityProbe {
                    ip: None,
                    capability_json: Some(capability_json),
                    detail: "ADB tunnel ready".to_string(),
                },
                Err(tunnel_err) => AgentCapabilityProbe {
                    ip: None,
                    capability_json: None,
                    detail: format!(
                        "ip route probe failed: {err}; adb tunnel unavailable: {tunnel_err}"
                    ),
                },
            };
        }
    };

    let Some(ip) = parse_default_route_source_ip(&route_output) else {
        return match read_agent_capability_via_adb_tunnel(serial) {
            Ok(capability_json) => AgentCapabilityProbe {
                ip: None,
                capability_json: Some(capability_json),
                detail: "ADB tunnel ready".to_string(),
            },
            Err(tunnel_err) => AgentCapabilityProbe {
                ip: None,
                capability_json: None,
                detail: format!("device LAN IP not found; adb tunnel unavailable: {tunnel_err}"),
            },
        };
    };

    match read_agent_capability(&ip) {
        Ok(capability_json) => AgentCapabilityProbe {
            ip: Some(ip.clone()),
            capability_json: Some(capability_json),
            detail: format!("{ip}:{AGENT_CAPABILITY_PORT} ready"),
        },
        Err(err) => match read_agent_capability_via_adb_tunnel(serial) {
            Ok(capability_json) => AgentCapabilityProbe {
                ip: Some(ip.clone()),
                capability_json: Some(capability_json),
                detail: format!("{ip}:{AGENT_CAPABILITY_PORT} unavailable; ADB tunnel ready"),
            },
            Err(tunnel_err) => AgentCapabilityProbe {
                ip: Some(ip.clone()),
                capability_json: None,
                detail: format!(
                    "{ip}:{AGENT_CAPABILITY_PORT} unavailable: {err}; adb tunnel unavailable: {tunnel_err}"
                ),
            },
        },
    }
}

fn read_agent_capability(ip: &str) -> Result<String, AdbDiscoveryError> {
    let address: SocketAddr = format!("{ip}:{AGENT_CAPABILITY_PORT}")
        .parse()
        .map_err(|err| AdbDiscoveryError::CommandFailed(format!("invalid agent address: {err}")))?;
    let timeout = Duration::from_millis(AGENT_CONNECT_TIMEOUT_MS);
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    stream
        .write_all(b"HELLO\n")
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    stream
        .flush()
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;

    let mut output = String::new();
    stream
        .read_to_string(&mut output)
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    Ok(output.trim().to_string())
}

fn read_agent_capability_via_adb_tunnel(serial: &str) -> Result<String, AdbDiscoveryError> {
    let forwarded_port = forward_agent_port(serial)?;
    read_agent_capability_from_endpoint("127.0.0.1", forwarded_port)
}

fn forward_agent_port(serial: &str) -> Result<u16, AdbDiscoveryError> {
    let local_port = local_agent_forward_port(serial);
    let local = format!("tcp:{local_port}");
    let remote = format!("tcp:{AGENT_CAPABILITY_PORT}");
    let _ = run_adb_for_serial(serial, &["forward", "--remove", &local]);
    run_adb_for_serial(serial, &["forward", &local, &remote])?;
    Ok(local_port)
}

fn local_agent_forward_port(serial: &str) -> u16 {
    let mut hash = 0u16;
    for byte in serial.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as u16);
    }
    39000u16 + (hash % 1000)
}

fn read_agent_capability_from_endpoint(host: &str, port: u16) -> Result<String, AdbDiscoveryError> {
    let address: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|err| AdbDiscoveryError::CommandFailed(format!("invalid agent address: {err}")))?;
    let timeout = Duration::from_millis(AGENT_CONNECT_TIMEOUT_MS);
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    stream
        .write_all(b"HELLO\n")
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    stream
        .flush()
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;

    let mut output = String::new();
    stream
        .read_to_string(&mut output)
        .map_err(|err| AdbDiscoveryError::CommandFailed(err.to_string()))?;
    Ok(output.trim().to_string())
}

fn parse_default_route_source_ip(output: &str) -> Option<String> {
    let mut tokens = output.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "src" {
            return tokens.next().map(str::to_string);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        AdbDiscoveryError, AdbTransferControl, AgentCapabilityProbe, DiscoveredAdbDevice,
        blake3_digest_reader, build_preflight_checks, check_pause, local_agent_forward_port,
        parse_adb_devices_output, parse_default_route_source_ip, parse_getprop_output,
        remote_chunk_dir, remote_chunk_part, remote_join, remote_parent, sanitize_remote_fragment,
        shell_quote,
    };
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    #[test]
    fn parses_usb_and_tcp_devices() {
        let output = r#"List of devices attached
R3CN30ABCDEF       device usb:1-4 product:cheetah model:Pixel_7_Pro device:cheetah transport_id:3
192.168.31.9:5555  unauthorized transport_id:5

"#;

        let devices = parse_adb_devices_output(output);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].serial, "R3CN30ABCDEF");
        assert_eq!(devices[0].status, "device");
        assert_eq!(devices[0].model.as_deref(), Some("Pixel_7_Pro"));
        assert_eq!(devices[0].transport_hint, "ADB (USB)");
        assert_eq!(devices[1].transport_hint, "ADB (TCP)");
        assert_eq!(devices[1].status, "unauthorized");
    }

    #[test]
    fn ignores_header_and_empty_lines() {
        let output = "List of devices attached\n\n";
        let devices = parse_adb_devices_output(output);
        assert!(devices.is_empty());
    }

    #[test]
    fn parses_getprop_key_values() {
        let output = r#"
[ro.product.manufacturer]: [Google]
[ro.build.version.release]: [14]
[ro.build.version.sdk]: [34]
[service.adb.tcp.port]: [5555]
"#;

        let props = parse_getprop_output(output);
        assert_eq!(
            props.get("ro.product.manufacturer").map(String::as_str),
            Some("Google")
        );
        assert_eq!(
            props.get("ro.build.version.release").map(String::as_str),
            Some("14")
        );
        assert_eq!(
            props.get("service.adb.tcp.port").map(String::as_str),
            Some("5555")
        );
    }

    #[test]
    fn builds_preflight_checks_from_probe_inputs() {
        let device = DiscoveredAdbDevice {
            serial: "R3CN30ABCDEF".to_string(),
            status: "device".to_string(),
            model: Some("Pixel_7_Pro".to_string()),
            product: Some("cheetah".to_string()),
            device_name: Some("cheetah".to_string()),
            transport_id: Some("3".to_string()),
            transport_hint: "ADB (USB)".to_string(),
        };

        let checks = build_preflight_checks(
            &device,
            Some("device"),
            Some("14"),
            Some("34"),
            Some("arm64-v8a"),
            Some("5555"),
            Some("package:/data/app/~~abc/base.apk"),
            Some(&AgentCapabilityProbe {
                ip: Some("192.168.31.20".to_string()),
                capability_json: Some("{\"type\":\"Capability\"}".to_string()),
                detail: "192.168.31.20:38997 ready".to_string(),
            }),
            true,
            true,
            None,
        );

        assert!(
            checks
                .iter()
                .any(|check| check.key == "adb_shell" && check.passed)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.key == "android_sdk" && check.passed)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.key == "wifi_candidate" && check.passed)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.key == "agent_package" && check.passed)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.key == "agent_capability" && check.passed)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.key == "remote_sdcard" && check.passed)
        );
    }

    #[test]
    fn parses_default_route_source_ip() {
        let output = "8.8.8.8 via 192.168.31.1 dev wlan0 src 192.168.31.20 uid 2000";
        assert_eq!(
            parse_default_route_source_ip(output).as_deref(),
            Some("192.168.31.20")
        );
        assert_eq!(
            parse_default_route_source_ip("unreachable").as_deref(),
            None
        );
    }

    #[test]
    fn derives_stable_local_forward_port_from_serial() {
        let first = local_agent_forward_port("R3CN30ABCDEF");
        let second = local_agent_forward_port("R3CN30ABCDEF");
        let other = local_agent_forward_port("ZX1G22");
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert!((39000..40000).contains(&first));
    }

    #[test]
    fn hashes_streamed_bytes_with_blake3() {
        let mut reader = Cursor::new(b"nekotrans remote bytes".to_vec());
        let digest = blake3_digest_reader(&mut reader).expect("digest should succeed");
        let expected = blake3::hash(b"nekotrans remote bytes").to_hex().to_string();
        assert_eq!(digest, expected);
    }

    #[test]
    fn computes_remote_parent_paths() {
        assert_eq!(remote_parent("/sdcard/NekotransDocs"), "/sdcard");
        assert_eq!(remote_parent("/sdcard/NekotransDocs/"), "/sdcard");
        assert_eq!(remote_parent("relative.txt"), ".");
        assert_eq!(remote_parent("/file.txt"), "/");
    }

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(shell_quote("/sdcard/My Docs"), "'/sdcard/My Docs'");
        assert_eq!(shell_quote("/sdcard/it's"), "'/sdcard/it'\\''s'");
    }

    #[test]
    fn joins_remote_paths_with_unix_separators() {
        assert_eq!(
            remote_join("/sdcard/NekotransDocs", Path::new("a/b/readme.txt")),
            "/sdcard/NekotransDocs/a/b/readme.txt"
        );
        assert_eq!(
            remote_join("/sdcard/NekotransDocs/", Path::new("readme.txt")),
            "/sdcard/NekotransDocs/readme.txt"
        );
    }

    #[test]
    fn builds_stable_remote_chunk_dirs() {
        let first = remote_chunk_dir("/sdcard/NekotransDocs/archive.bin");
        let second = remote_chunk_dir("/sdcard/NekotransDocs/archive.bin");
        assert_eq!(first, second);
        assert!(first.starts_with("/sdcard/Nekotrans/.chunks/"));
        assert!(first.contains("archive.bin"));
    }

    #[test]
    fn builds_stable_remote_chunk_part_paths() {
        assert_eq!(
            remote_chunk_part("/sdcard/NekotransDocs/archive.bin", 7),
            format!(
                "{}/part-00000007",
                remote_chunk_dir("/sdcard/NekotransDocs/archive.bin")
            )
        );
    }

    #[test]
    fn sanitizes_remote_fragments_for_shell_paths() {
        assert_eq!(
            sanitize_remote_fragment("/sdcard/My Docs/it's.bin"),
            "sdcard_My_Docs_it_s.bin"
        );
    }

    #[test]
    fn pause_control_stops_at_boundaries() {
        let flag = Arc::new(AtomicBool::new(true));
        let control = Some(AdbTransferControl::new(flag));
        let result = check_pause(&control);
        assert!(matches!(result, Err(AdbDiscoveryError::Paused(_))));

        control
            .as_ref()
            .expect("control should exist")
            .pause_requested
            .store(false, Ordering::Relaxed);
        assert!(check_pause(&control).is_ok());
    }
}
