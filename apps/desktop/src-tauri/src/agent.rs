use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub const DEFAULT_AGENT_PORT: u16 = 38997;
const DEFAULT_TIMEOUT_MS: u64 = 5000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEndpoint {
    pub host: String,
    pub port: u16,
}

impl AgentEndpoint {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    fn socket_addr(&self) -> Result<SocketAddr, AgentClientError> {
        format!("{}:{}", self.host, self.port)
            .parse()
            .map_err(|err| AgentClientError::InvalidEndpoint(format!("{err}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReply {
    pub endpoint: AgentEndpoint,
    pub command: String,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBinaryReply {
    pub endpoint: AgentEndpoint,
    pub command: String,
    pub header: String,
    pub payload: Vec<u8>,
}

pub struct BinaryChunkFrame<'a> {
    pub chunk_index: u32,
    pub offset: u64,
    pub payload: &'a [u8],
}

#[derive(Debug)]
pub enum AgentClientError {
    InvalidEndpoint(String),
    Io(String),
}

impl std::fmt::Display for AgentClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEndpoint(message) => write!(f, "invalid agent endpoint: {message}"),
            Self::Io(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for AgentClientError {}

pub fn hello(host: &str) -> Result<AgentReply, AgentClientError> {
    send_command(&AgentEndpoint::new(host, DEFAULT_AGENT_PORT), "HELLO")
}

pub fn ping(host: &str) -> Result<AgentReply, AgentClientError> {
    send_command(&AgentEndpoint::new(host, DEFAULT_AGENT_PORT), "PING")
}

pub fn start_task(host: &str, task_id: &str) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!("START_TASK {}", sanitize_command_arg(task_id)),
    )
}

pub fn set_target_root(host: &str, target_root: &str) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!("SET_TARGET_ROOT {}", sanitize_path_arg(target_root)),
    )
}

pub fn task_snapshot(host: &str) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        "TASK_SNAPSHOT",
    )
}

pub fn pause_task(host: &str) -> Result<AgentReply, AgentClientError> {
    send_command(&AgentEndpoint::new(host, DEFAULT_AGENT_PORT), "PAUSE_TASK")
}

pub fn resume_task(host: &str) -> Result<AgentReply, AgentClientError> {
    send_command(&AgentEndpoint::new(host, DEFAULT_AGENT_PORT), "RESUME_TASK")
}

pub fn start_file(
    host: &str,
    relative_path: &str,
    size_bytes: u64,
) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!(
            "START_FILE {} {}",
            sanitize_path_arg(relative_path),
            size_bytes
        ),
    )
}

pub fn complete_file(host: &str, relative_path: &str) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!("COMPLETE_FILE {}", sanitize_path_arg(relative_path)),
    )
}

pub fn ack_chunk(host: &str, chunk_index: u32) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!("CHUNK_ACK {chunk_index}"),
    )
}

pub fn chunk_status(
    host: &str,
    relative_path: &str,
    chunk_index: u32,
    offset: u64,
    length: u64,
) -> Result<AgentReply, AgentClientError> {
    send_chunk_status(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        relative_path,
        chunk_index,
        offset,
        length,
    )
}

pub fn push_chunk_payload(
    host: &str,
    relative_path: &str,
    chunk_index: u32,
    offset: u64,
    payload: &[u8],
) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!(
            "PUSH_CHUNK {} {} {} {}",
            sanitize_path_arg(relative_path),
            chunk_index,
            offset,
            base64_encode(payload)
        ),
    )
}

pub fn push_chunk_binary(
    host: &str,
    relative_path: &str,
    chunk_index: u32,
    offset: u64,
    payload: &[u8],
) -> Result<AgentReply, AgentClientError> {
    send_binary_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!(
            "PUSH_CHUNK_BIN {} {} {} {}",
            sanitize_path_arg(relative_path),
            chunk_index,
            offset,
            payload.len()
        ),
        payload,
    )
}

pub fn push_chunk_batch_binary(
    host: &str,
    relative_path: &str,
    chunks: &[BinaryChunkFrame<'_>],
) -> Result<AgentReply, AgentClientError> {
    send_binary_chunk_batch_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!(
            "PUSH_CHUNK_BATCH_BIN {} {}",
            sanitize_path_arg(relative_path),
            chunks.len()
        ),
        chunks,
    )
}

pub fn push_file_bundle_binary(
    host: &str,
    bundle_id: &str,
    manifest: &[u8],
    payload: &[u8],
) -> Result<AgentReply, AgentClientError> {
    let mut body = Vec::with_capacity(manifest.len() + payload.len());
    body.extend_from_slice(manifest);
    body.extend_from_slice(payload);
    send_binary_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!(
            "PUSH_FILE_BUNDLE_BIN {} {} {}",
            sanitize_command_arg(bundle_id),
            manifest.len(),
            payload.len()
        ),
        &body,
    )
}

