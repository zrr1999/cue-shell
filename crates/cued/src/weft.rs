use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Thin local client for weft's bootstrap HTTP-over-Unix-socket surface.
///
/// Batch 1 only relies on:
/// - `GET /discover` as a reachability/capability probe
/// - `POST /sessions/prepare` for initial `:ask` / `:spawn` forwarding
///
/// Follow-up prompts and cancellation are intentionally surfaced as explicit
/// unsupported errors until weft grows first-class northbound endpoints for
/// those operations.
#[derive(Debug, Clone)]
pub struct WeftClient {
    socket_path: PathBuf,
}

impl WeftClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn discover(&self) -> Result<CapabilityDiscovery, WeftClientError> {
        self.request(Method::Get, "/discover", Option::<&()>::None)
            .await
    }

    pub async fn prepare_session(
        &self,
        request: &SessionPrepareRequest,
    ) -> Result<PreparedSessionResponse, WeftClientError> {
        self.request(Method::Post, "/sessions/prepare", Some(request))
            .await
    }

    async fn request<T: Serialize + ?Sized, R: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<&T>,
    ) -> Result<R, WeftClientError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|source| WeftClientError::Connect {
                path: self.socket_path.clone(),
                source,
            })?;

        let body = match body {
            Some(value) => serde_json::to_vec(value).map_err(WeftClientError::Serialize)?,
            None => Vec::new(),
        };
        let request = method.render(path, &body);
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(WeftClientError::Io)?;
        stream.shutdown().await.map_err(WeftClientError::Io)?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(WeftClientError::Io)?;

        let parsed = HttpResponse::parse(&response)?;
        if !(200..300).contains(&parsed.status_code) {
            return Err(WeftClientError::Http {
                status_code: parsed.status_code,
                message: extract_error_message(&parsed.body),
            });
        }

        serde_json::from_slice(&parsed.body).map_err(WeftClientError::Deserialize)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionPrepareRequest {
    pub agent: String,
    pub workspace: WorkspaceRef,
    pub input: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceRef {
    pub root: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreparedSessionResponse {
    pub protocol: String,
    #[serde(default)]
    pub surface: Option<String>,
    pub request: PreparedSessionRequest,
    pub session: PreparedSessionState,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreparedSessionRequest {
    pub agent: String,
    pub workspace: WorkspaceRefResponse,
    pub input: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceRefResponse {
    pub root: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreparedSessionState {
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CapabilityDiscovery {
    pub protocol: String,
    #[serde(default)]
    pub capabilities: Vec<RuntimeCapability>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeCapability {
    pub id: String,
    pub summary: String,
}

#[derive(Debug)]
pub enum WeftClientError {
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },
    Io(std::io::Error),
    Serialize(serde_json::Error),
    Deserialize(serde_json::Error),
    Protocol(String),
    Http {
        status_code: u16,
        message: String,
    },
}

impl std::fmt::Display for WeftClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect { path, source } => {
                write!(f, "connect to weft socket {}: {source}", path.display())
            }
            Self::Io(source) => write!(f, "weft transport I/O error: {source}"),
            Self::Serialize(source) => write!(f, "serialize weft request: {source}"),
            Self::Deserialize(source) => write!(f, "decode weft response: {source}"),
            Self::Protocol(message) => write!(f, "invalid weft response: {message}"),
            Self::Http {
                status_code,
                message,
            } => write!(f, "weft returned HTTP {status_code}: {message}"),
        }
    }
}

impl std::error::Error for WeftClientError {}

#[derive(Debug, Clone, Copy)]
enum Method {
    Get,
    Post,
}

impl Method {
    fn render(self, path: &str, body: &[u8]) -> String {
        let method = match self {
            Self::Get => "GET",
            Self::Post => "POST",
        };

        if matches!(self, Self::Post) {
            format!(
                "{method} {path} HTTP/1.1\r\nHost: weft\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(body)
            )
        } else {
            format!("{method} {path} HTTP/1.1\r\nHost: weft\r\n\r\n")
        }
    }
}

struct HttpResponse {
    status_code: u16,
    body: Vec<u8>,
}

impl HttpResponse {
    fn parse(bytes: &[u8]) -> Result<Self, WeftClientError> {
        let Some((head, body)) = bytes.split_once_str(b"\r\n\r\n") else {
            return Err(WeftClientError::Protocol(
                "missing HTTP header terminator".into(),
            ));
        };
        let head = std::str::from_utf8(head).map_err(|error| {
            WeftClientError::Protocol(format!("invalid UTF-8 headers: {error}"))
        })?;
        let status_line = head
            .lines()
            .next()
            .ok_or_else(|| WeftClientError::Protocol("missing HTTP status line".into()))?;
        let mut parts = status_line.split_whitespace();
        let version = parts
            .next()
            .ok_or_else(|| WeftClientError::Protocol("missing HTTP version".into()))?;
        if !version.starts_with("HTTP/") {
            return Err(WeftClientError::Protocol(format!(
                "unexpected HTTP version `{version}`"
            )));
        }
        let status_code = parts
            .next()
            .ok_or_else(|| WeftClientError::Protocol("missing HTTP status code".into()))?
            .parse::<u16>()
            .map_err(|error| {
                WeftClientError::Protocol(format!("invalid HTTP status code: {error}"))
            })?;

        Ok(Self {
            status_code,
            body: body.to_vec(),
        })
    }
}

trait SplitOnceExt {
    fn split_once_str(&self, needle: &[u8]) -> Option<(&[u8], &[u8])>;
}

impl SplitOnceExt for [u8] {
    fn split_once_str(&self, needle: &[u8]) -> Option<(&[u8], &[u8])> {
        self.windows(needle.len())
            .position(|window| window == needle)
            .map(|index| (&self[..index], &self[index + needle.len()..]))
    }
}

fn extract_error_message(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|json| {
            json.get("message")
                .and_then(|value| value.as_str())
                .or_else(|| json.get("error").and_then(|value| value.as_str()))
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn prepare_session_posts_expected_request() {
        let socket_path = unique_test_socket_path("prepare");
        let captures = Arc::new(Mutex::new(Vec::<String>::new()));
        let captures_clone = Arc::clone(&captures);
        let accept_count = Arc::new(AtomicUsize::new(0));
        let accept_count_clone = Arc::clone(&accept_count);
        let listener = bind_test_listener(&socket_path).await;

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            accept_count_clone.fetch_add(1, Ordering::Relaxed);
            let mut request = Vec::new();
            stream
                .read_to_end(&mut request)
                .await
                .expect("read request");
            captures_clone
                .lock()
                .await
                .push(String::from_utf8_lossy(&request).into_owned());
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"protocol\":\"v1\",\"request\":{\"agent\":\"copilot\",\"workspace\":{\"root\":\"/workspace\"},\"input\":\"hello\"},\"session\":{\"status\":\"pending\"}}",
                )
                .await
                .expect("write response");
        });

        let client = WeftClient::new(&socket_path);
        let prepared = client
            .prepare_session(&SessionPrepareRequest {
                agent: "copilot".into(),
                workspace: WorkspaceRef {
                    root: "/workspace".into(),
                },
                input: "hello".into(),
            })
            .await
            .expect("prepare session");

        server.await.expect("server task");

        assert_eq!(prepared.protocol, "v1");
        assert_eq!(prepared.surface, None);
        assert_eq!(prepared.session.status, "pending");
        assert_eq!(accept_count.load(Ordering::Relaxed), 1);
        let captures = captures.lock().await;
        let request = captures.first().expect("captured request");
        assert!(request.starts_with("POST /sessions/prepare HTTP/1.1\r\n"));
        assert!(request.contains("\"agent\":\"copilot\""));
        assert!(request.contains("\"root\":\"/workspace\""));
        cleanup_test_socket(&socket_path);
    }

    #[tokio::test]
    async fn discover_uses_get_request() {
        let socket_path = unique_test_socket_path("discover");
        let captures = Arc::new(Mutex::new(Vec::<String>::new()));
        let captures_clone = Arc::clone(&captures);
        let listener = bind_test_listener(&socket_path).await;

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = Vec::new();
            stream
                .read_to_end(&mut request)
                .await
                .expect("read request");
            captures_clone
                .lock()
                .await
                .push(String::from_utf8_lossy(&request).into_owned());
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"protocol\":\"v1\",\"capabilities\":[{\"id\":\"team.manage\",\"summary\":\"Manage teams\"}]}",
                )
                .await
                .expect("write response");
        });

        let client = WeftClient::new(&socket_path);
        let discovery = client.discover().await.expect("discover");

        server.await.expect("server task");

        assert_eq!(discovery.protocol, "v1");
        assert_eq!(discovery.capabilities.len(), 1);
        assert_eq!(discovery.capabilities[0].id, "team.manage");
        let captures = captures.lock().await;
        let request = captures.first().expect("captured request");
        assert!(request.starts_with("GET /discover HTTP/1.1\r\n"));
        cleanup_test_socket(&socket_path);
    }

    async fn bind_test_listener(socket_path: &Path) -> UnixListener {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).expect("create test socket dir");
        }
        let _ = std::fs::remove_file(socket_path);
        UnixListener::bind(socket_path).expect("bind test listener")
    }

    fn unique_test_socket_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        PathBuf::from("target")
            .join("test-sockets")
            .join(format!("w-{name}-{nanos}.sock"))
    }

    fn cleanup_test_socket(socket_path: &Path) {
        let _ = std::fs::remove_file(socket_path);
    }
}
