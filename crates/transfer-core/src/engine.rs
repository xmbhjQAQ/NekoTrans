use crate::checkpoint::{CheckpointEntry, CheckpointStore, initialize_checkpoint};
use crate::inventory::expand_sources;
use crate::logging::{LogLevel, LogRecord, LogScope};
use crate::models::{
    ChunkDescriptor, Direction, FileFingerprint, TaskConfig, TaskId, TaskState, TransferItem,
    TransportMode, now_epoch_ms, split_into_chunks,
};
use crate::scheduler::{ChunkLease, LaneAssignment, Scheduler, SchedulerDecision};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineMetrics {
    pub total_bytes: u64,
    pub committed_bytes: u64,
    pub adb_bytes: u64,
    pub wifi_bytes: u64,
    pub completed_chunks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineTaskSnapshot {
    pub task_id: TaskId,
    pub state: TaskState,
    pub direction: Direction,
    pub transport_mode: TransportMode,
    pub verify_enabled: bool,
    pub total_files: usize,
    pub total_bytes: u64,
    pub committed_bytes: u64,
    pub adb_bytes: u64,
    pub wifi_bytes: u64,
    pub completed_chunks: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferUpdate {
    pub snapshot: EngineTaskSnapshot,
    pub new_logs: Vec<LogRecord>,
}

#[derive(Debug)]
pub enum TransferEngineError {
    UnknownTask(String),
    InvalidState(String),
    Inventory(String),
    Persistence(String),
}

impl std::fmt::Display for TransferEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTask(task_id) => write!(f, "unknown task {task_id}"),
            Self::InvalidState(message) => write!(f, "{message}"),
            Self::Inventory(message) => write!(f, "{message}"),
            Self::Persistence(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for TransferEngineError {}

#[derive(Debug)]
pub struct TransferEngine {
    checkpoint_store: CheckpointStore,
    tasks: BTreeMap<TaskId, EngineTask>,
}

#[derive(Debug)]
struct EngineTask {
    config: TaskConfig,
    items: Vec<TransferItem>,
    scheduler: Scheduler,
    checkpoint: CheckpointEntry,
    metrics: EngineMetrics,
    state: TaskState,
    last_error: Option<String>,
    log_cursor: usize,
    logs: Vec<LogRecord>,
}

impl TransferEngine {
    pub fn new(checkpoint_root: impl Into<PathBuf>) -> Self {
        Self {
            checkpoint_store: CheckpointStore::new(checkpoint_root),
            tasks: BTreeMap::new(),
        }
    }

    pub fn create_task(
        &mut self,
        config: TaskConfig,
        items: Vec<TransferItem>,
    ) -> Result<EngineTaskSnapshot, TransferEngineError> {
        let total_bytes = items.iter().map(|item| item.size_bytes).sum();
        let task_id = config.task_id.clone();
        let checkpoint = initialize_checkpoint(&config, &items);
        let scheduler = Scheduler::new(&config, &items);
        let mut task = EngineTask {
            config,
            items,
            scheduler,
            checkpoint,
            metrics: EngineMetrics {
                total_bytes,
                committed_bytes: 0,
                adb_bytes: 0,
                wifi_bytes: 0,
                completed_chunks: 0,
            },
            state: TaskState::Pending,
            last_error: None,
            log_cursor: 0,
            logs: Vec::new(),
        };
        task.logs.push(
            LogRecord::new(LogLevel::Info, LogScope::Audit, "task created").with_task_id(&task_id),
        );
        task.persist(&self.checkpoint_store)?;
        let snapshot = task.snapshot();
        self.tasks.insert(task_id, task);
        Ok(snapshot)
    }

    pub fn create_task_from_paths(
        &mut self,
        config: TaskConfig,
        selected_paths: &[PathBuf],
    ) -> Result<EngineTaskSnapshot, TransferEngineError> {
        let items = expand_sources(&config.source_root, selected_paths)
            .map_err(|err| TransferEngineError::Inventory(err.to_string()))?;
        self.create_task(config, items)
    }

    pub fn create_or_recover_task_from_paths(
        &mut self,
        config: TaskConfig,
        selected_paths: &[PathBuf],
    ) -> Result<EngineTaskSnapshot, TransferEngineError> {
        if let Some(task) = self.tasks.get(&config.task_id) {
            return Ok(task.snapshot());
        }

        if self.checkpoint_store.load(&config.task_id).is_ok() {
            return self.recover_task(&config.task_id);
        }

        self.create_task_from_paths(config, selected_paths)
    }

    pub fn ensure_demo_task(&mut self) -> Result<EngineTaskSnapshot, TransferEngineError> {
        if let Some(task) = self.tasks.get("demo-task") {
            return Ok(task.snapshot());
        }

        let config = TaskConfig::new(
            "demo-task",
            Direction::PcToAndroid,
            TransportMode::Dual,
            false,
            PathBuf::from("C:/Users/demo/Documents"),
            "/sdcard/Backup",
        );

        let items = vec![
            TransferItem {
                relative_path: PathBuf::from("media/archive-01.bin"),
                size_bytes: 96 * 1024 * 1024,
                modified_at_epoch_ms: now_epoch_ms(),
                fingerprint: None,
            },
            TransferItem {
                relative_path: PathBuf::from("photos/set-a/cat-01.jpg"),
                size_bytes: 6 * 1024 * 1024,
                modified_at_epoch_ms: now_epoch_ms(),
                fingerprint: None,
            },
            TransferItem {
                relative_path: PathBuf::from("photos/set-a/cat-02.jpg"),
                size_bytes: 7 * 1024 * 1024,
                modified_at_epoch_ms: now_epoch_ms(),
                fingerprint: None,
            },
        ];

        self.create_task(config, items)
    }

    pub fn tick_task(
        &mut self,
        task_id: &str,
        steps: usize,
    ) -> Result<TransferUpdate, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;

        if matches!(
            task.state,
            TaskState::Completed | TaskState::Cancelled | TaskState::Failed
        ) {
            return Err(TransferEngineError::InvalidState(format!(
                "task {task_id} is already terminal"
            )));
        }

        if task.state == TaskState::Paused {
            let snapshot = task.snapshot();
            return Ok(TransferUpdate {
                snapshot,
                new_logs: task.drain_new_logs(),
            });
        }

        task.state = TaskState::Running;
        task.checkpoint.checkpoint.state = TaskState::Running;

        for step in 0..steps.max(1) {
            let preferred_lane = if step % 2 == 0 {
                LaneAssignment::Adb
            } else {
                LaneAssignment::Wifi
            };

            match task.scheduler.lease_next(&task.config, preferred_lane) {
                SchedulerDecision::Lease(lease) => {
                    task.commit_chunk(lease.chunk, lease.lane, false);
                    task.scheduler.complete(lease);
                }
                SchedulerDecision::Idle => break,
            }
        }

        if task.scheduler.is_drained() {
            task.state = TaskState::Completed;
            task.checkpoint.checkpoint.state = TaskState::Completed;
            task.logs.push(
                LogRecord::new(LogLevel::Info, LogScope::Audit, "task completed")
                    .with_task_id(&task.config.task_id),
            );
        }

        task.persist(&self.checkpoint_store)?;
        let snapshot = task.snapshot();
        Ok(TransferUpdate {
            snapshot,
            new_logs: task.drain_new_logs(),
        })
    }

    pub fn pause_task(&mut self, task_id: &str) -> Result<EngineTaskSnapshot, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;
        if matches!(
            task.state,
            TaskState::Completed | TaskState::Cancelled | TaskState::Failed
        ) {
            return Err(TransferEngineError::InvalidState(format!(
                "task {task_id} can no longer be paused"
            )));
        }
        task.state = TaskState::Paused;
        task.checkpoint.checkpoint.state = TaskState::Paused;
        task.logs.push(
            LogRecord::new(LogLevel::Warn, LogScope::Audit, "task paused").with_task_id(task_id),
        );
        task.persist(&self.checkpoint_store)?;
        Ok(task.snapshot())
    }

    pub fn resume_task(
        &mut self,
        task_id: &str,
    ) -> Result<EngineTaskSnapshot, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;
        if task.state != TaskState::Paused {
            return Err(TransferEngineError::InvalidState(format!(
                "task {task_id} is not paused"
            )));
        }
        task.state = TaskState::Running;
        task.checkpoint.checkpoint.state = TaskState::Running;
        task.logs.push(
            LogRecord::new(LogLevel::Info, LogScope::Audit, "task resumed").with_task_id(task_id),
        );
        task.persist(&self.checkpoint_store)?;
        Ok(task.snapshot())
    }

    pub fn cancel_task(
        &mut self,
        task_id: &str,
    ) -> Result<EngineTaskSnapshot, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;
        if matches!(task.state, TaskState::Completed | TaskState::Cancelled) {
            return Ok(task.snapshot());
        }
        task.state = TaskState::Cancelled;
        task.checkpoint.checkpoint.state = TaskState::Cancelled;
        task.logs.push(
            LogRecord::new(LogLevel::Warn, LogScope::Audit, "task cancelled").with_task_id(task_id),
        );
        task.persist(&self.checkpoint_store)?;
        Ok(task.snapshot())
    }

    pub fn retry_task(&mut self, task_id: &str) -> Result<EngineTaskSnapshot, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;
        if matches!(task.state, TaskState::Completed | TaskState::Cancelled) {
            return Err(TransferEngineError::InvalidState(format!(
                "task {task_id} cannot be retried from terminal state"
            )));
        }
        task.state = TaskState::Paused;
        task.checkpoint.checkpoint.state = TaskState::Paused;
        task.last_error = None;
        task.logs.push(
            LogRecord::new(LogLevel::Info, LogScope::Audit, "task staged for retry")
                .with_task_id(task_id),
        );
        task.persist(&self.checkpoint_store)?;
        Ok(task.snapshot())
    }

    pub fn record_real_chunk_commit(
        &mut self,
        task_id: &str,
        chunk: ChunkDescriptor,
        lane: LaneAssignment,
        was_skipped: bool,
    ) -> Result<TransferUpdate, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;

        task.ensure_can_record_real_progress(task_id)?;
        task.state = TaskState::Running;
        task.checkpoint.checkpoint.state = TaskState::Running;
        task.commit_chunk(chunk, lane, was_skipped);
        task.mark_completed_if_drained();
        task.persist(&self.checkpoint_store)?;

        Ok(TransferUpdate {
            snapshot: task.snapshot(),
            new_logs: task.drain_new_logs(),
        })
    }

    pub fn lease_real_chunk(
        &mut self,
        task_id: &str,
        preferred_lane: LaneAssignment,
    ) -> Result<Option<ChunkLease>, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;

        task.ensure_can_record_real_progress(task_id)?;
        if task.state == TaskState::Paused {
            return Ok(None);
        }
        task.state = TaskState::Running;
        task.checkpoint.checkpoint.state = TaskState::Running;

        match task.scheduler.lease_next(&task.config, preferred_lane) {
            SchedulerDecision::Lease(lease) => {
                task.logs.push(
                    LogRecord::new(LogLevel::Debug, LogScope::Transfer, "real chunk leased")
                        .with_task_id(&task.config.task_id)
                        .with_chunk_id(&format!("{}", lease.chunk.chunk_index))
                        .with_lane(match lease.lane {
                            LaneAssignment::Adb => "adb",
                            LaneAssignment::Wifi => "wifi",
                        }),
                );
                Ok(Some(lease))
            }
            SchedulerDecision::Idle => Ok(None),
        }
    }

    pub fn complete_real_chunk_lease(
        &mut self,
        task_id: &str,
        lease: ChunkLease,
        was_skipped: bool,
    ) -> Result<TransferUpdate, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;

        task.ensure_can_record_real_progress(task_id)?;
        task.state = TaskState::Running;
        task.checkpoint.checkpoint.state = TaskState::Running;
        task.commit_chunk(lease.chunk, lease.lane, was_skipped);
        task.scheduler.complete(lease);
        task.mark_completed_if_drained();
        task.persist(&self.checkpoint_store)?;

        Ok(TransferUpdate {
            snapshot: task.snapshot(),
            new_logs: task.drain_new_logs(),
        })
    }

    pub fn record_real_file_complete(
        &mut self,
        task_id: &str,
        file_index: usize,
        lane: LaneAssignment,
        was_skipped: bool,
    ) -> Result<TransferUpdate, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;

        task.ensure_can_record_real_progress(task_id)?;
        task.state = TaskState::Running;
        task.checkpoint.checkpoint.state = TaskState::Running;
        let file_size = task.items.get(file_index).ok_or_else(|| {
            TransferEngineError::InvalidState(format!("unknown file {file_index}"))
        })?;
        let file_size = file_size.size_bytes;
        for chunk in split_into_chunks(file_index, file_size, task.config.chunk_size_bytes) {
            task.commit_chunk(chunk, lane, was_skipped);
        }
        task.mark_completed_if_drained();
        task.persist(&self.checkpoint_store)?;

        Ok(TransferUpdate {
            snapshot: task.snapshot(),
            new_logs: task.drain_new_logs(),
        })
    }

    pub fn record_task_failure(
        &mut self,
        task_id: &str,
        message: impl Into<String>,
    ) -> Result<EngineTaskSnapshot, TransferEngineError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;
        let message = message.into();
        task.state = TaskState::Failed;
        task.checkpoint.checkpoint.state = TaskState::Failed;
        task.last_error = Some(message.clone());
        task.logs.push(
            LogRecord::new(LogLevel::Error, LogScope::Transfer, message)
                .with_task_id(&task.config.task_id),
        );
        task.persist(&self.checkpoint_store)?;
        Ok(task.snapshot())
    }

    pub fn snapshots(&self) -> Vec<EngineTaskSnapshot> {
        self.tasks.values().map(EngineTask::snapshot).collect()
    }

    pub fn recoverable_tasks(&self) -> Result<Vec<String>, TransferEngineError> {
        let paths = self
            .checkpoint_store
            .list()
            .map_err(|err| TransferEngineError::Persistence(err.to_string()))?;
        let mut task_ids = Vec::new();

        for path in paths {
            if let Some(task_id) = path.file_stem().and_then(|value| value.to_str()) {
                let checkpoint = self
                    .checkpoint_store
                    .load(task_id)
                    .map_err(|err| TransferEngineError::Persistence(err.to_string()))?;
                if !matches!(
                    checkpoint.checkpoint.state,
                    TaskState::Completed | TaskState::Cancelled
                ) {
                    task_ids.push(task_id.to_string());
                }
            }
        }

        Ok(task_ids)
    }

    pub fn recover_task(
        &mut self,
        task_id: &str,
    ) -> Result<EngineTaskSnapshot, TransferEngineError> {
        if let Some(task) = self.tasks.get(task_id) {
            return Ok(task.snapshot());
        }

        let checkpoint = self
            .checkpoint_store
            .load(task_id)
            .map_err(|err| TransferEngineError::Persistence(err.to_string()))?;
        let items = checkpoint
            .checkpoint
            .files
            .iter()
            .map(|file| TransferItem {
                relative_path: file.relative_path.clone(),
                size_bytes: file.size_bytes,
                modified_at_epoch_ms: file.modified_at_epoch_ms,
                fingerprint: file
                    .fingerprint_hex
                    .clone()
                    .map(|hex_digest| FileFingerprint {
                        algorithm: "size-mtime",
                        hex_digest,
                    }),
            })
            .collect::<Vec<_>>();
        let completed_chunks_by_file = checkpoint
            .checkpoint
            .files
            .iter()
            .map(|file| file.completed_chunks.clone())
            .collect::<Vec<_>>();
        let completed_chunk_lanes_by_file = checkpoint
            .checkpoint
            .files
            .iter()
            .map(|file| file.completed_chunk_lanes.clone())
            .collect::<Vec<_>>();
        let scheduler =
            Scheduler::new_with_completed(&checkpoint.config, &items, &completed_chunks_by_file);
        let metrics = rebuild_metrics(
            &checkpoint.config,
            &items,
            &completed_chunks_by_file,
            &completed_chunk_lanes_by_file,
        );
        let resumed_state = match checkpoint.checkpoint.state {
            TaskState::Completed => TaskState::Completed,
            TaskState::Cancelled => TaskState::Cancelled,
            TaskState::Failed | TaskState::Paused | TaskState::Running | TaskState::Pending => {
                TaskState::Paused
            }
        };

        let mut task = EngineTask {
            config: checkpoint.config.clone(),
            items,
            scheduler,
            checkpoint,
            metrics,
            state: resumed_state,
            last_error: None,
            log_cursor: 0,
            logs: vec![
                LogRecord::new(
                    LogLevel::Info,
                    LogScope::Audit,
                    "task recovered from checkpoint",
                )
                .with_task_id(task_id),
            ],
        };
        task.persist(&self.checkpoint_store)?;
        let snapshot = task.snapshot();
        self.tasks.insert(task_id.to_string(), task);
        Ok(snapshot)
    }

    pub fn checkpoint_entry(&self, task_id: &str) -> Result<CheckpointEntry, TransferEngineError> {
        if let Some(task) = self.tasks.get(task_id) {
            return Ok(task.checkpoint.clone());
        }

        self.checkpoint_store
            .load(task_id)
            .map_err(|err| TransferEngineError::Persistence(err.to_string()))
    }

    pub fn log_lines(&self, task_id: &str) -> Result<Vec<String>, TransferEngineError> {
        let task = self
            .tasks
            .get(task_id)
            .ok_or_else(|| TransferEngineError::UnknownTask(task_id.to_string()))?;
        Ok(task.logs.iter().map(LogRecord::to_json_line).collect())
    }
}

