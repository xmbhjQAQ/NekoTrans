use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type TaskId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    PcToAndroid,
    AndroidToPc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    AdbOnly,
    WifiOnly,
    Dual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskConfig {
    pub task_id: TaskId,
    pub direction: Direction,
    pub transport_mode: TransportMode,
    pub verify_enabled: bool,
    pub source_root: PathBuf,
    pub target_root: String,
    pub chunk_size_bytes: u64,
    pub small_file_threshold_bytes: u64,
    pub max_in_flight_chunks_per_lane: usize,
    pub created_at_epoch_ms: u128,
}

impl TaskConfig {
    pub fn new(
        task_id: impl Into<String>,
        direction: Direction,
        transport_mode: TransportMode,
        verify_enabled: bool,
        source_root: PathBuf,
        target_root: impl Into<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            direction,
            transport_mode,
            verify_enabled,
            source_root,
            target_root: target_root.into(),
            chunk_size_bytes: 8 * 1024 * 1024,
            small_file_threshold_bytes: 32 * 1024 * 1024,
            max_in_flight_chunks_per_lane: 4,
            created_at_epoch_ms: now_epoch_ms(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferItem {
    pub relative_path: PathBuf,
    pub size_bytes: u64,
    pub modified_at_epoch_ms: u128,
    pub fingerprint: Option<FileFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    pub algorithm: &'static str,
    pub hex_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkDescriptor {
    pub file_index: usize,
    pub chunk_index: u32,
    pub offset: u64,
    pub length: u64,
}

impl Display for ChunkDescriptor {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "file={} chunk={} offset={} length={}",
            self.file_index, self.chunk_index, self.offset, self.length
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCapability {
    pub supports_adb: bool,
    pub supports_wifi: bool,
    pub supports_dual: bool,
    pub protocol_version: String,
    pub app_version: String,
    pub full_storage_access_granted: bool,
}

impl Default for DeviceCapability {
    fn default() -> Self {
        Self {
            supports_adb: true,
            supports_wifi: true,
            supports_dual: true,
            protocol_version: "0.1".to_string(),
            app_version: "0.1.0".to_string(),
            full_storage_access_granted: false,
        }
    }
}

pub fn split_into_chunks(
    file_index: usize,
    size_bytes: u64,
    chunk_size_bytes: u64,
) -> Vec<ChunkDescriptor> {
    if size_bytes == 0 {
        return vec![ChunkDescriptor {
            file_index,
            chunk_index: 0,
            offset: 0,
            length: 0,
        }];
    }

    let chunk_size = chunk_size_bytes.max(1);
    let chunks = size_bytes.div_ceil(chunk_size);
    let mut output = Vec::with_capacity(chunks as usize);

    for chunk_index in 0..chunks {
        let offset = chunk_index * chunk_size;
        let remaining = size_bytes.saturating_sub(offset);
        output.push(ChunkDescriptor {
            file_index,
            chunk_index: chunk_index as u32,
            offset,
            length: remaining.min(chunk_size),
        });
    }

    output
}

pub fn is_large_file(size_bytes: u64, small_file_threshold_bytes: u64) -> bool {
    size_bytes >= small_file_threshold_bytes
}

pub fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_millis()
}
