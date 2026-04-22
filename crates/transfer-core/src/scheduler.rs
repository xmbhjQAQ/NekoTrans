use crate::models::{
    ChunkDescriptor, TaskConfig, TransferItem, TransportMode, is_large_file, split_into_chunks,
};
use std::collections::{BTreeSet, HashMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneAssignment {
    Adb,
    Wifi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLease {
    pub lane: LaneAssignment,
    pub chunk: ChunkDescriptor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerDecision {
    Lease(ChunkLease),
    Idle,
}

#[derive(Debug)]
pub struct Scheduler {
    adb_queue: VecDeque<ChunkDescriptor>,
    wifi_queue: VecDeque<ChunkDescriptor>,
    inflight: HashMap<LaneAssignment, usize>,
    lane_bias: LaneAssignment,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self {
            adb_queue: VecDeque::new(),
            wifi_queue: VecDeque::new(),
            inflight: HashMap::from([
                (LaneAssignment::Adb, 0usize),
                (LaneAssignment::Wifi, 0usize),
            ]),
            lane_bias: LaneAssignment::Adb,
        }
    }
}

impl Scheduler {
    pub fn new(config: &TaskConfig, items: &[TransferItem]) -> Self {
        Self::new_with_completed(config, items, &[])
    }

    pub fn new_with_completed(
        config: &TaskConfig,
        items: &[TransferItem],
        completed_chunks_by_file: &[Vec<u32>],
    ) -> Self {
        let mut scheduler = Self {
            adb_queue: VecDeque::new(),
            wifi_queue: VecDeque::new(),
            inflight: HashMap::from([
                (LaneAssignment::Adb, 0usize),
                (LaneAssignment::Wifi, 0usize),
            ]),
            lane_bias: LaneAssignment::Adb,
        };

        for (file_index, item) in items.iter().enumerate() {
            let completed = completed_chunks_by_file
                .get(file_index)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<BTreeSet<_>>();
            let chunks = split_into_chunks(file_index, item.size_bytes, config.chunk_size_bytes)
                .into_iter()
                .filter(|chunk| !completed.contains(&chunk.chunk_index))
                .collect::<Vec<_>>();
            match config.transport_mode {
                TransportMode::AdbOnly => scheduler.adb_queue.extend(chunks),
                TransportMode::WifiOnly => scheduler.wifi_queue.extend(chunks),
                TransportMode::Dual => {
                    if is_large_file(item.size_bytes, config.small_file_threshold_bytes) {
                        for (index, chunk) in chunks.into_iter().enumerate() {
                            if index % 2 == 0 {
                                scheduler.adb_queue.push_back(chunk);
                            } else {
                                scheduler.wifi_queue.push_back(chunk);
                            }
                        }
                    } else {
                        match scheduler.lane_bias {
                            LaneAssignment::Adb => scheduler.adb_queue.extend(chunks),
                            LaneAssignment::Wifi => scheduler.wifi_queue.extend(chunks),
                        }
                        scheduler.flip_bias();
                    }
                }
            }
        }

        scheduler
    }

    pub fn lease_next(
        &mut self,
        config: &TaskConfig,
        preferred_lane: LaneAssignment,
    ) -> SchedulerDecision {
        let limit = config.max_in_flight_chunks_per_lane;
        if self.inflight_for(preferred_lane) >= limit {
            let fallback = self.other_lane(preferred_lane);
            if self.inflight_for(fallback) >= limit {
                return SchedulerDecision::Idle;
            }
            return self.try_lease(fallback);
        }

        self.try_lease(preferred_lane)
    }

    pub fn complete(&mut self, lease: ChunkLease) {
        if let Some(inflight) = self.inflight.get_mut(&lease.lane) {
            *inflight = inflight.saturating_sub(1);
        }
    }

    pub fn is_drained(&self) -> bool {
        self.adb_queue.is_empty()
            && self.wifi_queue.is_empty()
            && self.inflight.values().all(|value| *value == 0)
    }

    fn try_lease(&mut self, lane: LaneAssignment) -> SchedulerDecision {
        let queue = match lane {
            LaneAssignment::Adb => &mut self.adb_queue,
            LaneAssignment::Wifi => &mut self.wifi_queue,
        };

        if let Some(chunk) = queue.pop_front() {
            if let Some(inflight) = self.inflight.get_mut(&lane) {
                *inflight += 1;
            }
            SchedulerDecision::Lease(ChunkLease { lane, chunk })
        } else {
            let fallback = self.other_lane(lane);
            let fallback_queue = match fallback {
                LaneAssignment::Adb => &mut self.adb_queue,
                LaneAssignment::Wifi => &mut self.wifi_queue,
            };
            if let Some(chunk) = fallback_queue.pop_front() {
                if let Some(inflight) = self.inflight.get_mut(&fallback) {
                    *inflight += 1;
                }
                SchedulerDecision::Lease(ChunkLease {
                    lane: fallback,
                    chunk,
                })
            } else {
                SchedulerDecision::Idle
            }
        }
    }

    fn inflight_for(&self, lane: LaneAssignment) -> usize {
        *self.inflight.get(&lane).unwrap_or(&0)
    }

    fn other_lane(&self, lane: LaneAssignment) -> LaneAssignment {
        match lane {
            LaneAssignment::Adb => LaneAssignment::Wifi,
            LaneAssignment::Wifi => LaneAssignment::Adb,
        }
    }

    fn flip_bias(&mut self) {
        self.lane_bias = self.other_lane(self.lane_bias);
    }
}

#[cfg(test)]
mod tests {
    use super::{LaneAssignment, Scheduler, SchedulerDecision};
    use crate::models::{Direction, TaskConfig, TransferItem, TransportMode};
    use std::path::PathBuf;

    fn sample_item(name: &str, size_bytes: u64) -> TransferItem {
        TransferItem {
            relative_path: PathBuf::from(name),
            size_bytes,
            modified_at_epoch_ms: 1,
            fingerprint: None,
        }
    }

    #[test]
    fn dual_mode_splits_large_file_across_lanes() {
        let mut config = TaskConfig::new(
            "t1",
            Direction::PcToAndroid,
            TransportMode::Dual,
            false,
            PathBuf::from("C:/src"),
            "/sdcard/Backup",
        );
        config.chunk_size_bytes = 4;
        config.small_file_threshold_bytes = 8;

        let mut scheduler = Scheduler::new(&config, &[sample_item("movie.mkv", 16)]);
        let mut lanes = Vec::new();

        for _ in 0..4 {
            match scheduler.lease_next(&config, LaneAssignment::Adb) {
                SchedulerDecision::Lease(lease) => {
                    lanes.push(lease.lane);
                    scheduler.complete(lease);
                }
                SchedulerDecision::Idle => break,
            }
        }

        assert!(lanes.contains(&LaneAssignment::Adb));
        assert!(lanes.contains(&LaneAssignment::Wifi));
    }

    #[test]
    fn single_track_uses_requested_lane_only() {
        let config = TaskConfig::new(
            "t1",
            Direction::PcToAndroid,
            TransportMode::AdbOnly,
            false,
            PathBuf::from("C:/src"),
            "/sdcard/Backup",
        );

        let mut scheduler = Scheduler::new(&config, &[sample_item("doc.txt", 1024)]);
        match scheduler.lease_next(&config, LaneAssignment::Adb) {
            SchedulerDecision::Lease(lease) => assert_eq!(lease.lane, LaneAssignment::Adb),
            SchedulerDecision::Idle => panic!("scheduler should have emitted a chunk"),
        }
    }

    #[test]
    fn resume_skips_completed_chunks() {
        let mut config = TaskConfig::new(
            "t1",
            Direction::PcToAndroid,
            TransportMode::Dual,
            false,
            PathBuf::from("C:/src"),
            "/sdcard/Backup",
        );
        config.chunk_size_bytes = 4;
        config.small_file_threshold_bytes = 1;

        let mut scheduler =
            Scheduler::new_with_completed(&config, &[sample_item("movie.mkv", 16)], &[vec![0, 1]]);
        let mut seen_chunks = Vec::new();

        for _ in 0..4 {
            match scheduler.lease_next(&config, LaneAssignment::Adb) {
                SchedulerDecision::Lease(lease) => {
                    seen_chunks.push(lease.chunk.chunk_index);
                    scheduler.complete(lease);
                }
                SchedulerDecision::Idle => break,
            }
        }

        assert_eq!(seen_chunks, vec![2, 3]);
    }
}