impl EngineTask {
    fn snapshot(&self) -> EngineTaskSnapshot {
        EngineTaskSnapshot {
            task_id: self.config.task_id.clone(),
            state: self.state,
            direction: self.config.direction,
            transport_mode: self.config.transport_mode,
            verify_enabled: self.config.verify_enabled,
            total_files: self.items.len(),
            total_bytes: self.metrics.total_bytes,
            committed_bytes: self.metrics.committed_bytes,
            adb_bytes: self.metrics.adb_bytes,
            wifi_bytes: self.metrics.wifi_bytes,
            completed_chunks: self.metrics.completed_chunks,
            last_error: self.last_error.clone(),
        }
    }

    fn drain_new_logs(&mut self) -> Vec<LogRecord> {
        let new_logs = self.logs[self.log_cursor..].to_vec();
        self.log_cursor = self.logs.len();
        new_logs
    }

    fn persist(&mut self, store: &CheckpointStore) -> Result<(), TransferEngineError> {
        self.checkpoint.checkpoint.state = self.state;
        self.checkpoint.checkpoint.updated_at_epoch_ms = now_epoch_ms();
        store
            .persist(&self.checkpoint)
            .map_err(|err| TransferEngineError::Persistence(err.to_string()))?;
        Ok(())
    }

    fn ensure_can_record_real_progress(&self, task_id: &str) -> Result<(), TransferEngineError> {
        if matches!(self.state, TaskState::Cancelled | TaskState::Failed) {
            return Err(TransferEngineError::InvalidState(format!(
                "task {task_id} can no longer record progress"
            )));
        }
        Ok(())
    }