pub fn file_snapshot(host: &str) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        "FILE_SNAPSHOT",
    )
}

pub fn pull_chunk_payload(
    host: &str,
    relative_path: &str,
    offset: u64,
    length: u64,
) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!(
            "PULL_CHUNK {} {} {}",
            sanitize_path_arg(relative_path),
            offset,
            length
        ),
    )
}

pub fn pull_chunk_binary(
    host: &str,
    relative_path: &str,
    offset: u64,
    length: u64,
) -> Result<AgentBinaryReply, AgentClientError> {
    send_binary_reply_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!(
            "PULL_CHUNK_BIN {} {} {}",
            sanitize_path_arg(relative_path),
            offset,
            length
        ),
    )
}

pub fn stat_file(host: &str, relative_path: &str) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!("STAT_FILE {}", sanitize_path_arg(relative_path)),
    )
}

pub fn verify_file(host: &str, relative_path: &str) -> Result<AgentReply, AgentClientError> {
    send_command_with_timeout(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        &format!("VERIFY_FILE {} BLAKE3", sanitize_path_arg(relative_path)),
        Duration::from_millis(DEFAULT_TIMEOUT_MS.max(1_800_000)),
    )
}

pub fn log_snapshot(host: &str) -> Result<AgentReply, AgentClientError> {
    send_command(
        &AgentEndpoint::new(host, DEFAULT_AGENT_PORT),
        "LOG_SNAPSHOT",
    )
}

pub fn send_command(
    endpoint: &AgentEndpoint,
    command: &str,
) -> Result<AgentReply, AgentClientError> {
    let timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS);
    send_command_with_timeout(endpoint, command, timeout)
}

fn send_command_with_timeout(
    endpoint: &AgentEndpoint,
    command: &str,
    timeout: Duration,
) -> Result<AgentReply, AgentClientError> {
    let mut stream = TcpStream::connect_timeout(&endpoint.socket_addr()?, timeout)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .write_all(format!("{}\n", command.trim()).as_bytes())
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .flush()
        .map_err(|err| AgentClientError::Io(err.to_string()))?;

    let mut payload = String::new();
    stream
        .read_to_string(&mut payload)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;

    Ok(AgentReply {
        endpoint: endpoint.clone(),
        command: command.to_string(),
        payload: payload.trim().to_string(),
    })
}

fn send_binary_command(
    endpoint: &AgentEndpoint,
    command: &str,
    payload: &[u8],
) -> Result<AgentReply, AgentClientError> {
    let timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS);
    let mut stream = TcpStream::connect_timeout(&endpoint.socket_addr()?, timeout)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .write_all(format!("{}\n", command.trim()).as_bytes())
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .write_all(payload)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .flush()
        .map_err(|err| AgentClientError::Io(err.to_string()))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;

    Ok(AgentReply {
        endpoint: endpoint.clone(),
        command: command.to_string(),
        payload: response.trim().to_string(),
    })
}

fn send_binary_chunk_batch_command(
    endpoint: &AgentEndpoint,
    command: &str,
    chunks: &[BinaryChunkFrame<'_>],
) -> Result<AgentReply, AgentClientError> {
    let timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS.max(120_000));
    let mut stream = TcpStream::connect_timeout(&endpoint.socket_addr()?, timeout)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .write_all(format!("{}\n", command.trim()).as_bytes())
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    for chunk in chunks {
        stream
            .write_all(&chunk.chunk_index.to_be_bytes())
            .map_err(|err| AgentClientError::Io(err.to_string()))?;
        stream
            .write_all(&chunk.offset.to_be_bytes())
            .map_err(|err| AgentClientError::Io(err.to_string()))?;
        let length = u32::try_from(chunk.payload.len())
            .map_err(|_| AgentClientError::Io("chunk payload too large".to_string()))?;
        stream
            .write_all(&length.to_be_bytes())
            .map_err(|err| AgentClientError::Io(err.to_string()))?;
        stream
            .write_all(chunk.payload)
            .map_err(|err| AgentClientError::Io(err.to_string()))?;
    }
    stream
        .flush()
        .map_err(|err| AgentClientError::Io(err.to_string()))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;

    Ok(AgentReply {
        endpoint: endpoint.clone(),
        command: command.to_string(),
        payload: response.trim().to_string(),
    })
}

