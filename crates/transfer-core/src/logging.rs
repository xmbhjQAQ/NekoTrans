use crate::models::now_epoch_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogScope {
    Audit,
    Transfer,
    Device,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub ts_epoch_ms: u128,
    pub level: LogLevel,
    pub scope: LogScope,
    pub task_id: Option<String>,
    pub file_path: Option<String>,
    pub chunk_id: Option<String>,
    pub lane: Option<String>,
    pub message: String,
}

impl LogRecord {
    pub fn new(level: LogLevel, scope: LogScope, message: impl Into<String>) -> Self {
        Self {
            ts_epoch_ms: now_epoch_ms(),
            level,
            scope,
            task_id: None,
            file_path: None,
            chunk_id: None,
            lane: None,
            message: message.into(),
        }
    }

    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn with_file_path(mut self, file_path: impl Into<String>) -> Self {
        self.file_path = Some(file_path.into());
        self
    }

    pub fn with_lane(mut self, lane: impl Into<String>) -> Self {
        self.lane = Some(lane.into());
        self
    }

    pub fn with_chunk_id(mut self, chunk_id: impl Into<String>) -> Self {
        self.chunk_id = Some(chunk_id.into());
        self
    }

    pub fn to_json_line(&self) -> String {
        format!(
            "{{\"ts_epoch_ms\":{},\"level\":\"{}\",\"scope\":\"{}\",\"task_id\":{},\"file_path\":{},\"chunk_id\":{},\"lane\":{},\"message\":\"{}\"}}",
            self.ts_epoch_ms,
            self.level.as_str(),
            self.scope.as_str(),
            optional_json_string(self.task_id.as_deref()),
            optional_json_string(self.file_path.as_deref()),
            optional_json_string(self.chunk_id.as_deref()),
            optional_json_string(self.lane.as_deref()),
            escape_json(&self.message),
        )
    }
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl LogScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audit => "audit",
            Self::Transfer => "transfer",
            Self::Device => "device",
            Self::Protocol => "protocol",
        }
    }
}

fn optional_json_string(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{}\"", escape_json(value)),
        None => "null".to_string(),
    }
}

fn escape_json(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::{LogLevel, LogRecord, LogScope};

    #[test]
    fn log_record_serializes_to_json_line() {
        let output = LogRecord::new(LogLevel::Info, LogScope::Transfer, "chunk committed")
            .with_task_id("task-1")
            .with_lane("adb")
            .to_json_line();

        assert!(output.contains("\"task_id\":\"task-1\""));
        assert!(output.contains("\"lane\":\"adb\""));
        assert!(output.contains("\"scope\":\"transfer\""));
        assert!(output.contains("\"chunk_id\":null"));
    }
}
