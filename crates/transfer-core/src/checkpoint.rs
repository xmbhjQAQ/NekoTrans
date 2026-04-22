use crate::models::{Direction, TaskConfig, TaskState, TransferItem, TransportMode};
use crate::scheduler::LaneAssignment;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCheckpoint {
    pub relative_path: PathBuf,
    pub size_bytes: u64,
    pub modified_at_epoch_ms: u128,
    pub fingerprint_hex: Option<String>,
    pub completed_chunks: Vec<u32>,
    pub completed_chunk_lanes: BTreeMap<u32, LaneAssignment>,
    pub verification_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCheckpoint {
    pub task_id: String,
    pub state: TaskState,
    pub transport_mode: TransportMode,
    pub verify_enabled: bool,
    pub updated_at_epoch_ms: u128,
    pub files: Vec<FileCheckpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointEntry {
    pub config: TaskConfig,
    pub checkpoint: TaskCheckpoint,
}

#[derive(Debug)]
pub struct CheckpointStore {
    root: PathBuf,
}

#[derive(Debug)]
pub enum CheckpointError {
    Io(std::io::Error),
    Parse(String),
}

impl Display for CheckpointError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Parse(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for CheckpointError {}

impl From<std::io::Error> for CheckpointError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl CheckpointStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn persist(&self, entry: &CheckpointEntry) -> Result<PathBuf, CheckpointError> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join(format!("{}.ckpt", entry.config.task_id));
        fs::write(&path, serialize_entry(entry))?;
        Ok(path)
    }

    pub fn load(&self, task_id: &str) -> Result<CheckpointEntry, CheckpointError> {
        let path = self.root.join(format!("{task_id}.ckpt"));
        let content = fs::read_to_string(path)?;
        deserialize_entry(&content)
    }

    pub fn list(&self) -> Result<Vec<PathBuf>, CheckpointError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("ckpt") {
                entries.push(path);
            }
        }
        entries.sort();
        Ok(entries)
    }
}

pub fn initialize_checkpoint(config: &TaskConfig, items: &[TransferItem]) -> CheckpointEntry {
    let files = items
        .iter()
        .map(|item| FileCheckpoint {
            relative_path: item.relative_path.clone(),
            size_bytes: item.size_bytes,
            modified_at_epoch_ms: item.modified_at_epoch_ms,
            fingerprint_hex: item
                .fingerprint
                .as_ref()
                .map(|value| value.hex_digest.clone()),
            completed_chunks: Vec::new(),
            completed_chunk_lanes: BTreeMap::new(),
            verification_digest: None,
        })
        .collect();

    CheckpointEntry {
        config: config.clone(),
        checkpoint: TaskCheckpoint {
            task_id: config.task_id.clone(),
            state: TaskState::Pending,
            transport_mode: config.transport_mode,
            verify_enabled: config.verify_enabled,
            updated_at_epoch_ms: config.created_at_epoch_ms,
            files,
        },
    }
}

fn serialize_entry(entry: &CheckpointEntry) -> String {
    let mut lines = vec![
        format!("task_id={}", entry.config.task_id),
        format!("direction={}", serialize_direction(entry.config.direction)),
        format!(
            "transport_mode={}",
            serialize_transport_mode(entry.config.transport_mode)
        ),
        format!("verify_enabled={}", entry.config.verify_enabled),
        format!(
            "source_root={}",
            escape(&entry.config.source_root.to_string_lossy())
        ),
        format!("target_root={}", escape(&entry.config.target_root)),
        format!("chunk_size_bytes={}", entry.config.chunk_size_bytes),
        format!(
            "small_file_threshold_bytes={}",
            entry.config.small_file_threshold_bytes
        ),
        format!(
            "max_in_flight_chunks_per_lane={}",
            entry.config.max_in_flight_chunks_per_lane
        ),
        format!("created_at_epoch_ms={}", entry.config.created_at_epoch_ms),
        format!("state={}", serialize_state(entry.checkpoint.state)),
        format!(
            "updated_at_epoch_ms={}",
            entry.checkpoint.updated_at_epoch_ms
        ),
    ];

    for file in &entry.checkpoint.files {
        let chunks = file
            .completed_chunks
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let digest = file.verification_digest.clone().unwrap_or_default();
        let fingerprint = file.fingerprint_hex.clone().unwrap_or_default();
        let lanes = file
            .completed_chunk_lanes
            .iter()
            .map(|(chunk_index, lane)| format!("{chunk_index}:{}", serialize_lane(*lane)))
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!(
            "file={}|{}|{}|{}|{}|{}|{}",
            escape(&file.relative_path.to_string_lossy()),
            file.size_bytes,
            file.modified_at_epoch_ms,
            chunks,
            escape(&digest),
            escape(&fingerprint),
            lanes
        ));
    }

    lines.join("\n")
}

