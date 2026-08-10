//! ALAS task data source for the tray menu's task-list section.
//!
//! Two-tier, fail-safe strategy: prefer the live ALAS webui HTTP API, falling
//! back to the repo's `config/*.yaml` task files.
//!
//! Tier 1 — [`fetch_tasks`] tries each [`CANDIDATE_PATHS`] endpoint on
//! `127.0.0.1:<port>` and returns the first whose payload parses to a
//! non-empty task list (see [`parse_tasks_json`]).
//!
//! Tier 2 — [`parse_tasks_config`] reads the ALAS repo's `config/*.yaml`
//! task files (plus `config/scheduler.yaml` when present).
//!
//! All network I/O is bounded by a caller-supplied timeout applied to both
//! `connect_timeout` and `set_read_timeout`, so the tray poll thread can
//! never hang (Metis MAJOR-4).
//!
//! Human-gated note: the bundled ALAS payload is NOT part of this dev repo
//! (it is copied in manually at build time — see README "构建与完整 payload
//! 组装"), so the real webui endpoint names cannot be verified here.
//! [`CANDIDATE_PATHS`] is therefore provisional; the confirmation procedure
//! is documented in `.omo/evidence/task-5-alas-tray-menu.md`.

use std::{
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    time::Duration,
};

use anyhow::{anyhow, Result};

/// A single ALAS scheduler task as shown (read-only) in the tray menu.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Task {
    pub name: String,
    pub enabled: bool,
    pub running: bool,
}

/// Provisional webui endpoints tried in order by [`fetch_tasks`].
///
/// These are best-guess conventions, NOT confirmed against the real ALAS
/// webui (payload not bundled in this repo — see module doc). Confirm with
/// the procedure in `.omo/evidence/task-5-alas-tray-menu.md` and update this
/// const + that doc once a real payload is available.
pub const CANDIDATE_PATHS: &[&str] = &["/api/tasks", "/api/scheduler", "/tasks", "/api/task/list"];

/// Minimal hand-rolled HTTP/1.1 GET. No new dependencies.
///
/// Bounded end-to-end: `TcpStream::connect_timeout` for the connect phase and
/// `set_read_timeout`/`set_write_timeout` for the I/O phases, so a dead or
/// silent server can never block the caller (the tray poll thread) forever.
///
/// Response handling: status line must start with `HTTP/1.1 200`; headers and
/// body are split at `\r\n\r\n`; when a `Content-Length` header is present the
/// body must contain at least that many bytes (else `Err("truncated...")`)
/// and exactly that many are returned; otherwise the body is the remainder of
/// the stream read to EOF. Chunked transfer encoding is not supported
/// (assumption — see evidence doc).
pub fn http_get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<String> {
    let address: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| anyhow!("invalid address {host}:{port}: {e}"))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|e| anyhow!("connect to {host}:{port} failed: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| anyhow!("set_read_timeout failed: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| anyhow!("set_write_timeout failed: {e}"))?;

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| anyhow!("write request failed: {e}"))?;

    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(anyhow!("read timed out after {timeout:?}"));
            }
            Err(e) => return Err(anyhow!("read failed: {e}")),
        }
    }

    let text = String::from_utf8(bytes).map_err(|_| anyhow!("response is not valid UTF-8"))?;
    parse_http_response(&text)
}

/// Split an HTTP response into its status line / headers and extract the body.
fn parse_http_response(text: &str) -> Result<String> {
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response: no header/body separator"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    if !status_line.starts_with("HTTP/1.1 200") {
        return Err(anyhow!("unexpected status line: {status_line:?}"));
    }
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find_map(|(key, value)| {
            if key.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        });
    match content_length {
        Some(cl) => {
            if body.len() < cl {
                return Err(anyhow!("truncated: expected {cl} bytes, got {}", body.len()));
            }
            Ok(body[..cl].to_string())
        }
        None => Ok(body.to_string()),
    }
}