fn send_binary_reply_command(
    endpoint: &AgentEndpoint,
    command: &str,
) -> Result<AgentBinaryReply, AgentClientError> {
    let timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS.max(120_000));
    let mut stream = TcpStream::connect_timeout(&endpoint.socket_addr()?, timeout)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .write_all(format!("{}\n", command.trim()).as_bytes())
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    stream
        .flush()
        .map_err(|err| AgentClientError::Io(err.to_string()))?;

    let mut reader = BufReader::new(stream);
    let mut header = String::new();
    reader
        .read_line(&mut header)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;
    let header = header.trim().to_string();
    let length = binary_reply_length(&header)?;
    let mut payload = vec![0u8; length];
    reader
        .read_exact(&mut payload)
        .map_err(|err| AgentClientError::Io(err.to_string()))?;

    Ok(AgentBinaryReply {
        endpoint: endpoint.clone(),
        command: command.to_string(),
        header,
        payload,
    })
}

fn binary_reply_length(header: &str) -> Result<usize, AgentClientError> {
    let value = serde_json::from_str::<serde_json::Value>(header)
        .map_err(|err| AgentClientError::Io(format!("invalid binary reply header: {err}")))?;
    if value.get("type").and_then(serde_json::Value::as_str) == Some("Error") {
        return Err(AgentClientError::Io(header.to_string()));
    }
    let length = value
        .get("length")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            AgentClientError::Io(format!("binary reply header missing length: {header}"))
        })?;
    usize::try_from(length)
        .map_err(|_| AgentClientError::Io(format!("binary reply too large: {length}")))
}

fn send_chunk_status(
    endpoint: &AgentEndpoint,
    relative_path: &str,
    chunk_index: u32,
    offset: u64,
    length: u64,
) -> Result<AgentReply, AgentClientError> {
    send_command(
        endpoint,
        &format!(
            "CHUNK_STATUS {} {} {} {}",
            sanitize_path_arg(relative_path),
            chunk_index,
            offset,
            length
        ),
    )
}

fn sanitize_command_arg(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .collect::<String>()
}

fn sanitize_path_arg(value: &str) -> String {
    encode_path_arg(value)
}