    fn commit_chunk(
        &mut self,
        chunk: ChunkDescriptor,
        lane: LaneAssignment,
        was_skipped: bool,
    ) -> bool {
        let Some(file_checkpoint) = self.checkpoint.checkpoint.files.get_mut(chunk.file_index)
        else {
            return false;
        };
        if file_checkpoint
            .completed_chunks
            .contains(&chunk.chunk_index)
        {
            return false;
        }

        file_checkpoint.completed_chunks.push(chunk.chunk_index);
        file_checkpoint.completed_chunks.sort_unstable();
        file_checkpoint
            .completed_chunk_lanes
            .insert(chunk.chunk_index, lane);
        self.metrics.committed_bytes += chunk.length;
        self.metrics.completed_chunks += 1;
        match lane {
            LaneAssignment::Adb => self.metrics.adb_bytes += chunk.length,
            LaneAssignment::Wifi => self.metrics.wifi_bytes += chunk.length,
        }

        let file_path = self.items[chunk.file_index]
            .relative_path
            .to_string_lossy()
            .to_string();
        let message = if was_skipped {
            "real chunk skipped from checkpoint"
        } else {
            "real chunk committed"
        };
        self.logs.push(
            LogRecord::new(LogLevel::Info, LogScope::Transfer, message)
                .with_task_id(&self.config.task_id)
                .with_file_path(file_path)
                .with_chunk_id(&format!("{}", chunk.chunk_index))
                .with_lane(match lane {
                    LaneAssignment::Adb => "adb",
                    LaneAssignment::Wifi => "wifi",
                }),
        );
        true
    }