/// Tolerant JSON task-list parser.
///
/// Accepts either a top-level array of task objects, or a JSON object that
/// carries the task array under one of the common keys `tasks`, `data`,
/// `scheduler` (first match wins). Per object:
/// - name: first of `name` | `task` | `title` (default: empty string)
/// - enabled: first of `enabled` | `enable` | `status`; string `status`
///   values `enabled`/`running`/`on` count as true (default: false)
/// - running: first of `running` | `status`; string `status` values
///   `running`/`active` count as true (default: false)
///
/// Non-object items are skipped. Malformed JSON is an `Err`; valid JSON of
/// any other shape yields an empty list (callers fall back to config).
pub fn parse_tasks_json(s: &str) -> Result<Vec<Task>> {
    let value: serde_json::Value =
        serde_json::from_str(s).map_err(|e| anyhow!("invalid task JSON: {e}"))?;
    let array = match &value {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(o) => {
            let mut found = None;
            for key in ["tasks", "data", "scheduler"] {
                if let Some(serde_json::Value::Array(a)) = o.get(key) {
                    found = Some(a);
                    break;
                }
            }
            match found {
                Some(a) => a,
                None => return Ok(Vec::new()),
            }
        }
        _ => return Ok(Vec::new()),
    };
    Ok(array.iter().filter_map(task_from_json).collect())
}