fn deserialize_entry(content: &str) -> Result<CheckpointEntry, CheckpointError> {
    let mut map = BTreeMap::<String, String>::new();
    let mut files = Vec::new();

    for line in content.lines() {
        if let Some(value) = line.strip_prefix("file=") {
            let mut parts = value.splitn(7, '|');
            let relative = parts
                .next()
                .ok_or_else(|| CheckpointError::Parse("missing file path".to_string()))?;
            let size = parts
                .next()
                .ok_or_else(|| CheckpointError::Parse("missing file size".to_string()))?;
            let third = parts
                .next()
                .ok_or_else(|| CheckpointError::Parse("missing chunk data".to_string()))?;
            let fourth = parts.next().unwrap_or_default();
            let fifth = parts.next();
            let sixth = parts.next();
            let seventh = parts.next();

            let (modified_at_epoch_ms, chunks, digest, fingerprint, lanes) =
                if let Some(lanes) = seventh {
                    (
                        third.parse::<u128>().map_err(|_| {
                            CheckpointError::Parse(format!("invalid modified_at_epoch_ms {third}"))
                        })?,
                        fourth,
                        fifth.unwrap_or_default(),
                        sixth.unwrap_or_default(),
                        lanes,
                    )
                } else if let Some(fingerprint) = sixth {
                    (
                        third.parse::<u128>().map_err(|_| {
                            CheckpointError::Parse(format!("invalid modified_at_epoch_ms {third}"))
                        })?,
                        fourth,
                        fifth.unwrap_or_default(),
                        fingerprint,
                        "",
                    )
                } else {
                    (0, third, fourth, fifth.unwrap_or_default(), "")
                };

            let completed_chunks = if chunks.is_empty() {
                Vec::new()
            } else {
                chunks
                    .split(',')
                    .map(|chunk| {
                        chunk
                            .parse::<u32>()
                            .map_err(|_| CheckpointError::Parse(format!("invalid chunk {chunk}")))
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            let completed_chunk_lanes = if lanes.is_empty() {
                BTreeMap::new()
            } else {
                lanes
                    .split(',')
                    .filter(|entry| !entry.is_empty())
                    .map(|entry| {
                        let (chunk_index, lane) = entry.split_once(':').ok_or_else(|| {
                            CheckpointError::Parse(format!("invalid completed lane {entry}"))
                        })?;
                        Ok((
                            chunk_index.parse::<u32>().map_err(|_| {
                                CheckpointError::Parse(format!(
                                    "invalid completed lane chunk {chunk_index}"
                                ))
                            })?,
                            parse_lane(lane)?,
                        ))
                    })
                    .collect::<Result<BTreeMap<_, _>, CheckpointError>>()?
            };

            files.push(FileCheckpoint {
                relative_path: PathBuf::from(unescape(relative)),
                size_bytes: size
                    .parse::<u64>()
                    .map_err(|_| CheckpointError::Parse(format!("invalid file size {size}")))?,
                modified_at_epoch_ms,
                fingerprint_hex: match unescape(fingerprint) {
                    value if value.is_empty() => None,
                    value => Some(value),
                },
                completed_chunks,
                completed_chunk_lanes,
                verification_digest: match unescape(digest) {
                    value if value.is_empty() => None,
                    value => Some(value),
                },
            });
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.to_string(), value.to_string());
        }
    }

    let task_id = required(&map, "task_id")?;
    let direction = parse_direction(required(&map, "direction")?)?;
    let transport_mode = parse_transport_mode(required(&map, "transport_mode")?)?;
    let verify_enabled = required(&map, "verify_enabled")?
        .parse::<bool>()
        .map_err(|_| CheckpointError::Parse("invalid verify_enabled".to_string()))?;
    let source_root = PathBuf::from(unescape(required(&map, "source_root")?));
    let target_root = unescape(required(&map, "target_root")?);
    let chunk_size_bytes = required(&map, "chunk_size_bytes")?
        .parse::<u64>()
        .map_err(|_| CheckpointError::Parse("invalid chunk_size_bytes".to_string()))?;
    let small_file_threshold_bytes = required(&map, "small_file_threshold_bytes")?
        .parse::<u64>()
        .map_err(|_| CheckpointError::Parse("invalid small_file_threshold_bytes".to_string()))?;
    let max_in_flight_chunks_per_lane = required(&map, "max_in_flight_chunks_per_lane")?
        .parse::<usize>()
        .map_err(|_| CheckpointError::Parse("invalid max_in_flight_chunks_per_lane".to_string()))?;
    let created_at_epoch_ms = required(&map, "created_at_epoch_ms")?
        .parse::<u128>()
        .map_err(|_| CheckpointError::Parse("invalid created_at_epoch_ms".to_string()))?;
    let state = parse_state(required(&map, "state")?)?;
    let updated_at_epoch_ms = required(&map, "updated_at_epoch_ms")?
        .parse::<u128>()
        .map_err(|_| CheckpointError::Parse("invalid updated_at_epoch_ms".to_string()))?;

    Ok(CheckpointEntry {
        config: TaskConfig {
            task_id: task_id.to_string(),
            direction,
            transport_mode,
            verify_enabled,
            source_root,
            target_root,
            chunk_size_bytes,
            small_file_threshold_bytes,
            max_in_flight_chunks_per_lane,
            created_at_epoch_ms,
        },
        checkpoint: TaskCheckpoint {
            task_id: task_id.to_string(),
            state,
            transport_mode,
            verify_enabled,
            updated_at_epoch_ms,
            files,
        },
    })
}

fn required<'a>(map: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, CheckpointError> {
    map.get(key)
        .map(String::as_str)
        .ok_or_else(|| CheckpointError::Parse(format!("missing {key}")))
}

fn serialize_direction(value: Direction) -> &'static str {
    match value {
        Direction::PcToAndroid => "pc_to_android",
        Direction::AndroidToPc => "android_to_pc",
    }
}

