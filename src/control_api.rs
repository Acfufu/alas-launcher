//! Minimal HTTP/1.1 client for the ALAS control API (patch-injected).
//! Deliberately dependency-free: one-shot TcpStream per call, JSON bodies,
//! localhost only (no TLS — password/SSL degrades client-side via
//! deploy_config::ws_control_available, and the server also 401s).

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstanceState {
    pub name: String,
    pub state: u8,
}

/// Scheduler control direction (moved here from the deleted pywebio.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerAction {
    Start,
    Stop,
}

#[derive(Debug)]
pub enum ApiError {
    Locked,
    Http(u16),
    Io(std::io::Error),
    Protocol(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Locked => write!(f, "control API locked (password/SSL)"),
            ApiError::Http(code) => write!(f, "control API HTTP {code}"),
            ApiError::Io(e) => write!(f, "control API io: {e}"),
            ApiError::Protocol(m) => write!(f, "control API protocol: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}

const TIMEOUT: Duration = Duration::from_secs(5);

fn request(port: u16, method: &str, path: &str) -> Result<Vec<u8>, ApiError> {
    let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(ApiError::Io)?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(ApiError::Io)?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(ApiError::Io)?;
    let req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(ApiError::Io)?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(ApiError::Io)?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| ApiError::Protocol("no header/body separator".into()))?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| ApiError::Protocol("malformed status line".into()))?;
    match status {
        200 => Ok(body.as_bytes().to_vec()),
        401 => Err(ApiError::Locked),
        other => Err(ApiError::Http(other)),
    }
}

fn parse_instance(body: &[u8]) -> Result<InstanceState, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::Protocol(e.to_string()))
}

pub fn api_instances(port: u16) -> Result<Vec<InstanceState>, ApiError> {
    let body = request(port, "GET", "/api/alas/instances")?;
    serde_json::from_slice(&body).map_err(|e| ApiError::Protocol(e.to_string()))
}

pub fn api_scheduler_start(port: u16, name: &str) -> Result<InstanceState, ApiError> {
    let body = request(port, "POST", &format!("/api/alas/{name}/scheduler/start"))?;
    parse_instance(&body)
}

pub fn api_scheduler_stop(port: u16, name: &str) -> Result<InstanceState, ApiError> {
    let body = request(port, "POST", &format!("/api/alas/{name}/scheduler/stop"))?;
    parse_instance(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// Serve ONE canned HTTP response, capture the request line, then close.
    fn mock_server(response: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = sock.read(&mut buf).unwrap();
            tx.send(String::from_utf8_lossy(&buf[..n]).to_string()).unwrap();
            sock.write_all(response.as_bytes()).unwrap();
            let _ = sock.shutdown(std::net::Shutdown::Both);
        });
        (port, rx)
    }

    const OK_INSTANCES: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n[{\"name\":\"alas\",\"state\":1}]";
    const LOCKED: &str = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 18\r\n\r\n{\"error\":\"locked\"}";

    #[test]
    fn instances_parses_json_array() {
        let (port, _rx) = mock_server(OK_INSTANCES);
        let list = api_instances(port).unwrap();
        assert_eq!(list, vec![InstanceState { name: "alas".into(), state: 1 }]);
    }

    #[test]
    fn locked_returns_api_error_locked() {
        let (port, _rx) = mock_server(LOCKED);
        assert!(matches!(api_scheduler_start(port, "alas"), Err(ApiError::Locked)));
    }
}