fn task_from_json(v: &serde_json::Value) -> Option<Task> {
    let obj = v.as_object()?;
    let name = ["name", "task", "title"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
        .map(String::from)
        .unwrap_or_default();
    let status = obj.get("status").and_then(|v| v.as_str());
    let enabled = obj
        .get("enabled")
        .and_then(|v| v.as_bool())
        .or_else(|| obj.get("enable").and_then(|v| v.as_bool()))
        .or_else(|| status.map(|s| matches!(s, "enabled" | "running" | "on")))
        .unwrap_or(false);
    let running = obj
        .get("running")
        .and_then(|v| v.as_bool())
        .or_else(|| status.map(|s| matches!(s, "running" | "active")))
        .unwrap_or(false);
    Some(Task {
        name,
        enabled,
        running,
    })
}

/// Fallback data source: read the ALAS repo's `config/*.yaml` task files.
///
/// Reads top-level `.yaml` files under `dir/config`, excluding `deploy.yaml`
/// (launcher config, not a task) and `scheduler.yaml` (handled separately).
/// For each standalone task file: task name = file stem; `enabled` = first of
/// `enabled` | `enable` | `Scheduler.Enable` | `Scheduler.enable` in the
/// YAML, defaulting to **true** (a task file existing means the task exists).
/// `config/scheduler.yaml`, when present, contributes entries from either a
/// mapping (`task-name: enable-flag`) or a sequence of objects (`name` +
/// `enable`/`enabled`), each defaulting to enabled.
///
/// `running` is always false: the launcher cannot know ALAS runtime state
/// from files — runtime state only comes from the webui API path.
///
/// Missing `config/` directory or no task files → `Ok(vec![])` (empty, not
/// an error). Malformed/unreadable individual files are skipped tolerantly so
/// one bad file cannot kill the whole menu.
pub fn parse_tasks_config(dir: &Path) -> Result<Vec<Task>> {
    let config_dir = dir.join("config");
    let entries = match std::fs::read_dir(&config_dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };

    let mut tasks = Vec::new();
    let mut scheduler_yaml: Option<std::path::PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        let Some(stem) = file_name.strip_suffix(".yaml") else {
            continue;
        };
        if stem == "deploy" {
            continue;
        }
        if stem == "scheduler" {
            scheduler_yaml = Some(path);
            continue;
        }
        let enabled = read_yaml(&path)
            .and_then(|v| lookup_enabled(&v))
            .unwrap_or(true);
        tasks.push(Task {
            name: stem.to_string(),
            enabled,
            running: false,
        });
    }

    if let Some(path) = scheduler_yaml {
        if let Some(value) = read_yaml(&path) {
            match value {
                serde_yaml::Value::Mapping(map) => {
                    for (key, val) in map {
                        let Some(name) = key.as_str().map(String::from) else {
                            continue;
                        };
                        tasks.push(Task {
                            name,
                            enabled: enabled_from_value(&val).unwrap_or(true),
                            running: false,
                        });
                    }
                }
                serde_yaml::Value::Sequence(seq) => {
                    for item in seq {
                        if item.as_mapping().is_none() {
                            continue;
                        }
                        tasks.push(Task {
                            name: name_from_value(&item),
                            enabled: enabled_from_value(&item).unwrap_or(true),
                            running: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    Ok(tasks)
}

/// Read + parse a YAML file; `None` on any error (tolerant skip).
fn read_yaml(path: &Path) -> Option<serde_yaml::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&text).ok()
}

/// `enabled` lookup for a standalone task file:
/// `enabled` | `enable` | `Scheduler.Enable` | `Scheduler.enable` (booleans).
fn lookup_enabled(v: &serde_yaml::Value) -> Option<bool> {
    for key in ["enabled", "enable"] {
        if let Some(b) = v.get(key).and_then(|x| x.as_bool()) {
            return Some(b);
        }
    }
    if let Some(scheduler) = v.get("Scheduler") {
        for key in ["Enable", "enable"] {
            if let Some(b) = scheduler.get(key).and_then(|x| x.as_bool()) {
                return Some(b);
            }
        }
    }
    None
}

/// `enabled` flag from a scheduler.yaml entry: a bare bool, or an object
/// carrying `enable`/`enabled`/`Enable`.
fn enabled_from_value(v: &serde_yaml::Value) -> Option<bool> {
    match v {
        serde_yaml::Value::Bool(b) => Some(*b),
        serde_yaml::Value::Mapping(_) => ["enable", "enabled", "Enable"]
            .iter()
            .find_map(|key| v.get(*key).and_then(|x| x.as_bool())),
        _ => None,
    }
}

/// Task name from a scheduler.yaml sequence item: `name` | `task` | `title`.
fn name_from_value(v: &serde_yaml::Value) -> String {
    ["name", "task", "title"]
        .iter()
        .find_map(|key| v.get(*key).and_then(|x| x.as_str()))
        .map(String::from)
        .unwrap_or_default()
}

/// Dispatcher: webui API first, `config/*.yaml` fallback.
///
/// Tries every [`CANDIDATE_PATHS`] entry via [`http_get`] on `127.0.0.1:port`
/// and returns the first whose body parses to a **non-empty** task list
/// (guards against an API that returns 200 with an empty/error payload while
/// the config files have real tasks). If every candidate fails or is empty,
/// returns [`parse_tasks_config`]'s result (which is `Ok(vec![])` — never an
/// error — when the payload has no task data at all).
pub fn fetch_tasks(port: u16, alas_dir: &Path, timeout: Duration) -> Result<Vec<Task>> {
    for path in CANDIDATE_PATHS {
        match http_get("127.0.0.1", port, path, timeout) {
            Ok(body) => {
                if let Ok(tasks) = parse_tasks_json(&body) {
                    if !tasks.is_empty() {
                        return Ok(tasks);
                    }
                }
            }
            Err(_) => continue,
        }
    }
    parse_tasks_config(alas_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    // ---- fixtures ---------------------------------------------------------

    /// Array fixture: 3 tasks with mixed name keys and status values.
    const JSON_FIXTURE_ARRAY: &str = r#"[
        {"name": "daily", "enabled": true, "running": false},
        {"task": "campaign", "enable": false},
        {"title": "mail", "status": "running"}
    ]"#;

    /// Object fixture: task array under the common "tasks" key.
    const JSON_FIXTURE_OBJECT: &str = r#"{"tasks": [{"name": "daily", "enabled": true}]}"#;

    // ---- test helpers ------------------------------------------------------

    /// Temporary directory guard; removed on drop (also on test panic).
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "alas-tasks-{}-{label}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Mock server: accept one connection, read the request, write `response`.
    fn serve_response(response: String) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (port, handle)
    }

    // ---- http_get: happy paths ---------------------------------------------

    #[test]
    fn http_get_happy_with_content_length() {
        let body = r#"{"tasks":[{"name":"daily","enabled":true}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
            body.len()
        );
        let (port, handle) = serve_response(response);
        let got = http_get("127.0.0.1", port, "/api/tasks", Duration::from_secs(2)).unwrap();
        assert_eq!(got, body);
        handle.join().unwrap();
    }

    #[test]
    fn http_get_happy_no_content_length_read_to_close() {
        // No Content-Length: body = rest of stream (server drops → EOF).
        let body = "no-length-body";
        let response = format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}");
        let (port, handle) = serve_response(response);
        let got = http_get("127.0.0.1", port, "/tasks", Duration::from_secs(2)).unwrap();
        assert_eq!(got, body);
        handle.join().unwrap();
    }

    // ---- http_get: failure paths --------------------------------------------

    #[test]
    fn http_get_connection_refused_returns_err() {
        // Bind, note the port, drop the listener → nothing listens there.
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            port
        };
        let res = http_get("127.0.0.1", port, "/x", Duration::from_secs(2));
        assert!(res.is_err(), "expected Err, got {:?}", res);
    }

    #[test]
    fn http_get_timeout_returns_err_bounded() {
        // Listener accepts but never responds; the read timeout must fire.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 64];
                let _ = stream.read(&mut buf);
                thread::sleep(Duration::from_millis(2000)); // never write a response
            }
        });
        let start = std::time::Instant::now();
        let res = http_get("127.0.0.1", port, "/x", Duration::from_millis(300));
        let elapsed = start.elapsed();
        assert!(res.is_err(), "expected Err, got {:?}", res);
        assert!(
            elapsed < Duration::from_secs(3),
            "timeout did not bound the call: {elapsed:?}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn http_get_malformed_response_returns_err() {
        let (port, handle) = serve_response("not http\r\n\r\n".to_string());
        let res = http_get("127.0.0.1", port, "/x", Duration::from_secs(2));
        assert!(res.is_err(), "expected Err, got {:?}", res);
        handle.join().unwrap();
    }

    #[test]
    fn http_get_truncated_body_returns_err() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort".to_string();
        let (port, handle) = serve_response(response);
        let res = http_get("127.0.0.1", port, "/x", Duration::from_secs(2));
        assert!(res.is_err(), "expected Err, got {:?}", res);
        assert!(
            format!("{:?}", res.err()).contains("truncated"),
            "expected truncated error"
        );
        handle.join().unwrap();
    }

    // ---- parse_tasks_json ----------------------------------------------------

    #[test]
    fn parse_tasks_json_array_fixture() {
        let tasks = parse_tasks_json(JSON_FIXTURE_ARRAY).unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(
            tasks[0],
            Task {
                name: "daily".into(),
                enabled: true,
                running: false
            }
        );
        // name from "task", enabled from "enable"
        assert_eq!(
            tasks[1],
            Task {
                name: "campaign".into(),
                enabled: false,
                running: false
            }
        );
        // name from "title", status "running" → enabled AND running
        assert_eq!(
            tasks[2],
            Task {
                name: "mail".into(),
                enabled: true,
                running: true
            }
        );
    }

    #[test]
    fn parse_tasks_json_object_with_tasks_key() {
        let tasks = parse_tasks_json(JSON_FIXTURE_OBJECT).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "daily");
        assert!(tasks[0].enabled);
        assert!(!tasks[0].running);
    }

    #[test]
    fn parse_tasks_json_data_key_and_defaults() {
        // "data" key path; missing enabled → default false; missing name → ""
        let tasks = parse_tasks_json(r#"{"data": [{"name": "x"}, {"title": "y", "status": "enabled"}]}"#)
            .unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(!tasks[0].enabled);
        assert!(tasks[1].enabled);
        assert!(!tasks[1].running);
        let skipped = parse_tasks_json(r#"{"data": [42, "nope", {"name": "ok"}]}"#).unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].name, "ok");
    }

    #[test]
    fn parse_tasks_json_empty_ok() {
        assert_eq!(parse_tasks_json("[]").unwrap(), vec![]);
        assert_eq!(parse_tasks_json(r#"{"tasks": []}"#).unwrap(), vec![]);
        assert_eq!(parse_tasks_json(r#"{"unrelated": 1}"#).unwrap(), vec![]);
    }

    #[test]
    fn parse_tasks_json_malformed_err() {
        assert!(parse_tasks_json("{not json").is_err());
        assert!(parse_tasks_json("").is_err());
        assert!(parse_tasks_json("garbage").is_err());
    }

    // ---- parse_tasks_config ----------------------------------------------------

    #[test]
    fn parse_tasks_config_standalone_and_scheduler_map() {
        let tmp = TempDir::new("standalone");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("deploy.yaml"), "Deploy:\n  Webui:\n    WebuiPort: 22267\n").unwrap();
        std::fs::write(cfg.join("daily.yaml"), "enabled: true\n").unwrap();
        std::fs::write(cfg.join("campaign.yaml"), "enabled: false\n").unwrap();
        std::fs::write(cfg.join("plain.yaml"), "some: config\n").unwrap(); // no enable key → default true
        std::fs::write(cfg.join("notes.txt"), "not a task file\n").unwrap(); // non-yaml ignored
        std::fs::write(cfg.join("scheduler.yaml"), "daily: true\ncampaign: false\n").unwrap();

        let tasks = parse_tasks_config(tmp.path()).unwrap();
        assert!(tasks.iter().any(|t| t.name == "daily" && t.enabled));
        assert!(tasks.iter().any(|t| t.name == "campaign" && !t.enabled));
        assert!(tasks.iter().any(|t| t.name == "plain" && t.enabled));
        assert!(
            !tasks.iter().any(|t| t.name == "deploy"),
            "deploy.yaml must be excluded"
        );
        assert!(
            tasks.iter().all(|t| !t.running),
            "running must be false from files"
        );
    }

    #[test]
    fn parse_tasks_config_scheduler_sequence() {
        let tmp = TempDir::new("sched-seq");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("scheduler.yaml"),
            "- name: daily\n  enable: true\n- task: campaign\n  enabled: false\n",
        )
        .unwrap();
        let tasks = parse_tasks_config(tmp.path()).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().any(|t| t.name == "daily" && t.enabled));
        assert!(tasks.iter().any(|t| t.name == "campaign" && !t.enabled));
    }

    #[test]
    fn parse_tasks_config_missing_dir_empty() {
        let tmp = TempDir::new("missing");
        assert_eq!(parse_tasks_config(tmp.path()).unwrap(), vec![]);
    }

    #[test]
    fn parse_tasks_config_empty_config_dir_empty() {
        let tmp = TempDir::new("empty");
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        assert_eq!(parse_tasks_config(tmp.path()).unwrap(), vec![]);
    }

    // ---- fetch_tasks -----------------------------------------------------------

    #[test]
    fn fetch_tasks_prefers_api_over_config() {
        // Mock webui: first candidate path returns a task list.
        let body = r#"{"data": [{"name": "daily", "enabled": true}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (port, handle) = serve_response(response);
        // Even with a yaml fallback present, the API must win.
        let tmp = TempDir::new("prefers-api");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("daily.yaml"), "enabled: true\n").unwrap();

        let tasks = fetch_tasks(port, tmp.path(), Duration::from_secs(2)).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "daily");
        assert!(tasks[0].enabled);
        handle.join().unwrap();
    }

    #[test]
    fn fetch_tasks_falls_back_to_config_on_all_404() {
        // Mock webui that 404s every candidate path.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            for _ in 0..CANDIDATE_PATHS.len() {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    }
                    Err(_) => break,
                }
            }
        });
        let tmp = TempDir::new("fallback");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("daily.yaml"), "enabled: true\n").unwrap();

        let tasks = fetch_tasks(port, tmp.path(), Duration::from_secs(2)).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "daily");
        assert!(tasks[0].enabled);
        handle.join().unwrap();
    }

    #[test]
    fn fetch_tasks_empty_api_and_no_config_returns_empty() {
        // API answers 200 with an empty task list on every path → fallback
        // with no config dir → Ok(vec![]), NOT Err.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let empty = r#"{"tasks": []}"#;
        let handle = thread::spawn(move || {
            for _ in 0..CANDIDATE_PATHS.len() {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{empty}",
                            empty.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(_) => break,
                }
            }
        });
        let tmp = TempDir::new("empty-api");
        let tasks = fetch_tasks(port, tmp.path(), Duration::from_secs(2)).unwrap();
        assert_eq!(tasks, vec![]);
        handle.join().unwrap();
    }
}