    fn mark_completed_if_drained(&mut self) {
        let all_done = self.items.iter().enumerate().all(|(file_index, item)| {
            let expected =
                split_into_chunks(file_index, item.size_bytes, self.config.chunk_size_bytes).len();
            let actual = self
                .checkpoint
                .checkpoint
                .files
                .get(file_index)
                .map(|file| file.completed_chunks.len())
                .unwrap_or_default();
            actual >= expected
        });

        if all_done && self.state != TaskState::Completed {
            self.state = TaskState::Completed;
            self.checkpoint.checkpoint.state = TaskState::Completed;
            self.logs.push(
                LogRecord::new(LogLevel::Info, LogScope::Audit, "task completed")
                    .with_task_id(&self.config.task_id),
            );
        }
    }
}

fn rebuild_metrics(
    config: &TaskConfig,
    items: &[TransferItem],
    completed_chunks_by_file: &[Vec<u32>],
    completed_chunk_lanes_by_file: &[BTreeMap<u32, LaneAssignment>],
) -> EngineMetrics {
    let total_bytes = items.iter().map(|item| item.size_bytes).sum();
    let mut committed_bytes = 0u64;
    let mut adb_bytes = 0u64;
    let mut wifi_bytes = 0u64;
    let mut completed_chunks = 0u64;

    for (file_index, item) in items.iter().enumerate() {
        let chunks = split_into_chunks(file_index, item.size_bytes, config.chunk_size_bytes);
        let completed = completed_chunks_by_file
            .get(file_index)
            .cloned()
            .unwrap_or_default();
        let completed_chunk_lanes = completed_chunk_lanes_by_file
            .get(file_index)
            .cloned()
            .unwrap_or_default();

        for chunk_index in completed {
            if let Some(chunk) = chunks.iter().find(|chunk| chunk.chunk_index == chunk_index) {
                committed_bytes += chunk.length;
                completed_chunks += 1;

                match completed_chunk_lanes.get(&chunk_index).copied() {
                    Some(LaneAssignment::Adb) => adb_bytes += chunk.length,
                    Some(LaneAssignment::Wifi) => wifi_bytes += chunk.length,
                    None => match config.transport_mode {
                        TransportMode::AdbOnly => adb_bytes += chunk.length,
                        TransportMode::WifiOnly => wifi_bytes += chunk.length,
                        TransportMode::Dual => {
                            if chunk.chunk_index % 2 == 0 {
                                adb_bytes += chunk.length;
                            } else {
                                wifi_bytes += chunk.length;
                            }
                        }
                    },
                }
            }
        }
    }

    EngineMetrics {
        total_bytes,
        committed_bytes,
        adb_bytes,
        wifi_bytes,
        completed_chunks,
    }
}

