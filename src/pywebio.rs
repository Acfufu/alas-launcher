//! PyWebIO protocol helpers for ALAS scheduler control (macOS tray).
//!
//! Two halves:
//! 1. Runtime version guard ([`pywebio_version`] / [`check_pywebio_version`]):
//!    read the pinned `pywebio` version from the payload `requirements.txt`
//!    and check it against the protocol this launcher speaks.
//! 2. The WebSocket client ([`click_scheduler`]): drive the webui's
//!    scheduler toggle through the same button-onclick path a browser uses.
//!
//! Wire facts are pinned by the empirical capture in
//! `.omo/evidence/task-5-capture/` (`notes.md` is the authoritative writeup):
//! - Endpoint `ws://127.0.0.1:{port}/?app=index`; text JSON frames accepted
//!   (aiohttp handler does `msg.json()`); an `Origin` header is harmless.
//! - The session is auto-created server-side on ws open — there is NO
//!   `new_session` client message. Task ids arrive as `index-<rand>`.
//! - Server commands: `{"command": ..., "spec": ..., "task_id": ...}`;
//!   DOM specs appear inside `output` commands (type `buttons` for
//!   clickable widgets).
//! - Client events: `{"event":"callback","task_id":<widget callback_id>,
//!   "data":<wire value>}` (data is the button's INDEX, not its label) and
//!   `{"event":"js_yield","task_id":<run_script task_id>,"data":<result>}`.
//! - The server blocks on `eval_js()` (localStorage reads, visibility
//!   checks) until the client answers with `js_yield`; unanswered, the DOM
//!   never renders.
//! - Navigation needs TWO clicks: the aside instance button (scope
//!   `#pywebio-scope-alas-instance-*`, label = instance name "alas"), then
//!   the menu "Overview" item (scope `#pywebio-scope-menu`, style marker
//!   `--menu-Overview--`). Only then does the overview page with
//!   `scheduler_btn` render.
//! - The scheduler toggle is a `put_button` (BinarySwitchButton) inside
//!   scope `#pywebio-scope-scheduler_btn`; its callback id is valid for
//!   <1s after render (stale callbacks dropped), so the click is sent the
//!   moment the spec arrives.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tracing::debug;
use tungstenite::client::{client, IntoClientRequest};
use tungstenite::http::HeaderValue;
use tungstenite::Message;

use crate::menu_model::ControlLabels;

/// Floor on how long to wait after the ws opens before clicking the aside
/// button: the GUI's `run()` init (which installs `state_switch`) must finish
/// first, otherwise the server's callback thread crashes with
/// `AttributeError: 'AlasGUI' object has no attribute 'state_switch'`
/// (capture: the fixture client waited ~6s).
const HOME_QUIESCE: Duration = Duration::from_secs(6);
/// Observation window after a scheduler click, watching for the button
/// re-render (label flip) that confirms the click landed. Re-renders arrive
/// immediately after the callback, so a short window suffices. The window is
/// observation-only: the click is NEVER repeated (bug-fix task-6b: the ALAS
/// scheduler boot takes 2-5s, so the flip cannot arrive in time; a re-click
/// would deliver a SECOND callback and double-spawn the scheduler).
const CONFIRM_WINDOW: Duration = Duration::from_millis(1500);
/// Granularity of each socket read. Keeps the <1s callback-id staleness race
/// bounded: after a spec arrives we react within ~this many ms.
const READ_POLL: Duration = Duration::from_millis(250);
/// Upper bound for a single ws write.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Parse the pinned pywebio version from a requirements.txt payload.
///
/// Scans lines for `pywebio==<version>`, tolerating trailing comments such
/// as `# via -r requirements-in.txt`. Matching is case-sensitive; a missing
/// or malformed line yields `None`.
pub(crate) fn pywebio_version(requirements_txt: &str) -> Option<String> {
    requirements_txt.lines().find_map(|line| {
        let version = line.trim().strip_prefix("pywebio==")?;
        let version: String = version
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if version.is_empty() {
            return None;
        }
        Some(version)
    })
}

/// True when the pywebio version matches the protocol this launcher speaks.
///
/// Only `Some("1.6.2")` passes; anything else (including `None`) is a
/// mismatch the caller warns about. Purely advisory, never blocking.
pub(crate) fn check_pywebio_version(v: Option<&str>) -> bool {
    v == Some("1.6.2")
}

/// The scheduler-toggle direction a click is expected to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerAction {
    Start,
    Stop,
}

impl SchedulerAction {
    /// Label the scheduler button should show in its pre-click state.
    fn label_before<'a>(&self, labels: &'a ControlLabels) -> &'a str {
        match self {
            SchedulerAction::Start => &labels.start,
            SchedulerAction::Stop => &labels.stop,
        }
    }

    /// Label the button should show after a successful click (flipped).
    fn label_after<'a>(&self, labels: &'a ControlLabels) -> &'a str {
        match self {
            SchedulerAction::Start => &labels.stop,
            SchedulerAction::Stop => &labels.start,
        }
    }
}

/// One decoded server command. `spec` is kept as raw JSON — payloads are
/// heterogeneous (output specs, run_script code, pin_onchange names).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ServerMessage {
    pub command: String,
    pub spec: Value,
    pub task_id: String,
}

/// Decode a server frame. Text JSON only — the aiohttp handler accepts
/// `msg.json()` for text frames (capture: every frame is text JSON).
/// Malformed JSON or a missing non-null string field is an error.
pub(crate) fn parse_server_message(frame: &str) -> Result<ServerMessage> {
    let v: Value = serde_json::from_str(frame).context("server frame is not valid JSON")?;
    let command = v
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string field \"command\""))?
        .to_string();
    let spec = v
        .get("spec")
        .cloned()
        .ok_or_else(|| anyhow!("missing field \"spec\""))?;
    let task_id = v
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string field \"task_id\""))?
        .to_string();
    Ok(ServerMessage {
        command,
        spec,
        task_id,
    })
}