pub fn encode_path_arg(value: &str) -> String {
    let normalized = value.replace('\\', "/");
    let mut encoded = String::with_capacity(normalized.len());
    for byte in normalized.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'-' | b'_' | b'/') {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        let triple = ((first as u32) << 16) | ((second as u32) << 8) | third as u32;

        output.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() >= 2 {
            output.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() == 3 {
            output.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::{
        AgentEndpoint, BinaryChunkFrame, send_binary_chunk_batch_command, send_binary_command,
        send_binary_reply_command, send_chunk_status, send_command,
    };
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn sends_line_command_and_reads_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local listener");
        let port = listener.local_addr().expect("local addr").port();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut command = String::new();
            reader.read_line(&mut command).expect("read command");
            assert_eq!(command.trim(), "PING");
            stream
                .write_all(b"{\"type\":\"Pong\",\"protocol_version\":\"0.1\"}\n")
                .expect("write reply");
        });

        let reply = send_command(&AgentEndpoint::new("127.0.0.1", port), "PING")
            .expect("agent command should succeed");
        assert_eq!(
            reply.payload,
            "{\"type\":\"Pong\",\"protocol_version\":\"0.1\"}"
        );
        handle.join().expect("server thread should finish");
    }

    #[test]
    fn sanitizes_start_task_command_argument() {
        assert_eq!(super::sanitize_command_arg("task-01_ok"), "task-01_ok");
        assert_eq!(super::sanitize_command_arg("task 01;rm"), "task01rm");
    }

    #[test]
    fn encodes_file_path_command_argument() {
        assert_eq!(
            super::sanitize_path_arg("photos\\cat 01.jpg"),
            "photos/cat%2001.jpg"
        );
        assert_eq!(
            super::sanitize_path_arg("../坏 name's.bin"),
            "../%E5%9D%8F%20name%27s.bin"
        );
    }

    #[test]
    fn encodes_base64_payloads() {
        assert_eq!(super::base64_encode(b""), "");
        assert_eq!(super::base64_encode(b"f"), "Zg==");
        assert_eq!(super::base64_encode(b"fo"), "Zm8=");
        assert_eq!(super::base64_encode(b"foo"), "Zm9v");
        assert_eq!(super::base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn sends_chunk_status_command_with_encoded_path() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local listener");
        let port = listener.local_addr().expect("local addr").port();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut command = String::new();
            reader.read_line(&mut command).expect("read command");
            assert_eq!(
                command.trim(),
                "CHUNK_STATUS photos/cat%2001.jpg 7 1024 4096"
            );
            stream
                .write_all(b"{\"status\":\"committed\",\"relative_path\":\"photos/cat01.jpg\",\"chunk_index\":7}\n")
                .expect("write reply");
        });

        let reply = send_chunk_status(
            &AgentEndpoint::new("127.0.0.1", port),
            "photos\\cat 01.jpg",
            7,
            1024,
            4096,
        )
        .expect("chunk status command should succeed");
        assert_eq!(
            reply.command,
            "CHUNK_STATUS photos/cat%2001.jpg 7 1024 4096"
        );
        assert_eq!(
            reply.payload,
            "{\"status\":\"committed\",\"relative_path\":\"photos/cat01.jpg\",\"chunk_index\":7}"
        );
        handle.join().expect("server thread should finish");
    }

    #[test]
    fn sends_binary_chunk_command_and_raw_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local listener");
        let port = listener.local_addr().expect("local addr").port();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut command = String::new();
            reader.read_line(&mut command).expect("read command");
            assert_eq!(command.trim(), "PUSH_CHUNK_BIN photos/cat01.jpg 7 1024 5");
            let mut payload = [0u8; 5];
            reader.read_exact(&mut payload).expect("read payload");
            assert_eq!(&payload, b"hello");
            stream
                .write_all(b"{\"type\":\"ChunkAck\",\"relative_path\":\"photos/cat01.jpg\",\"chunk_index\":7,\"status\":\"written\"}\n")
                .expect("write reply");
        });

        let reply = send_binary_command(
            &AgentEndpoint::new("127.0.0.1", port),
            "PUSH_CHUNK_BIN photos/cat01.jpg 7 1024 5",
            b"hello",
        )
        .expect("binary push should succeed");
        assert_eq!(reply.command, "PUSH_CHUNK_BIN photos/cat01.jpg 7 1024 5");
        assert!(reply.payload.contains("\"status\":\"written\""));
        handle.join().expect("server thread should finish");
    }

    #[test]
    fn sends_binary_chunk_batch_command_and_framed_payloads() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local listener");
        let port = listener.local_addr().expect("local addr").port();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut command = String::new();
            reader.read_line(&mut command).expect("read command");
            assert_eq!(command.trim(), "PUSH_CHUNK_BATCH_BIN photos/cat%2001.jpg 2");

            let mut first_header = [0u8; 16];
            reader
                .read_exact(&mut first_header)
                .expect("read first header");
            assert_eq!(
                u32::from_be_bytes(first_header[0..4].try_into().unwrap()),
                1
            );
            assert_eq!(
                u64::from_be_bytes(first_header[4..12].try_into().unwrap()),
                8
            );
            assert_eq!(
                u32::from_be_bytes(first_header[12..16].try_into().unwrap()),
                5
            );
            let mut first_payload = [0u8; 5];
            reader
                .read_exact(&mut first_payload)
                .expect("read first payload");
            assert_eq!(&first_payload, b"hello");

            let mut second_header = [0u8; 16];
            reader
                .read_exact(&mut second_header)
                .expect("read second header");
            assert_eq!(
                u32::from_be_bytes(second_header[0..4].try_into().unwrap()),
                3
            );
            assert_eq!(
                u64::from_be_bytes(second_header[4..12].try_into().unwrap()),
                24
            );
            assert_eq!(
                u32::from_be_bytes(second_header[12..16].try_into().unwrap()),
                5
            );
            let mut second_payload = [0u8; 5];
            reader
                .read_exact(&mut second_payload)
                .expect("read second payload");
            assert_eq!(&second_payload, b"world");

            stream
                .write_all(b"{\"type\":\"ChunkBatchAck\",\"status\":\"batch_written\"}\n")
                .expect("write reply");
        });

        let frames = [
            BinaryChunkFrame {
                chunk_index: 1,
                offset: 8,
                payload: b"hello",
            },
            BinaryChunkFrame {
                chunk_index: 3,
                offset: 24,
                payload: b"world",
            },
        ];
        let reply = send_binary_chunk_batch_command(
            &AgentEndpoint::new("127.0.0.1", port),
            "PUSH_CHUNK_BATCH_BIN photos/cat%2001.jpg 2",
            &frames,
        )
        .expect("binary batch push should succeed");
        assert!(reply.payload.contains("\"status\":\"batch_written\""));
        handle.join().expect("server thread should finish");
    }

    #[test]
    fn reads_binary_chunk_reply_header_and_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local listener");
        let port = listener.local_addr().expect("local addr").port();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut command = String::new();
            reader.read_line(&mut command).expect("read command");
            assert_eq!(command.trim(), "PULL_CHUNK_BIN photos/cat%2001.jpg 1024 5");
            stream
                .write_all(
                    b"{\"type\":\"ChunkPayloadBin\",\"relative_path\":\"photos/cat 01.jpg\",\"offset\":1024,\"length\":5}\nhello",
                )
                .expect("write binary reply");
        });

        let reply = send_binary_reply_command(
            &AgentEndpoint::new("127.0.0.1", port),
            "PULL_CHUNK_BIN photos/cat%2001.jpg 1024 5",
        )
        .expect("binary pull should succeed");
        assert!(reply.header.contains("\"type\":\"ChunkPayloadBin\""));
        assert_eq!(reply.payload, b"hello");
        handle.join().expect("server thread should finish");
    }
}
