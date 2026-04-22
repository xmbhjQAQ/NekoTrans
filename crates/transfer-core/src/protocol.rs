use crate::models::{ChunkDescriptor, DeviceCapability, TaskId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceHello {
    pub device_id: String,
    pub device_name: String,
    pub capability: DeviceCapability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChunk {
    pub task_id: TaskId,
    pub session_id: String,
    pub lane: String,
    pub chunk: ChunkDescriptor,
    pub payload_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRequest {
    pub task_id: TaskId,
    pub relative_path: String,
    pub algorithm: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    StartTask {
        task_id: TaskId,
    },
    PauseTask {
        task_id: TaskId,
    },
    ResumeTask {
        task_id: TaskId,
    },
    CancelTask {
        task_id: TaskId,
    },
    ChunkAck {
        task_id: TaskId,
        chunk: ChunkDescriptor,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolMessage {
    Hello(DeviceHello),
    Control(ControlMessage),
    Chunk(FileChunk),
    Verify(VerifyRequest),
}