/// The clickable widgets this client needs, located from server output
/// specs. `None` = not yet observed. A later render replaces the id (each
/// render mints a fresh callback id), so callers see the LATEST spec.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LocatedButtons {
    /// aside instance button: scope `#pywebio-scope-alas-instance-*`,
    /// label = instance name "alas" (the default config this tray controls).
    pub aside_callback_id: Option<String>,
    /// menu "Overview" item: scope `#pywebio-scope-menu` AND the
    /// `--menu-Overview--` style marker (language-independent).
    pub menu_callback_id: Option<String>,
    /// scheduler toggle inside scope `#pywebio-scope-scheduler_btn`.
    pub scheduler_callback_id: Option<String>,
    /// scheduler button label at observation time (confirmation hint only).
    pub scheduler_label: Option<String>,
}

impl LocatedButtons {
    /// The scheduler toggle as `(callback_id, label)`.
    fn scheduler_button(&self) -> Option<(String, String)> {
        self.scheduler_callback_id
            .as_ref()
            .map(|id| (id.clone(), self.scheduler_label.clone().unwrap_or_default()))
    }
}

/// Recursively collect the button specs carried by one output spec.
///
/// Output specs nest (custom_widget -> data.contents -> buttons), so the
/// walk visits every object in the tree and classifies `type == "buttons"`
/// nodes by scope/style. Classification is label-agnostic for the menu and
/// scheduler buttons (Oracle MAJOR-1: label is locate-only, never a gate),
/// so webui language drift can never disable a click.
fn collect_buttons(v: &Value, out: &mut LocatedButtons) {
    match v {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("buttons") {
                if let (Some(id), Some(scope)) = (
                    map.get("callback_id").and_then(Value::as_str),
                    map.get("scope").and_then(Value::as_str),
                ) {
                    let label = map
                        .get("buttons")
                        .and_then(|b| b.get(0))
                        .and_then(|b| b.get("label"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let style = map.get("style").and_then(Value::as_str).unwrap_or("");
                    if scope.starts_with("#pywebio-scope-alas-instance-") && label == "alas" {
                        out.aside_callback_id = Some(id.to_string());
                    }
                    if scope == "#pywebio-scope-menu" && style.contains("--menu-Overview--") {
                        out.menu_callback_id = Some(id.to_string());
                    }
                    if scope == "#pywebio-scope-scheduler_btn" {
                        out.scheduler_callback_id = Some(id.to_string());
                        out.scheduler_label = Some(label.to_string());
                    }
                }
            }
            for child in map.values() {
                collect_buttons(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_buttons(item, out);
            }
        }
        _ => {}
    }
}

/// Locate the clickable widgets across all `output` messages seen so far.
///
/// `labels` is accepted for API symmetry with the caller but plays NO role
/// in location: the menu item is found by its `--menu-Overview--` style
/// marker and the scheduler button by its scope, so a webui running in any
/// language (or one whose labels match neither `labels.start` nor
/// `labels.stop`) still resolves — full label mismatch still clicks
/// (Oracle MAJOR-1).
pub(crate) fn locate_buttons(messages: &[ServerMessage], _labels: &ControlLabels) -> LocatedButtons {
    let mut out = LocatedButtons::default();
    for m in messages {
        if m.command == "output" {
            collect_buttons(&m.spec, &mut out);
        }
    }
    out
}

/// Compute the `js_yield` reply for a server command, if one is required.
///
/// ALAS blocks the main thread on `eval_js()` (localStorage reads,
/// visibility checks) and never renders the DOM until the client answers —
/// capture: while unanswered only `pin_onchange` floods arrive. Only
/// `run_script` with `eval: true` needs a reply; fire-and-forget `run_js`
/// messages (style injection, reload hooks) do not. `visibilityState` reads
/// get `"visible"` (this client is a headless window); localStorage reads
/// and anything else get `null` (the captured client's answers).
pub(crate) fn js_yield_reply(msg: &ServerMessage) -> Option<Value> {
    if msg.command != "run_script" {
        return None;
    }
    if !msg.spec.get("eval").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let code = msg.spec.get("code").and_then(Value::as_str).unwrap_or("");
    if code.contains("visibilityState") {
        Some(json!("visible"))
    } else {
        Some(Value::Null)
    }
}

/// Encode a client `callback` event. `data` is the widget's WIRE value —
/// the button INDEX (0), not the label: pywebio `_format_button` rewrites
/// button values to indexes and the server maps the index back before
/// invoking the callback (capture: `{"event":"callback","task_id":<widget
/// callback_id>,"data":0}`).
pub(crate) fn callback_event(callback_id: &str, data: i64) -> String {
    json!({"event": "callback", "task_id": callback_id, "data": data}).to_string()
}

/// Encode a client `js_yield` event answering a `run_script` eval: the
/// `task_id` echoes the server command's task id, `data` the eval result.
pub(crate) fn js_yield_event(task_id: &str, data: &Value) -> String {
    json!({"event": "js_yield", "task_id": task_id, "data": data}).to_string()
}

/// Open the ws to the webui with an `Origin` header mirroring the browser
/// (same-site; harmless — capture confirmed the ws handshake does not
/// enforce origin). All socket operations are time-bounded: connect via
/// `connect_timeout`, reads via `READ_POLL`, writes via `WRITE_TIMEOUT`.
///
/// The request is built through tungstenite's `IntoClientRequest` (from the
/// URL string), NOT a hand-rolled `http::Request`: since tungstenite 0.30
/// `client()` no longer synthesizes the `Sec-WebSocket-Key` header, and a
/// request without it fails the handshake before any byte reaches the
/// server (exposed by the todo-7 real-payload roundtrip).
fn connect(port: u16, deadline: Instant) -> Result<tungstenite::WebSocket<TcpStream>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("connect timeout");
    }
    let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    let stream = TcpStream::connect_timeout(&addr, remaining).context("tcp connect failed")?;
    stream.set_read_timeout(Some(READ_POLL)).context("set read timeout")?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .context("set write timeout")?;
    let url = format!("ws://127.0.0.1:{port}/?app=index");
    let mut request = url.into_client_request().context("build ws request")?;
    request.headers_mut().insert(
        "Origin",
        HeaderValue::from_str(&format!("http://127.0.0.1:{port}")).context("build Origin header")?,
    );
    let (ws, _response) = client(request, stream).map_err(|e| anyhow!("ws handshake failed: {e:?}"))?;
    Ok(ws)
}