#[cfg(test)]
mod tests {
    use super::TransferEngine;
    use crate::models::{Direction, TaskConfig, TaskState, TransportMode};
    use crate::scheduler::LaneAssignment;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_temp_dir() -> std::path::PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "nekotrans-engine-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        let _ = fs::create_dir_all(&path);
        path
    }

    #[test]
    fn engine_can_advance_demo_task() {
        let mut engine = TransferEngine::new(unique_temp_dir());
        let snapshot = engine.ensure_demo_task().expect("demo task should exist");
        assert_eq!(snapshot.state, TaskState::Pending);

        let update = engine
            .tick_task("demo-task", 3)
            .expect("tick should succeed");
        assert!(update.snapshot.committed_bytes > 0);
        assert!(!update.new_logs.is_empty());
    }

    #[test]
    fn pause_and_resume_roundtrip() {
        let mut engine = TransferEngine::new(unique_temp_dir());
        engine.ensure_demo_task().expect("demo task should exist");

        let paused = engine.pause_task("demo-task").expect("pause should work");
        assert_eq!(paused.state, TaskState::Paused);

        let resumed = engine.resume_task("demo-task").expect("resume should work");
        assert_eq!(resumed.state, TaskState::Running);
    }

    #[test]
    fn engine_can_create_task_from_real_paths() {
        let root = unique_temp_dir();
        fs::create_dir_all(root.join("docs")).expect("create docs directory");
        fs::write(root.join("docs").join("readme.txt"), b"hello").expect("write file");

        let mut engine = TransferEngine::new(root.join(".state"));
        let config = TaskConfig::new(
            "task-from-paths",
            Direction::PcToAndroid,
            TransportMode::AdbOnly,
            false,
            root.clone(),
            "/sdcard/Backup",
        );

        let snapshot = engine
            .create_task_from_paths(config, &[PathBuf::from("docs")])
            .expect("task creation should succeed");

        assert_eq!(snapshot.total_files, 1);
        assert!(snapshot.total_bytes > 0);
    }

    #[test]
    fn recoverable_task_list_includes_persisted_entries() {
        let mut engine = TransferEngine::new(unique_temp_dir());
        engine.ensure_demo_task().expect("demo task should exist");
        let recoverables = engine
            .recoverable_tasks()
            .expect("recovery enumeration should work");
        assert!(recoverables.iter().any(|task_id| task_id == "demo-task"));
    }

    #[test]
    fn recovered_task_keeps_progress_and_resumes_from_remaining_chunks() {
        let root = unique_temp_dir();
        let mut engine = TransferEngine::new(&root);
        engine.ensure_demo_task().expect("demo task should exist");
        let first_update = engine
            .tick_task("demo-task", 4)
            .expect("tick should persist progress");
        let committed_before = first_update.snapshot.committed_bytes;
        drop(engine);

        let mut recovered_engine = TransferEngine::new(&root);
        let recovered = recovered_engine
            .recover_task("demo-task")
            .expect("recovery should work");
        assert_eq!(recovered.state, TaskState::Paused);
        assert_eq!(recovered.committed_bytes, committed_before);

        let resumed = recovered_engine
            .resume_task("demo-task")
            .expect("resume should work after recovery");
        assert_eq!(resumed.state, TaskState::Running);

        let progressed = recovered_engine
            .tick_task("demo-task", 1)
            .expect("should continue remaining chunks");
        assert!(progressed.snapshot.committed_bytes > committed_before);
    }

    #[test]
    fn records_real_adb_chunk_progress_into_checkpoint() {
        let root = unique_temp_dir();
        fs::create_dir_all(root.join("docs")).expect("create docs directory");
        fs::write(root.join("docs").join("readme.txt"), b"hello world").expect("write file");

        let mut engine = TransferEngine::new(root.join(".state"));
        let mut config = TaskConfig::new(
            "real-adb-task",
            Direction::PcToAndroid,
            TransportMode::AdbOnly,
            false,
            root.join("docs"),
            "/sdcard/NekotransDocs",
        );
        config.chunk_size_bytes = 4;

        engine
            .create_task_from_paths(config, &[PathBuf::from(".")])
            .expect("task creation should succeed");
        let update = engine
            .record_real_chunk_commit(
                "real-adb-task",
                crate::models::ChunkDescriptor {
                    file_index: 0,
                    chunk_index: 0,
                    offset: 0,
                    length: 4,
                },
                crate::scheduler::LaneAssignment::Adb,
                false,
            )
            .expect("real chunk commit should persist");

        assert_eq!(update.snapshot.committed_bytes, 4);
        assert_eq!(update.snapshot.adb_bytes, 4);
        assert_eq!(update.snapshot.completed_chunks, 1);
        assert!(!update.new_logs.is_empty());

        drop(engine);
        let mut recovered = TransferEngine::new(root.join(".state"));
        let snapshot = recovered
            .recover_task("real-adb-task")
            .expect("real task should recover");
        assert_eq!(snapshot.committed_bytes, 4);
        assert_eq!(snapshot.adb_bytes, 4);
    }

    #[test]
    fn real_workers_can_lease_and_complete_dual_chunks() {
        let root = unique_temp_dir();
        fs::write(root.join("movie.bin"), vec![7u8; 16]).expect("write file");

        let mut engine = TransferEngine::new(root.join(".state"));
        let mut config = TaskConfig::new(
            "dual-real-workers",
            Direction::PcToAndroid,
            TransportMode::Dual,
            false,
            root.clone(),
            "/sdcard/Nekotrans",
        );
        config.chunk_size_bytes = 4;
        config.small_file_threshold_bytes = 1;

        engine
            .create_task_from_paths(config, &[PathBuf::from("movie.bin")])
            .expect("task creation should succeed");

        let adb_lease = engine
            .lease_real_chunk("dual-real-workers", crate::scheduler::LaneAssignment::Adb)
            .expect("lease should succeed")
            .expect("adb lease should exist");
        let wifi_lease = engine
            .lease_real_chunk("dual-real-workers", crate::scheduler::LaneAssignment::Wifi)
            .expect("lease should succeed")
            .expect("wifi lease should exist");
        assert_eq!(adb_lease.lane, crate::scheduler::LaneAssignment::Adb);
        assert_eq!(wifi_lease.lane, crate::scheduler::LaneAssignment::Wifi);

        let first = engine
            .complete_real_chunk_lease("dual-real-workers", adb_lease, false)
            .expect("complete adb lease");
        assert_eq!(first.snapshot.adb_bytes, 4);

        let second = engine
            .complete_real_chunk_lease("dual-real-workers", wifi_lease, false)
            .expect("complete wifi lease");
        assert_eq!(second.snapshot.wifi_bytes, 4);
        assert_eq!(second.snapshot.completed_chunks, 2);
    }

    #[test]
    fn recovered_dual_task_preserves_real_lane_metrics() {
        let root = unique_temp_dir();
        fs::write(root.join("tiny-a.bin"), vec![1u8; 2]).expect("write tiny a");
        fs::write(root.join("tiny-b.bin"), vec![2u8; 2]).expect("write tiny b");

        let mut engine = TransferEngine::new(root.join(".state"));
        let mut config = TaskConfig::new(
            "dual-recovery-lanes",
            Direction::PcToAndroid,
            TransportMode::Dual,
            false,
            root.clone(),
            "/sdcard/Nekotrans",
        );
        config.chunk_size_bytes = 4;
        config.small_file_threshold_bytes = 1024;

        engine
            .create_task_from_paths(
                config,
                &[PathBuf::from("tiny-a.bin"), PathBuf::from("tiny-b.bin")],
            )
            .expect("task creation should succeed");

        let first = engine
            .lease_real_chunk("dual-recovery-lanes", LaneAssignment::Adb)
            .expect("first lease should succeed")
            .expect("first lease should exist");
        let second = engine
            .lease_real_chunk("dual-recovery-lanes", LaneAssignment::Wifi)
            .expect("second lease should succeed")
            .expect("second lease should exist");

        engine
            .complete_real_chunk_lease("dual-recovery-lanes", first, false)
            .expect("complete first");
        let update = engine
            .complete_real_chunk_lease("dual-recovery-lanes", second, false)
            .expect("complete second");
        assert_eq!(update.snapshot.adb_bytes, 2);
        assert_eq!(update.snapshot.wifi_bytes, 2);
        drop(engine);

        let mut recovered = TransferEngine::new(root.join(".state"));
        let snapshot = recovered
            .recover_task("dual-recovery-lanes")
            .expect("task should recover");
        assert_eq!(snapshot.adb_bytes, 2);
        assert_eq!(snapshot.wifi_bytes, 2);
        assert_eq!(snapshot.committed_bytes, 4);
    }
}