fn parse_direction(value: &str) -> Result<Direction, CheckpointError> {
    match value {
        "pc_to_android" => Ok(Direction::PcToAndroid),
        "android_to_pc" => Ok(Direction::AndroidToPc),
        _ => Err(CheckpointError::Parse(format!("invalid direction {value}"))),
    }
}

fn serialize_transport_mode(value: TransportMode) -> &'static str {
    match value {
        TransportMode::AdbOnly => "adb_only",
        TransportMode::WifiOnly => "wifi_only",
        TransportMode::Dual => "dual",
    }
}

fn parse_transport_mode(value: &str) -> Result<TransportMode, CheckpointError> {
    match value {
        "adb_only" => Ok(TransportMode::AdbOnly),
        "wifi_only" => Ok(TransportMode::WifiOnly),
        "dual" => Ok(TransportMode::Dual),
        _ => Err(CheckpointError::Parse(format!(
            "invalid transport mode {value}"
        ))),
    }
}

fn serialize_state(value: TaskState) -> &'static str {
    match value {
        TaskState::Pending => "pending",
        TaskState::Running => "running",
        TaskState::Paused => "paused",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Cancelled => "cancelled",
    }
}

fn parse_state(value: &str) -> Result<TaskState, CheckpointError> {
    match value {
        "pending" => Ok(TaskState::Pending),
        "running" => Ok(TaskState::Running),
        "paused" => Ok(TaskState::Paused),
        "completed" => Ok(TaskState::Completed),
        "failed" => Ok(TaskState::Failed),
        "cancelled" => Ok(TaskState::Cancelled),
        _ => Err(CheckpointError::Parse(format!(
            "invalid task state {value}"
        ))),
    }
}

fn serialize_lane(value: LaneAssignment) -> &'static str {
    match value {
        LaneAssignment::Adb => "adb",
        LaneAssignment::Wifi => "wifi",
    }
}

fn parse_lane(value: &str) -> Result<LaneAssignment, CheckpointError> {
    match value {
        "adb" => Ok(LaneAssignment::Adb),
        "wifi" => Ok(LaneAssignment::Wifi),
        _ => Err(CheckpointError::Parse(format!("invalid lane {value}"))),
    }
}

fn escape(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('|', "\\p")
}

fn unescape(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('p') => result.push('|'),
                Some('\\') => result.push('\\'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{CheckpointStore, initialize_checkpoint};
    use crate::models::{Direction, TaskConfig, TransferItem, TransportMode};
    use crate::scheduler::LaneAssignment;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_temp_dir() -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "nekotrans-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        let _ = fs::create_dir_all(&path);
        path
    }

    #[test]
    fn checkpoint_roundtrip_works() {
        let config = TaskConfig::new(
            "task-1",
            Direction::PcToAndroid,
            TransportMode::Dual,
            true,
            PathBuf::from("C:/Users/example/Documents"),
            "/sdcard/Backup",
        );
        let items = vec![TransferItem {
            relative_path: PathBuf::from("photos/cat.png"),
            size_bytes: 1234,
            modified_at_epoch_ms: 5,
            fingerprint: None,
        }];
        let mut entry = initialize_checkpoint(&config, &items);
        entry.checkpoint.files[0].completed_chunks = vec![0, 1];
        entry.checkpoint.files[0]
            .completed_chunk_lanes
            .insert(0, LaneAssignment::Adb);
        entry.checkpoint.files[0]
            .completed_chunk_lanes
            .insert(1, LaneAssignment::Wifi);
        entry.checkpoint.files[0].verification_digest = Some("abc123".to_string());
        entry.checkpoint.files[0].fingerprint_hex = Some("meta-digest".to_string());

        let store = CheckpointStore::new(unique_temp_dir());
        let path = store.persist(&entry).expect("persist should succeed");
        let loaded = store.load("task-1").expect("load should succeed");

        assert!(path.exists());
        assert_eq!(loaded, entry);
    }
}