/// Drive the webui through the scheduler toggle, mirroring a browser's
/// click path: aside instance button -> menu "Overview" -> scheduler button.
///
/// Confirmation semantics (todo 5): delivering the scheduler `callback` IS
/// success. `Err` only on connect failure, protocol mismatch (binary frame),
/// button-not-found or timeout. The label flip after the click is an
/// observation-only confirmation signal — under webui language drift the
/// flip may never match `labels`, and the click must still count as
/// delivered. ONCE the callback is sent the click is NEVER repeated: a
/// second callback would double-spawn the ALAS scheduler (its boot takes
/// 2-5s, longer than the flip-observation window, so a re-click could never
/// be confirmation-driven — bug-fix task-6b). Ok is returned on the label
/// flip, on CONFIRM_WINDOW elapse, on connection close or on timeout; a
/// genuinely dropped click is retryable by the user, and the poll loop
/// self-heals the tray state line.
///
/// Each attempt runs against a single short-lived ws session (no long-lived
/// connection, no heartbeat, no login flow) bounded by `timeout` (default
/// 15s per the plan; the ~6s home quiesce dominates the budget).
pub(crate) fn click_scheduler(
    port: u16,
    action: SchedulerAction,
    labels: &ControlLabels,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let start = Instant::now();
    let mut ws = connect(port, deadline).context("PyWebIO ws connect failed")?;

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Phase {
        Home,
        Menu,
        Scheduler,
        Confirm,
    }

    let mut phase = Phase::Home;
    let mut seen: Vec<ServerMessage> = Vec::new();
    let mut confirm_started = Instant::now();

    loop {
        if Instant::now() >= deadline {
            if phase == Phase::Confirm {
                return Ok(());
            }
            bail!("PyWebIO session did not reach the scheduler button within {timeout:?} (phase {phase:?})");
        }
        if phase == Phase::Confirm && confirm_started.elapsed() >= CONFIRM_WINDOW {
            return Ok(());
        }

        match phase {
            Phase::Home => {
                let loc = locate_buttons(&seen, labels);
                if let Some(id) = loc.aside_callback_id {
                    if start.elapsed() >= HOME_QUIESCE {
                        debug!(target: "pywebio", "clicking aside instance button {id}");
                        ws.send(Message::text(callback_event(&id, 0)))
                            .context("send aside click failed")?;
                        phase = Phase::Menu;
                    }
                }
            }
            Phase::Menu => {
                if let Some(id) = locate_buttons(&seen, labels).menu_callback_id {
                    debug!(target: "pywebio", "clicking menu Overview button {id}");
                    ws.send(Message::text(callback_event(&id, 0)))
                        .context("send menu click failed")?;
                    phase = Phase::Scheduler;
                }
            }
            Phase::Scheduler => {
                if let Some((id, label)) = locate_buttons(&seen, labels).scheduler_button() {
                    if label != action.label_before(labels) {
                        // Oracle MAJOR-1: label is locate-only, never a gate.
                        // Language drift or a stale render still clicks; the
                        // user-visible direction (Start/Stop) is decided
                        // before this session, and the confirm phase never
                        // re-clicks.
                        debug!(
                            target: "pywebio",
                            "scheduler button label {label:?} != expected {:?}; clicking anyway",
                            action.label_before(labels),
                        );
                    }
                    ws.send(Message::text(callback_event(&id, 0)))
                        .context("send scheduler click failed")?;
                    confirm_started = Instant::now();
                    phase = Phase::Confirm;
                }
            }
            Phase::Confirm => {
                // Observation-only: the callback was already delivered — NEVER
                // click again (a second callback double-spawns the scheduler;
                // its 2-5s boot outlives any flip-observation window). Return
                // Ok on the label flip; CONFIRM_WINDOW elapse, close, binary
                // frame, read error and deadline all return Ok below.
                let loc = locate_buttons(&seen, labels);
                if let Some((_, label)) = loc.scheduler_button() {
                    if label == action.label_after(labels) {
                        return Ok(());
                    }
                }
            }
        }

        match ws.read() {
            Ok(Message::Text(text)) => match parse_server_message(text.as_str()) {
                Ok(msg) => {
                    if let Some(data) = js_yield_reply(&msg) {
                        ws.send(Message::text(js_yield_event(&msg.task_id, &data)))
                            .context("send js_yield reply failed")?;
                    }
                    seen.push(msg);
                }
                Err(err) => {
                    debug!(target: "pywebio", "dropping unparseable frame: {err:#}");
                }
            },
            Ok(Message::Binary(_)) => {
                if phase == Phase::Confirm {
                    return Ok(());
                }
                bail!("protocol mismatch: binary frame, expected text JSON");
            }
            Ok(Message::Close(_)) => {
                if phase == Phase::Confirm {
                    return Ok(());
                }
                bail!("server closed the ws before the scheduler click was sent");
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(io_err))
                if matches!(io_err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(err) => {
                if phase == Phase::Confirm {
                    return Ok(());
                }
                return Err(err).context("PyWebIO ws read failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{check_pywebio_version, pywebio_version};

    const REAL_SAMPLE: &str = "pywebio==1.6.2            # via -r requirements-in.txt";

    #[test]
    fn parses_real_payload_line_with_trailing_comment() {
        assert_eq!(pywebio_version(REAL_SAMPLE).as_deref(), Some("1.6.2"));
    }

    #[test]
    fn parses_plain_line() {
        assert_eq!(pywebio_version("pywebio==1.6.2\n").as_deref(), Some("1.6.2"));
    }

    #[test]
    fn parses_multiline_file() {
        let txt = "tornado==6.1\npywebio==1.6.2\nuser-agents==2.2.0\n";
        assert_eq!(pywebio_version(txt).as_deref(), Some("1.6.2"));
    }

    #[test]
    fn is_case_sensitive() {
        assert_eq!(pywebio_version("PyWebIO==1.6.2"), None);
        assert_eq!(pywebio_version("PYWEBIO==1.6.2"), None);
    }

    #[test]
    fn missing_pywebio_line_yields_none() {
        assert_eq!(pywebio_version("tornado==6.1\nuser-agents==2.2.0\n"), None);
        assert_eq!(pywebio_version(""), None);
    }

    #[test]
    fn malformed_lines_yield_none() {
        assert_eq!(pywebio_version("pywebio==\n"), None);
        assert_eq!(pywebio_version("pywebio 1.6.2\n"), None);
        assert_eq!(pywebio_version("pywebio>=1.6.2\n"), None);
    }

    #[test]
    fn guard_matrix() {
        assert!(check_pywebio_version(Some("1.6.2")));
        assert!(!check_pywebio_version(Some("1.6.3")));
        assert!(!check_pywebio_version(Some("2.0.0")));
        assert!(!check_pywebio_version(None));
        assert!(check_pywebio_version(pywebio_version(REAL_SAMPLE).as_deref()));
    }
}

#[cfg(test)]
mod client_tests {
    use super::{
        callback_event, js_yield_event, js_yield_reply, locate_buttons, parse_server_message,
        SchedulerAction, ServerMessage,
    };
    use crate::menu_model::{control_labels, ControlLabels};
    use serde_json::{json, Value};

    // ---- Fixtures: verbatim frames from .omo/evidence/task-5-capture/ ----
    // home.jsonl:1 / home-all.jsonl:926,927,937 / overview.jsonl:932,949.
    // The scheduler_btn button spec was NOT captured (capture ended when its
    // scope container rendered empty); SCHEDULER_BUTTON_* are synthesized to
    // the exact shape put_button produces (module/webui/widgets.py
    // BinarySwitchButton.update_button -> put_button, cf. the captured
    // aside/menu buttons specs) — documented derivation, not fabricated data.

    const PIN_ONCHANGE: &str = r##"{"command": "pin_onchange", "spec": {"name": "Alas_Emulator_Serial", "callback_id": "CB-put_queue-869zT0OrZy", "clear": false}, "task_id": "index-4419903296"}"##;

    const RUN_SCRIPT_LOCALSTORAGE: &str = r##"{"command": "run_script", "spec": {"code": "localStorage.getItem(key)", "args": {"key": "aside"}, "eval": true}, "task_id": "index-4419903296"}"##;

    const RUN_SCRIPT_RELOAD: &str = r##"{"command": "run_script", "spec": {"code": "\n        reload = 1;\n        WebIO._state.CurrentSession.on_session_close(\n            ()=>{\n                setTimeout(\n                    ()=>{\n                        if (reload == 1){\n                            location.reload();\n                        }\n                    }, 4000\n                )\n            }\n        );\n        ", "args": {}}, "task_id": "index-4419903296"}"##;

    const ASIDE_BUTTON: &str = r##"{"command": "output", "spec": {"type": "custom_widget", "template": "<div style=\"display: grid; grid-auto-flow: row; grid-template-rows: 0;\">\n        {{#contents}}\n            {{& pywebio_output_parse}}\n        {{/contents}}\n    </div>", "data": {"contents": [{"type": "custom_widget", "template": "<div class=\"{{dom_class_name}}\">\n            {{#contents}}\n                {{#.}}\n                    {{& pywebio_output_parse}}\n                {{/.}}\n            {{/contents}}\n        </div>", "data": {"contents": [{"type": "html", "content": "<svg class=\"aside-icon icon-run\" viewBox=\"0 0 1024 1024\" version=\"1.1\" xmlns=\"http://www.w3.org/2000/svg\"><path d=\"M213.333333 65.386667a85.333333 85.333333 0 0 1 43.904 12.16L859.370667 438.826667a85.333333 85.333333 0 0 1 0 146.346666L257.237333 946.453333A85.333333 85.333333 0 0 1 128 873.28V150.72a85.333333 85.333333 0 0 1 85.333333-85.333333z m0 64a21.333333 21.333333 0 0 0-21.184 18.837333L192 150.72v722.56a21.333333 21.333333 0 0 0 30.101333 19.456l2.197334-1.152L826.453333 530.282667a21.333333 21.333333 0 0 0 2.048-35.178667l-2.048-1.386667L224.298667 132.416A21.333333 21.333333 0 0 0 213.333333 129.386667z\"></path></svg>", "sanitize": false, "scope": "#pywebio-scope-alas-instance-0", "position": -1}], "dom_class_name": "pywebio-scope-cQKM2JaRa2"}, "scope": "#pywebio-scope-alas-instance-0", "position": -1, "style": ";z-index: 1; margin-left: 8px;text-align: center"}, {"type": "buttons", "callback_id": "CB-click_callback-PytyyOxVgc", "buttons": [{"label": "alas", "value": 0, "color": "aside"}], "link": false, "outline": false, "group": false, "scope": "#pywebio-scope-alas-instance-0", "position": -1, "style": ";z-index: 2; --aside-alas--;"}]}, "scope": "#pywebio-scope-alas-instance-0", "position": -1}, "task_id": "index-4419903296"}"##;

    const MENU_BUTTON: &str = r##"{"command": "output", "spec": {"type": "buttons", "callback_id": "CB-click_callback-G1ZkGUcbfK", "buttons": [{"label": "\u603b\u89c8", "value": 0, "color": "menu"}], "link": false, "outline": false, "group": false, "scope": "#pywebio-scope-menu", "position": -1, "style": ";--menu-Overview--"}, "task_id": "click_callback-4419689392"}"##;

    const SCHEDULER_BAR: &str = r##"{"command": "output", "spec": {"type": "scope", "dom_id": "pywebio-scope-scheduler-bar", "contents": [{"type": "text", "content": "\u8c03\u5ea6\u5668", "inline": false, "scope": "#pywebio-scope-schedulers", "position": -1, "style": ";font-size: 1.25rem; margin: auto .5rem auto;"}, {"type": "scope", "dom_id": "pywebio-scope-scheduler_btn", "contents": [], "scope": "#pywebio-scope-schedulers", "position": -1}], "scope": "#pywebio-scope-schedulers", "position": -1}, "task_id": "click_callback-4419689392"}"##;

    // Derived from widgets.py update_button (put_button in scope scheduler_btn).
    const SCHEDULER_BUTTON_START: &str = r##"{"command": "output", "spec": {"type": "buttons", "callback_id": "CB-click_callback-schedSynth1", "buttons": [{"label": "\u542f\u52a8", "value": 0, "color": "on"}], "link": false, "outline": false, "group": false, "scope": "#pywebio-scope-scheduler_btn", "position": -1, "style": ""}, "task_id": "click_callback-4419689392"}"##;

    const SCHEDULER_BUTTON_STOPPED: &str = r##"{"command": "output", "spec": {"type": "buttons", "callback_id": "CB-click_callback-schedSynth2", "buttons": [{"label": "\u505c\u6b62", "value": 0, "color": "off"}], "link": false, "outline": false, "group": false, "scope": "#pywebio-scope-scheduler_btn", "position": -1, "style": ""}, "task_id": "click_callback-4419689392"}"##;

    // en-US menu label on the same scope/style (drift variant).
    const MENU_BUTTON_EN: &str = r##"{"command": "output", "spec": {"type": "buttons", "callback_id": "CB-click_callback-ENmenuXYZ", "buttons": [{"label": "Overview", "value": 0, "color": "menu"}], "link": false, "outline": false, "group": false, "scope": "#pywebio-scope-menu", "position": -1, "style": ";--menu-Overview--"}, "task_id": "click_callback-4419689392"}"##;

    // ja-JP scheduler label: matches NEITHER zh-CN start nor stop.
    const SCHEDULER_BUTTON_DRIFT: &str = r##"{"command": "output", "spec": {"type": "buttons", "callback_id": "CB-click_callback-driftXYZ", "buttons": [{"label": "\u5b9f\u884c", "value": 0, "color": "on"}], "link": false, "outline": false, "group": false, "scope": "#pywebio-scope-scheduler_btn", "position": -1, "style": ""}, "task_id": "click_callback-4419689392"}"##;

    // Another menu section: same menu scope, NO --menu-Overview-- marker.
    const MENU_BUTTON_OTHER_SECTION: &str = r##"{"command": "output", "spec": {"type": "buttons", "callback_id": "CB-click_callback-sectXYZ", "buttons": [{"label": "\u51fa\u51fb", "value": 0, "color": "menu"}], "link": false, "outline": false, "group": false, "scope": "#pywebio-scope-menu", "position": -1, "style": ";--menu-Main--"}, "task_id": "click_callback-4419689392"}"##;

    fn labels_zh() -> ControlLabels {
        control_labels("zh-CN", &Value::Null)
    }

    fn parse(frame: &str) -> ServerMessage {
        parse_server_message(frame).expect("fixture frame must parse")
    }

    // ---- message parsing ----

    #[test]
    fn parse_real_pin_onchange_frame() {
        let m = parse(PIN_ONCHANGE);
        assert_eq!(m.command, "pin_onchange");
        assert_eq!(m.task_id, "index-4419903296");
        assert_eq!(m.spec["name"], "Alas_Emulator_Serial");
        assert_eq!(m.spec["callback_id"], "CB-put_queue-869zT0OrZy");
    }

    #[test]
    fn parse_menu_frame_exposes_scope_and_style() {
        let m = parse(MENU_BUTTON);
        assert_eq!(m.command, "output");
        assert_eq!(m.spec["scope"], "#pywebio-scope-menu");
        assert_eq!(m.spec["style"], ";--menu-Overview--");
        assert_eq!(m.spec["callback_id"], "CB-click_callback-G1ZkGUcbfK");
    }

    #[test]
    fn parse_rejects_malformed_frames_without_panicking() {
        assert!(parse_server_message("not json").is_err());
        assert!(parse_server_message("").is_err());
        assert!(parse_server_message(r#"{"command": 1, "spec": {}, "task_id": "x"}"#).is_err());
        assert!(parse_server_message(r#"{"command": "output", "task_id": "x"}"#).is_err());
        assert!(parse_server_message(r#"{"command": "output", "spec": {}}"#).is_err());
    }

    // ---- button location ----

    #[test]
    fn locate_all_three_buttons_from_captured_frames() {
        let frames = [PIN_ONCHANGE, ASIDE_BUTTON, MENU_BUTTON, SCHEDULER_BAR, SCHEDULER_BUTTON_START]
            .map(parse);
        let loc = locate_buttons(&frames, &labels_zh());
        assert_eq!(
            loc.aside_callback_id.as_deref(),
            Some("CB-click_callback-PytyyOxVgc")
        );
        assert_eq!(
            loc.menu_callback_id.as_deref(),
            Some("CB-click_callback-G1ZkGUcbfK")
        );
        assert_eq!(
            loc.scheduler_callback_id.as_deref(),
            Some("CB-click_callback-schedSynth1")
        );
        assert_eq!(loc.scheduler_label.as_deref(), Some("启动"));
    }

    #[test]
    fn empty_scope_container_is_not_a_clickable_button() {
        // The scheduler-bar message only DECLARES the empty
        // pywebio-scope-scheduler_btn container; the clickable put_button
        // arrives in a later frame. The container must not be clicked.
        let frames = [SCHEDULER_BAR].map(parse);
        let loc = locate_buttons(&frames, &labels_zh());
        assert_eq!(loc.scheduler_callback_id, None);
        assert_eq!(loc, super::LocatedButtons::default());
    }

    #[test]
    fn pin_flood_and_run_script_alone_locate_nothing() {
        let frames = [PIN_ONCHANGE, RUN_SCRIPT_LOCALSTORAGE, RUN_SCRIPT_RELOAD].map(parse);
        assert_eq!(locate_buttons(&frames, &labels_zh()), super::LocatedButtons::default());
    }

    #[test]
    fn language_drift_still_locates_by_scope_and_style() {
        // webui labels match NEITHER tray labels.start nor labels.stop:
        // location must not depend on the label (Oracle MAJOR-1).
        let frames = [MENU_BUTTON_EN, SCHEDULER_BUTTON_DRIFT].map(parse);
        let loc = locate_buttons(&frames, &labels_zh());
        assert_eq!(loc.menu_callback_id.as_deref(), Some("CB-click_callback-ENmenuXYZ"));
        assert_eq!(
            loc.scheduler_callback_id.as_deref(),
            Some("CB-click_callback-driftXYZ")
        );
        assert_eq!(loc.scheduler_label.as_deref(), Some("実行"));
    }

    #[test]
    fn latest_rerender_wins_for_callback_id() {
        // Each render mints a fresh callback id; a re-render must replace
        // the previous id (stale ids get dropped by the server).
        let frames = [SCHEDULER_BUTTON_START, SCHEDULER_BUTTON_STOPPED].map(parse);
        let loc = locate_buttons(&frames, &labels_zh());
        assert_eq!(
            loc.scheduler_callback_id.as_deref(),
            Some("CB-click_callback-schedSynth2")
        );
        assert_eq!(loc.scheduler_label.as_deref(), Some("停止"));
    }

    #[test]
    fn menu_button_requires_overview_style_marker() {
        // Other menu-section buttons share the menu scope; only the
        // Overview item carries the --menu-Overview-- marker.
        let frames = [MENU_BUTTON_OTHER_SECTION].map(parse);
        assert_eq!(locate_buttons(&frames, &labels_zh()).menu_callback_id, None);
    }

    // ---- js_yield auto-reply decision ----

    #[test]
    fn eval_localstorage_requires_null_js_yield() {
        let m = parse(RUN_SCRIPT_LOCALSTORAGE);
        assert_eq!(js_yield_reply(&m), Some(Value::Null));
    }

    #[test]
    fn eval_visibility_state_replies_visible() {
        let m = ServerMessage {
            command: "run_script".into(),
            spec: json!({"code": "document.visibilityState", "args": {}, "eval": true}),
            task_id: "loop-4419745488".into(),
        };
        assert_eq!(js_yield_reply(&m), Some(json!("visible")));
    }

    #[test]
    fn non_eval_and_non_script_commands_need_no_reply() {
        let reload = parse(RUN_SCRIPT_RELOAD);
        assert_eq!(js_yield_reply(&reload), None);
        let pin = parse(PIN_ONCHANGE);
        assert_eq!(js_yield_reply(&pin), None);
        let no_eval_flag = ServerMessage {
            command: "run_script".into(),
            spec: json!({"code": "doSomething()"}),
            task_id: "t".into(),
        };
        assert_eq!(js_yield_reply(&no_eval_flag), None);
        let unknown_eval = ServerMessage {
            command: "run_script".into(),
            spec: json!({"code": "doSomething()", "eval": true}),
            task_id: "t".into(),
        };
        assert_eq!(js_yield_reply(&unknown_eval), Some(Value::Null));
    }

    // ---- wire event encoding (vs sent-events.json) ----

    #[test]
    fn callback_event_matches_captured_sent_event() {
        let sent: Value =
            serde_json::from_str(r#"{"event": "callback", "task_id": "CB-click_callback-PytyyOxVgc", "data": 0}"#)
                .unwrap();
        let encoded: Value =
            serde_json::from_str(&callback_event("CB-click_callback-PytyyOxVgc", 0)).unwrap();
        assert_eq!(encoded, sent);
    }

    #[test]
    fn js_yield_event_echoes_task_id_and_data() {
        let sent: Value = serde_json::from_str(
            r#"{"event": "js_yield", "task_id": "loop-4419745488", "data": "visible"}"#,
        )
        .unwrap();
        let encoded: Value =
            serde_json::from_str(&js_yield_event("loop-4419745488", &json!("visible"))).unwrap();
        assert_eq!(encoded, sent);

        let sent_null: Value =
            serde_json::from_str(r#"{"event": "js_yield", "task_id": "index-4419903296", "data": null}"#)
                .unwrap();
        let encoded_null: Value = serde_json::from_str(&js_yield_event(
            "index-4419903296",
            &Value::Null,
        ))
        .unwrap();
        assert_eq!(encoded_null, sent_null);
    }

    #[test]
    fn action_expected_labels_follow_action() {
        let labels = labels_zh();
        assert_eq!(SchedulerAction::Start.label_before(&labels), "启动");
        assert_eq!(SchedulerAction::Start.label_after(&labels), "停止");
        assert_eq!(SchedulerAction::Stop.label_before(&labels), "停止");
        assert_eq!(SchedulerAction::Stop.label_after(&labels), "启动");
    }
}

/// Real-payload integration test (todo 7): a full scheduler Start/Stop
/// roundtrip against the REAL installed ALAS payload on an ISOLATED port.
///
/// - Never touches the user's live backend: every instance spawned here
///   binds 22367 (`--port` overrides deploy.yaml `WebuiPort`, gui.py), and
///   the guard kills the whole process tree on EVERY exit path (Drop runs
///   on panic too: SIGTERM/SIGKILL on the process group the spawned python
///   created via `process_group(0)`, then a pkill fallback on the exact
///   cmdline, then it waits for the port to close).
/// - Environment mirrors the app (src/setup.rs unix `setup_environment`,
///   which itself cannot be called here: `alas_repo_dir()` derives from
///   `current_exe()` and panics under `cargo test`): cwd = payload dir,
///   toolkit PATH + LD_LIBRARY_PATH prepended, `./toolkit/bin/python`.
/// - Scheduler detection mirrors the todo-3 discriminator (tray.rs
///   `uvicorn_alive_child_count`) and its production predicate
///   `backend::scheduler_alive`: alive, non-zombie, non-resource-tracker
///   children of the uvicorn process (the reload wrapper's child when
///   `Deploy.Update.EnableReload`; the backend process itself otherwise);
///   baseline = the multiprocessing.Manager only (1), scheduler running => 2.
///
/// Run: `cargo test ws_roundtrip_real_payload -- --ignored --nocapture`
#[cfg(test)]
mod real_payload_tests {
    use super::*;
    use crate::menu_model::{control_labels, ControlLabels};
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command};

    /// The real installed payload this QA targets.
    const PAYLOAD: &str = "/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript";
    /// Isolated test port — NEVER 22267 (a user's live backend may be there).
    const TEST_PORT: u16 = 22367;
    /// The live backend port, probed read-only to prove we never touched it.
    const LIVE_PORT: u16 = 22267;
    /// Budget for the webui to boot after spawn.
    const BOOT_TIMEOUT: Duration = Duration::from_secs(90);
    /// Budget for the scheduler child to appear after Start (todo 7: <=20s).
    const START_TIMEOUT: Duration = Duration::from_secs(20);
    /// Budget for the scheduler child to disappear after Stop (todo 7: <=10s).
    const STOP_TIMEOUT: Duration = Duration::from_secs(10);
    /// Per-click ws session budget (todo 5 default).
    const CLICK_TIMEOUT: Duration = Duration::from_secs(15);

    fn labels_zh() -> ControlLabels {
        control_labels("zh-CN", &Value::Null)
    }

    /// Mirror of src/setup.rs `prepend_path_to_env`.
    fn prepend_path(key: &str, path: &Path) {
        let mut paths = vec![path.to_path_buf()];
        if let Some(old) = std::env::var_os(key) {
            paths.extend(std::env::split_paths(&old));
        }
        std::env::set_var(key, std::env::join_paths(paths).unwrap());
    }

    /// Replicate src/setup.rs:52-60 (unix `setup_environment`): cwd becomes
    /// the payload dir and PATH/LD_LIBRARY_PATH gain the toolkit entries.
    fn setup_payload_env(payload: &Path) {
        std::env::set_current_dir(payload).expect("set cwd to payload");
        prepend_path(
            "PATH",
            &payload.join("toolkit").join("libexec").join("git-core"),
        );
        prepend_path("PATH", &payload.join("toolkit").join("bin"));
        prepend_path("LD_LIBRARY_PATH", &payload.join("toolkit").join("lib"));
    }

    /// `Deploy.Update.EnableReload` from config/deploy.yaml (true when
    /// missing — the ALAS default): decides which process is the uvicorn.
    /// Kept as a payload-path mirror (QA runs against an arbitrary installed
    /// payload, not the launcher cwd) but the traversal itself is delegated
    /// to the shared `deploy_config` module so the mirror cannot drift.
    fn deploy_enable_reload(payload: &Path) -> bool {
        std::fs::read_to_string(payload.join("config").join("deploy.yaml"))
            .ok()
            .and_then(|s| serde_yaml::from_str::<Value>(&s).ok())
            .map(|c| crate::deploy_config::DeployConfig::from_value(Some(&c)).enable_reload())
            .unwrap_or(true)
    }

    /// The multiprocessing resource_tracker child, excluded from the count
    /// (present in BOTH tree shapes; tray.rs `is_resource_tracker`).
    fn is_resource_tracker(p: &sysinfo::Process) -> bool {
        p.cmd()
            .iter()
            .any(|c| c.to_string_lossy().contains("multiprocessing.resource_tracker"))
    }

    /// The todo-3 discriminator, mirrored from tray.rs
    /// `uvicorn_alive_child_count`: alive non-zombie non-resource-tracker
    /// children of the uvicorn process. 0 when the backend is gone
    /// (conservative — scheduler shown stopped).
    fn scheduler_child_count(backend_pid: u32, enable_reload: bool) -> usize {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let uvicorn_pid = if enable_reload {
            sys.processes()
                .values()
                .find(|p| {
                    p.parent().map(|q| q.as_u32()) == Some(backend_pid) && !is_resource_tracker(p)
                })
                .map(|p| p.pid())
        } else {
            Some(sysinfo::Pid::from_u32(backend_pid))
        };
        let Some(uvicorn_pid) = uvicorn_pid else {
            return 0;
        };
        sys.processes()
            .values()
            .filter(|p| p.parent() == Some(uvicorn_pid))
            .filter(|p| p.status() != sysinfo::ProcessStatus::Zombie)
            .filter(|p| !is_resource_tracker(p))
            .count()
    }

    fn port_open(port: u16) -> bool {
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
        TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
    }

    /// HTTP status code of `GET /`; -1 when the port is not serving HTTP.
    fn webui_status(port: u16) -> i32 {
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
        let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
            return -1;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        let mut buf = [0u8; 64];
        let Ok(n) = stream.read(&mut buf) else {
            return -1;
        };
        String::from_utf8_lossy(&buf[..n])
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<i32>().ok())
            .unwrap_or(-1)
    }

    /// The backend process listening on `port` (reuse path only).
    fn backend_pid_on_port(port: u16) -> Option<u32> {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let needle = format!("--port {port}");
        sys.processes()
            .values()
            .find(|p| p.cmd().iter().any(|c| c.to_string_lossy().contains(&needle)))
            .map(|p| p.pid().as_u32())
    }

    /// Poll `cond` every 500ms until it holds or `timeout` elapses.
    fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if cond() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Poll the discriminator, printing each observation, until `pred`
    /// holds or `timeout` elapses. Returns the last observed count.
    fn wait_scheduler_count(
        what: &str,
        backend_pid: u32,
        reload: bool,
        timeout: Duration,
        pred: impl Fn(usize) -> bool,
    ) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let count = scheduler_child_count(backend_pid, reload);
            eprintln!("  [{what}] scheduler child count = {count}");
            if pred(count) || Instant::now() >= deadline {
                return count;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Kills every process spawned for the test on every exit path: SIGTERM
    /// the whole process group (the spawned python created its own group via
    /// `process_group(0)`; the live ALAS tree confirms the gui.py wrapper,
    /// uvicorn and Manager all share the wrapper's pgid), SIGKILL
    /// escalation, then a pkill fallback on the exact cmdline for anything
    /// that escaped, then wait for the port to close.
    struct BackendGuard {
        child: Option<Child>,
        pgid: Option<i32>,
        port: u16,
    }

    impl Drop for BackendGuard {
        fn drop(&mut self) {
            if let Some(pgid) = self.pgid {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(-pgid),
                    nix::sys::signal::Signal::SIGTERM,
                );
            }
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if let Some(child) = self.child.as_mut() {
                    if child.try_wait().ok().flatten().is_some() {
                        self.child = None;
                    }
                }
                if !port_open(self.port) {
                    break;
                }
                if Instant::now() >= deadline {
                    if let Some(pgid) = self.pgid {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(-pgid),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = Command::new("pkill")
                .args(["-f", "gui.py --host 127.0.0.1 --port 22367"])
                .status();
            let _ = Command::new("pkill")
                .args(["-9", "-f", "gui.py --host 127.0.0.1 --port 22367"])
                .status();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && port_open(self.port) {
                std::thread::sleep(Duration::from_millis(200));
            }
            eprintln!(
                "cleanup: port {} open after guard drop = {}",
                self.port,
                port_open(self.port)
            );
        }
    }

    /// Spawn the ALAS backend on the isolated port; reuse an already-live
    /// 22367 (leftover from a crashed run) without spawning a second one.
    fn spawn_backend(payload: &Path) -> BackendGuard {
        if port_open(TEST_PORT) {
            eprintln!("port {TEST_PORT} already live; reusing (cleanup still applies)");
            return BackendGuard {
                child: None,
                pgid: None,
                port: TEST_PORT,
            };
        }
        let mut cmd = Command::new(payload.join("toolkit").join("bin").join("python"));
        cmd.args(["gui.py", "--host", "127.0.0.1", "--port", "22367"]);
        cmd.current_dir(payload);
        cmd.process_group(0);
        let child = cmd.spawn().expect("failed to spawn the ALAS backend");
        let pid = child.id();
        eprintln!("spawned backend pid {pid} on port {TEST_PORT} (own process group)");
        BackendGuard {
            child: Some(child),
            pgid: Some(pid as i32),
            port: TEST_PORT,
        }
    }

    /// End-to-end scheduler control against the real payload on an isolated
    /// port: Start -> scheduler child appears, Stop -> back to baseline,
    /// webui serving HTTP 200 throughout. Env mirrors the app (cwd =
    /// payload, toolkit PATH/LD_LIBRARY_PATH, `./toolkit/bin/python`).
    #[test]
    #[ignore]
    fn ws_roundtrip_real_payload() {
        let payload = Path::new(PAYLOAD);
        if !payload.join("gui.py").exists() {
            eprintln!("real ALAS payload not present at {PAYLOAD}; skipping");
            return;
        }
        eprintln!("=== ws_roundtrip_real_payload: real payload at {PAYLOAD}");
        eprintln!(
            "live backend ({LIVE_PORT}) listening before test = {}",
            port_open(LIVE_PORT)
        );

        setup_payload_env(payload);
        let requirements = std::fs::read_to_string(payload.join("requirements.txt")).unwrap_or_default();
        assert!(
            check_pywebio_version(pywebio_version(&requirements).as_deref()),
            "payload pywebio != 1.6.2; refusing to drive an unknown protocol"
        );
        eprintln!("pywebio version guard: 1.6.2 confirmed");

        let reload = deploy_enable_reload(payload);
        eprintln!("Deploy.Update.EnableReload = {reload}");

        let guard = spawn_backend(payload);
        let backend_pid = match &guard.child {
            Some(child) => child.id(),
            None => backend_pid_on_port(TEST_PORT)
                .expect("reused backend not findable in the process table"),
        };
        eprintln!("backend pid = {backend_pid}");

        assert!(
            wait_for(BOOT_TIMEOUT, || port_open(TEST_PORT)),
            "webui did not open port {TEST_PORT} within {BOOT_TIMEOUT:?}"
        );
        assert_eq!(webui_status(TEST_PORT), 200, "webui not serving HTTP 200");
        eprintln!("webui up on {TEST_PORT}: HTTP 200");

        // Settle on the baseline (Manager-only) count before clicking, so a
        // transient startup child can never satisfy the Start check.
        assert!(
            wait_for(Duration::from_secs(30), || {
                scheduler_child_count(backend_pid, reload) <= 1
            }),
            "scheduler child count never settled to baseline"
        );
        let baseline = scheduler_child_count(backend_pid, reload);
        eprintln!("baseline scheduler child count = {baseline}");

        // ---- START ----
        // Exact-count assertion (bug-fix task-6b): one Start click must spawn
        // EXACTLY one scheduler child (baseline + 1). A re-click regression
        // would deliver a second callback and double-spawn, overshooting to
        // baseline + 2 — the wait below would then time out and the assert
        // would fail (the old `>1` predicate could not tell them apart).
        eprintln!("click_scheduler(Start)");
        click_scheduler(TEST_PORT, SchedulerAction::Start, &labels_zh(), CLICK_TIMEOUT)
            .expect("click_scheduler(Start) failed");
        let started = wait_scheduler_count(
            "after Start",
            backend_pid,
            reload,
            START_TIMEOUT,
            |count| count == baseline + 1,
        );
        assert_eq!(
            started,
            baseline + 1,
            "exactly one scheduler child must appear after Start; count {started} (a double spawn would show {})",
            baseline + 2,
        );
        assert_eq!(
            webui_status(TEST_PORT),
            200,
            "webui died while scheduler running"
        );
        eprintln!("scheduler STARTED: child count {started} (baseline {baseline} + 1), webui still 200");

        // ---- STOP ----
        eprintln!("click_scheduler(Stop)");
        click_scheduler(TEST_PORT, SchedulerAction::Stop, &labels_zh(), CLICK_TIMEOUT)
            .expect("click_scheduler(Stop) failed");
        let stopped = wait_scheduler_count(
            "after Stop",
            backend_pid,
            reload,
            STOP_TIMEOUT,
            |count| count == baseline,
        );
        assert_eq!(
            stopped,
            baseline,
            "scheduler child count must return to the pre-Start baseline; count {stopped}"
        );
        assert_eq!(webui_status(TEST_PORT), 200, "webui died after scheduler stop");
        eprintln!("scheduler STOPPED: child count {stopped}, webui still 200");
        eprintln!(
            "live backend ({LIVE_PORT}) still listening = {}",
            port_open(LIVE_PORT)
        );

        drop(guard);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !port_open(TEST_PORT),
            "port {TEST_PORT} still open after guard cleanup"
        );
        eprintln!(
            "cleanup verified: port {TEST_PORT} closed; live {LIVE_PORT} intact = {}",
            port_open(LIVE_PORT)
        );
    }
}
