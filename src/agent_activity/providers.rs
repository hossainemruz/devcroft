//! Structured observation adapters for agent harnesses that expose it.

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

use super::ActivityEmitter;
use crate::agent::AgentKind;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const IO_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_HTTP_BODY: usize = 64 * 1024;
const MAX_HTTP_CHUNK: usize = 1024 * 1024;

#[derive(Default)]
pub(super) struct ProviderLaunch {
    pub(super) arguments: Vec<String>,
    pub(super) opencode_v1_arguments: Vec<String>,
    pub(super) environment: Vec<(String, String)>,
    pub(super) cleanup_paths: Vec<PathBuf>,
}

pub(super) fn prepare(
    agent: AgentKind,
    generation: u64,
    emitter: &ActivityEmitter,
    cancelled: Arc<AtomicBool>,
) -> Result<ProviderLaunch> {
    match agent {
        AgentKind::Opencode => prepare_opencode(emitter.clone(), cancelled),
        AgentKind::Claude => prepare_claude(generation, emitter.clone(), cancelled),
        AgentKind::Codex | AgentKind::Omp => Ok(ProviderLaunch::default()),
    }
}

fn prepare_opencode(
    emitter: ActivityEmitter,
    cancelled: Arc<AtomicBool>,
) -> Result<ProviderLaunch> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("allocating an OpenCode activity port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let password = random_hex(24)?;
    let observer_password = password.clone();
    thread::Builder::new()
        .name("opencode-activity".to_owned())
        .spawn(move || observe_opencode(port, &observer_password, emitter, cancelled))
        .context("starting OpenCode activity observer")?;
    Ok(opencode_v1_launch(port, password))
}

fn opencode_v1_launch(port: u16, password: String) -> ProviderLaunch {
    ProviderLaunch {
        opencode_v1_arguments: vec![
            "--hostname".to_owned(),
            "127.0.0.1".to_owned(),
            "--port".to_owned(),
            port.to_string(),
        ],
        environment: vec![("DEVCROFT_OPENCODE_SERVER_PASSWORD".to_owned(), password)],
        arguments: Vec::new(),
        cleanup_paths: Vec::new(),
    }
}

fn observe_opencode(
    port: u16,
    password: &str,
    emitter: ActivityEmitter,
    cancelled: Arc<AtomicBool>,
) {
    // A healthy SSE connection can still miss a state transition. Refresh the
    // authoritative snapshot independently, including while the stream is quiet.
    thread::scope(|scope| {
        scope.spawn(|| {
            while !cancelled.load(Ordering::Acquire) {
                if reconcile_opencode(port, password, &emitter, &cancelled).is_err() {
                    emitter.unavailable("OpenCode status connection unavailable".to_owned());
                }
                wait_cancelled(&cancelled, Duration::from_secs(1));
            }
        });
        observe_opencode_events(port, password, &emitter, &cancelled);
    });
}

fn observe_opencode_events(
    port: u16,
    password: &str,
    emitter: &ActivityEmitter,
    cancelled: &AtomicBool,
) {
    let mut delay = Duration::from_millis(100);
    while !cancelled.load(Ordering::Acquire) {
        let result = consume_opencode_stream(port, password, emitter, cancelled);
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if result.is_ok() {
            delay = Duration::from_millis(100);
        } else {
            delay = (delay * 2).min(Duration::from_secs(2));
        }
        wait_cancelled(cancelled, delay);
    }
}

fn consume_opencode_stream(
    port: u16,
    password: &str,
    emitter: &ActivityEmitter,
    cancelled: &AtomicBool,
) -> Result<()> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let auth = base64_encode(format!("opencode:{password}").as_bytes());
    write!(
        stream,
        "GET /event HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\nAuthorization: Basic {auth}\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let (status, chunked, _) = read_response_headers(&mut reader, cancelled)?;
    if !(200..300).contains(&status) {
        bail!("OpenCode event stream returned HTTP {status}");
    }
    let mut parser = SseParser::default();
    if chunked {
        consume_chunked(&mut reader, cancelled, |bytes| {
            parser.push(bytes, |event, data| {
                handle_opencode_event(event, data, emitter)
            });
        })?;
    } else {
        let mut buffer = [0_u8; 8192];
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(length) => parser.push(&buffer[..length], |event, data| {
                    handle_opencode_event(event, data, emitter)
                }),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

/// Reconcile current session work independently of event stream delivery.
/// This prevents a disconnect from leaving a stale Working/Idle projection;
/// request dialogs still rely on their identity-bearing events and the live
/// terminal fallback.
fn reconcile_opencode(
    port: u16,
    password: &str,
    emitter: &ActivityEmitter,
    cancelled: &AtomicBool,
) -> Result<()> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let auth = base64_encode(format!("opencode:{password}").as_bytes());
    write!(
        stream,
        "GET /session/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nAuthorization: Basic {auth}\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let (status, chunked, content_length) = read_response_headers(&mut reader, cancelled)?;
    if !(200..300).contains(&status) {
        bail!("OpenCode status snapshot returned HTTP {status}");
    }
    let body = read_http_body(&mut reader, chunked, content_length, cancelled)?;
    let statuses = serde_json::from_slice::<Value>(&body)?;
    let working_sessions = opencode_working_sessions(&statuses)?;
    emitter.reconcile_sessions(working_sessions, "OpenCode status reconciled".to_owned());
    Ok(())
}

fn opencode_working_sessions(statuses: &Value) -> Result<HashSet<String>> {
    let Some(statuses) = statuses.as_object() else {
        bail!("OpenCode status snapshot was not an object");
    };
    Ok(statuses
        .iter()
        .filter_map(|(session, status)| {
            let state = status
                .as_str()
                .or_else(|| status.get("type").and_then(Value::as_str));
            matches!(state, Some("busy" | "working" | "retry")).then(|| session.clone())
        })
        .collect())
}

fn read_http_body(
    reader: &mut BufReader<TcpStream>,
    chunked: bool,
    content_length: Option<usize>,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    if chunked {
        let mut oversized = false;
        consume_chunked(reader, cancelled, |bytes| {
            if body.len().saturating_add(bytes.len()) > MAX_HTTP_BODY {
                oversized = true;
            } else if !oversized {
                body.extend_from_slice(bytes);
            }
        })?;
        if oversized {
            bail!("OpenCode status snapshot is too large");
        }
        return Ok(body);
    }
    let length = content_length.ok_or_else(|| anyhow!("missing response content length"))?;
    if length > MAX_HTTP_BODY {
        bail!("OpenCode status snapshot is too large");
    }
    body.resize(length, 0);
    read_exact_retry(reader, &mut body, cancelled)?;
    Ok(body)
}

fn handle_opencode_event(sse_event: Option<&str>, data: &str, emitter: &ActivityEmitter) {
    let Ok(envelope) = serde_json::from_str::<Value>(data) else {
        return;
    };
    let payload = envelope.get("payload").unwrap_or(&envelope);
    let event_type = payload.get("type").and_then(Value::as_str).or(sse_event);
    let Some(event_type) = event_type else {
        return;
    };
    let properties = payload.get("properties").unwrap_or(payload);
    let session = string_field(properties, &["sessionID", "sessionId"]).unwrap_or("terminal");
    let request = string_field(
        properties,
        &["permissionID", "requestID", "requestId", "id"],
    )
    .unwrap_or("request");
    match event_type {
        "tui.session.select" => emitter.session_selected(session),
        "session.created" | "session.updated" => {
            // `createNext` publishes Created *and* Updated, and every later
            // touch/title change publishes Updated. The SSE stream is volatile
            // and creation events during connect are missed, so Updated is the
            // reliable identity signal for new sessions. Only root sessions
            // (no parentID) establish pane identity; children never match the
            // catalog's root-only rows.
            if let Some(info) = properties.get("info")
                && info.get("parentID").and_then(Value::as_str).is_none()
                && let Some(id) = info.get("id").and_then(Value::as_str)
            {
                emitter.session_selected(id);
            }
        }
        "session.status" => {
            let status = properties
                .get("status")
                .and_then(|status| status.as_str().or_else(|| status.get("type")?.as_str()));
            match status {
                Some("busy" | "working" | "retry") => {
                    emitter.session_working(session, Some("OpenCode is working".to_owned()))
                }
                Some("idle") => emitter.session_idle(session, Some("OpenCode is ready".to_owned())),
                _ => {}
            }
        }
        "session.idle" => emitter.session_idle(session, Some("OpenCode is ready".to_owned())),
        "permission.updated"
        | "permission.asked"
        | "permission.v2.asked"
        | "question.asked"
        | "question.v2.asked" => emitter.request_opened(session, request),
        "permission.replied"
        | "permission.v2.replied"
        | "question.replied"
        | "question.rejected"
        | "question.v2.replied"
        | "question.v2.rejected" => emitter.request_resolved(session, request),
        _ => {}
    }
}

fn prepare_claude(
    generation: u64,
    emitter: ActivityEmitter,
    cancelled: Arc<AtomicBool>,
) -> Result<ProviderLaunch> {
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("binding the Claude hook receiver")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let token = random_hex(32)?;
    let runtime = std::env::temp_dir().join(format!(
        "devcroft-agent-activity-{}-{generation}-{}",
        std::process::id(),
        random_hex(8)?
    ));
    let url = format!("http://127.0.0.1:{port}/agent-activity");
    if let Err(error) = materialize_claude_plugin(&runtime, &url) {
        let _ = fs::remove_dir_all(&runtime);
        return Err(error);
    }

    let observer_token = token.clone();
    if let Err(error) = thread::Builder::new()
        .name("claude-activity".to_owned())
        .spawn(move || observe_claude(listener, &observer_token, emitter, cancelled))
    {
        let _ = fs::remove_dir_all(&runtime);
        return Err(error).context("starting Claude hook receiver");
    }
    Ok(ProviderLaunch {
        arguments: vec![
            "--plugin-dir".to_owned(),
            runtime.to_string_lossy().into_owned(),
        ],
        opencode_v1_arguments: Vec::new(),
        environment: vec![("DEVCROFT_AGENT_ACTIVITY_TOKEN".to_owned(), token)],
        cleanup_paths: vec![runtime],
    })
}

fn materialize_claude_plugin(runtime: &std::path::Path, url: &str) -> Result<()> {
    let manifest_dir = runtime.join(".claude-plugin");
    let hooks_dir = runtime.join("hooks");
    fs::create_dir_all(&manifest_dir)?;
    fs::create_dir_all(&hooks_dir)?;
    fs::write(
        manifest_dir.join("plugin.json"),
        serde_json::to_vec(&json!({
            "name": "devcroft-agent-activity",
            "version": "1.0.0",
            "description": "Reports Claude Code lifecycle activity to Devcroft."
        }))?,
    )?;
    fs::write(
        hooks_dir.join("hooks.json"),
        serde_json::to_vec(&claude_hook_configuration(url))?,
    )?;
    restrict_directory(runtime);
    Ok(())
}

fn claude_hook_configuration(url: &str) -> Value {
    let hook = json!({
        "type": "http",
        "url": url,
        "headers": {
            "Authorization": "Bearer ${DEVCROFT_AGENT_ACTIVITY_TOKEN}"
        },
        "allowedEnvVars": ["DEVCROFT_AGENT_ACTIVITY_TOKEN"],
        "timeout": 2
    });
    let mut hooks = serde_json::Map::new();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "PermissionRequest",
        "Notification",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        hooks.insert(event.to_owned(), json!([{ "hooks": [hook.clone()] }]));
    }
    json!({ "hooks": hooks })
}

fn observe_claude(
    listener: TcpListener,
    token: &str,
    emitter: ActivityEmitter,
    cancelled: Arc<AtomicBool>,
) {
    while !cancelled.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, peer)) if peer.ip().is_loopback() => {
                let _ = handle_claude_request(&mut stream, token, &emitter, &cancelled);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return,
        }
    }
}

fn handle_claude_request(
    stream: &mut TcpStream,
    token: &str,
    emitter: &ActivityEmitter,
    cancelled: &AtomicBool,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    read_line_retry(&mut reader, &mut request_line, cancelled)?;
    let accepted_route = request_line.starts_with("POST /agent-activity HTTP/");
    let mut authorized = false;
    let mut json_content = false;
    let mut content_length = None;
    loop {
        let mut line = String::new();
        read_line_retry(&mut reader, &mut line, cancelled)?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            authorized = value.as_bytes() == format!("Bearer {token}").as_bytes();
        } else if name.eq_ignore_ascii_case("content-type") {
            json_content = value.starts_with("application/json");
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse::<usize>().ok();
        }
    }
    let status = if !accepted_route {
        404
    } else if !authorized {
        401
    } else if !json_content {
        415
    } else {
        let length = content_length.ok_or_else(|| anyhow!("missing hook content length"))?;
        if length > MAX_HTTP_BODY {
            413
        } else {
            let mut body = vec![0_u8; length];
            reader.read_exact(&mut body)?;
            if let Ok(payload) = serde_json::from_slice::<Value>(&body) {
                handle_claude_event(&payload, emitter);
                204
            } else {
                400
            }
        }
    };
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        match status {
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
            _ => "Error",
        }
    )?;
    stream.flush()?;
    Ok(())
}

fn handle_claude_event(payload: &Value, emitter: &ActivityEmitter) {
    let Some(event) = payload.get("hook_event_name").and_then(Value::as_str) else {
        return;
    };
    let session = string_field(payload, &["session_id", "sessionId"]).unwrap_or("terminal");
    let request = string_field(payload, &["tool_use_id", "notification_id"]);
    match event {
        "SessionStart" => {
            emitter.session_selected(session);
            emitter.session_idle(session, Some("Claude is ready".to_owned()));
        }
        "UserPromptSubmit" => {
            // SessionStart only supports command/mcp_tool hooks, so its HTTP
            // handler never fires and new sessions keep native=None (staying
            // "New session" after refresh). UserPromptSubmit supports HTTP,
            // fires for the main session with the authoritative id, and
            // coincides with the first preview title becoming available, so
            // establish identity here. This also heals a spurious SessionStart
            // startup id on resume, since subsequent prompts carry the correct
            // id. Subagents never receive user prompts, so this cannot adopt
            // a sidechain id.
            emitter.session_selected(session);
            emitter.resolve_session(session);
            emitter.session_working(session, Some("Claude is working".to_owned()));
        }
        "PreToolUse" => {
            emitter.session_working(session, Some("Claude is working".to_owned()));
        }
        "PostToolUse" | "PostToolUseFailure" => {
            if let Some(request) = request {
                emitter.request_resolved(session, request);
            }
            emitter.session_working(session, Some("Claude is working".to_owned()));
        }
        "PermissionRequest" => {
            emitter.request_opened(session, request.unwrap_or("permission"));
        }
        "Notification" if is_input_notification(payload) => {
            emitter.request_opened(session, request.unwrap_or("input"));
        }
        "Stop" => {
            emitter.resolve_session(session);
            emitter.session_idle(session, Some("Claude finished its turn".to_owned()));
        }
        "StopFailure" => {
            emitter.resolve_session(session);
            emitter.failed("Claude turn stopped with an error".to_owned());
        }
        "SessionEnd" => {
            emitter.resolve_session(session);
            emitter.exited(true);
        }
        _ => {}
    }
}

fn is_input_notification(payload: &Value) -> bool {
    matches!(
        payload.get("notification_type").and_then(Value::as_str),
        Some("permission_prompt" | "elicitation_dialog" | "input_required")
    )
}

fn read_response_headers(
    reader: &mut BufReader<TcpStream>,
    cancelled: &AtomicBool,
) -> Result<(u16, bool, Option<usize>)> {
    let mut status_line = String::new();
    read_line_retry(reader, &mut status_line, cancelled)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("invalid HTTP status"))?;
    let mut chunked = false;
    let mut content_length = None;
    loop {
        let mut line = String::new();
        read_line_retry(reader, &mut line, cancelled)?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if line.to_ascii_lowercase().starts_with("transfer-encoding:")
            && line.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        } else if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().ok();
        }
    }
    Ok((status, chunked, content_length))
}

fn consume_chunked(
    reader: &mut BufReader<TcpStream>,
    cancelled: &AtomicBool,
    mut consume: impl FnMut(&[u8]),
) -> Result<()> {
    loop {
        let mut size_line = String::new();
        read_line_retry(reader, &mut size_line, cancelled)?;
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or(""), 16)?;
        if size == 0 {
            return Ok(());
        }
        if size > MAX_HTTP_CHUNK {
            bail!("OpenCode HTTP chunk is too large");
        }
        let mut chunk = vec![0_u8; size];
        read_exact_retry(reader, &mut chunk, cancelled)?;
        consume(&chunk);
        let mut ending = [0_u8; 2];
        read_exact_retry(reader, &mut ending, cancelled)?;
    }
}

fn read_line_retry(
    reader: &mut impl BufRead,
    line: &mut String,
    cancelled: &AtomicBool,
) -> Result<()> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            bail!("cancelled");
        }
        match reader.read_line(line) {
            Ok(0) => bail!("unexpected end of stream"),
            Ok(_) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn read_exact_retry(
    reader: &mut impl Read,
    mut buffer: &mut [u8],
    cancelled: &AtomicBool,
) -> Result<()> {
    while !buffer.is_empty() {
        if cancelled.load(Ordering::Acquire) {
            bail!("cancelled");
        }
        match reader.read(buffer) {
            Ok(0) => bail!("unexpected end of stream"),
            Ok(length) => buffer = &mut buffer[length..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[derive(Default)]
struct SseParser {
    pending: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8], mut handle: impl FnMut(Option<&str>, &str)) {
        self.pending.extend_from_slice(bytes);
        while let Some((end, delimiter)) = next_sse_boundary(&self.pending) {
            let block = String::from_utf8_lossy(&self.pending[..end]).replace("\r\n", "\n");
            self.pending.drain(..end + delimiter);
            let mut event = None;
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_owned());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.trim_start());
                }
            }
            if !data.is_empty() {
                handle(event.as_deref(), &data.join("\n"));
            }
        }
        if self.pending.len() > MAX_HTTP_BODY {
            self.pending.clear();
        }
    }
}

fn next_sse_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes.windows(2).position(|window| window == b"\n\n");
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(left), Some(right)) if left <= right => Some((left, 2)),
        (Some(_), Some(right)) => Some((right, 4)),
        (Some(left), None) => Some((left, 2)),
        (None, Some(right)) => Some((right, 4)),
        (None, None) => None,
    }
}

fn string_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
}

fn wait_cancelled(cancelled: &AtomicBool, duration: Duration) {
    let slices = (duration.as_millis() / 25).max(1) as usize;
    for _ in 0..slices {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut raw = vec![0_u8; bytes];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut raw))
        .context("reading secure random bytes")?;
    Ok(raw.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[cfg(unix)]
fn restrict_directory(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    for child in [
        path.join(".claude-plugin"),
        path.join("hooks"),
        path.join(".claude-plugin/plugin.json"),
        path.join("hooks/hooks.json"),
    ] {
        let mode = if child.is_dir() { 0o700 } else { 0o600 };
        let _ = fs::set_permissions(child, fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn restrict_directory(_: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_activity::{ActivityState, AgentActivityStore, PreparedAgentLaunch};

    fn test_launch() -> (AgentActivityStore, PathBuf, PreparedAgentLaunch) {
        let (store, _) = AgentActivityStore::new();
        let checkout = PathBuf::from("/tmp/devcroft-provider-test");
        let launch = store.start(&checkout, AgentKind::Codex);
        (store, checkout, launch)
    }

    #[test]
    fn base64_matches_basic_auth_example() {
        assert_eq!(base64_encode(b"opencode:secret"), "b3BlbmNvZGU6c2VjcmV0");
    }

    #[test]
    fn opencode_v1_launch_keeps_server_flags_out_of_common_arguments() {
        let prepared = opencode_v1_launch(4096, "secret".into());
        assert!(prepared.arguments.is_empty());
        assert_eq!(
            prepared.opencode_v1_arguments,
            ["--hostname", "127.0.0.1", "--port", "4096"]
        );
        assert_eq!(
            prepared.environment,
            [("DEVCROFT_OPENCODE_SERVER_PASSWORD".into(), "secret".into())]
        );
    }

    #[test]
    fn sse_parser_handles_fragmented_records() {
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        parser.push(b"event: update\ndata: {\"type\":", |event, data| {
            events.push((event.map(str::to_owned), data.to_owned()));
        });
        parser.push(b"\"session.idle\"}\n\n", |event, data| {
            events.push((event.map(str::to_owned), data.to_owned()));
        });
        assert_eq!(
            events,
            vec![(
                Some("update".to_owned()),
                "{\"type\":\"session.idle\"}".to_owned()
            )]
        );
    }

    #[test]
    fn sse_parser_preserves_utf8_split_across_chunks() {
        let mut parser = SseParser::default();
        let mut data = Vec::new();
        let event = "data: {\"detail\":\"waiting ⚠\"}\r\n\r\n".as_bytes();
        let split = event
            .windows("⚠".len())
            .position(|window| window == "⚠".as_bytes())
            .unwrap()
            + 1;
        parser.push(&event[..split], |_, value| data.push(value.to_owned()));
        parser.push(&event[split..], |_, value| data.push(value.to_owned()));
        assert_eq!(data, vec!["{\"detail\":\"waiting ⚠\"}".to_owned()]);
    }

    #[test]
    fn claude_configuration_is_observational_http_hooks() {
        let config = claude_hook_configuration("http://127.0.0.1:1/agent-activity");
        assert!(config["hooks"]["PermissionRequest"].is_array());
        assert_eq!(config["hooks"]["Stop"][0]["hooks"][0]["type"], "http");
    }

    #[test]
    fn opencode_events_track_request_lifecycle() {
        let (store, checkout, launch) = test_launch();
        let emitter = launch.emitter();
        handle_opencode_event(
            None,
            r#"{"type":"session.status","properties":{"sessionID":"s","status":{"type":"busy"}}}"#,
            &emitter,
        );
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Working
        );
        handle_opencode_event(
            None,
            r#"{"type":"permission.asked","properties":{"sessionID":"s","permissionID":"p"}}"#,
            &emitter,
        );
        assert_eq!(store.snapshot().attention_count(), 1);
        handle_opencode_event(
            None,
            r#"{"type":"permission.replied","properties":{"sessionID":"s","permissionID":"p"}}"#,
            &emitter,
        );
        assert_eq!(store.snapshot().attention_count(), 0);
    }

    #[test]
    fn opencode_reconciliation_keeps_only_working_sessions() {
        let sessions = opencode_working_sessions(&json!({
            "busy": {"type": "busy"},
            "retrying": {"type": "retry"},
            "settled": {"type": "idle"}
        }))
        .unwrap();
        assert_eq!(
            sessions,
            HashSet::from(["busy".to_owned(), "retrying".to_owned()])
        );
    }

    #[test]
    fn opencode_updated_establishes_root_identity_for_title_reconciliation() {
        // The SSE stream is volatile and creation events during connect are
        // missed. Updated fires on create and every later touch/title change,
        // so it is the reliable identity signal for new sessions.
        let (_store, _checkout, launch) = test_launch();
        assert_eq!(launch.native_session(), None);
        handle_opencode_event(
            None,
            r#"{"type":"session.updated","properties":{"sessionID":"new-id","info":{"id":"new-id"}}}"#,
            &launch.emitter(),
        );
        assert_eq!(launch.native_session().as_deref(), Some("new-id"));
        // Child sessions never match the catalog's root-only rows.
        handle_opencode_event(
            None,
            r#"{"type":"session.updated","properties":{"sessionID":"child","info":{"id":"child","parentID":"new-id"}}}"#,
            &launch.emitter(),
        );
        assert_eq!(launch.native_session().as_deref(), Some("new-id"));
    }

    #[test]
    fn claude_stop_failure_is_not_successful_completion() {
        let (store, checkout, launch) = test_launch();
        let emitter = launch.emitter();
        handle_claude_event(
            &json!({"hook_event_name": "UserPromptSubmit", "session_id": "s"}),
            &emitter,
        );
        handle_claude_event(
            &json!({"hook_event_name": "StopFailure", "session_id": "s"}),
            &emitter,
        );
        let activity = store.snapshot().for_checkout(&checkout).unwrap().clone();
        assert_eq!(activity.state, ActivityState::Idle);
        assert_eq!(
            activity.detail.as_deref(),
            Some("Claude turn stopped with an error")
        );
    }

    #[test]
    fn opencode_snapshot_recovers_a_missed_completion_event() {
        let (store, checkout, launch) = test_launch();
        let emitter = launch.emitter();
        handle_opencode_event(
            None,
            r#"{"type":"session.status","properties":{"sessionID":"s","status":{"type":"busy"}}}"#,
            &emitter,
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            assert!(request.starts_with("GET /session/status HTTP/1.1\r\n"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
        });
        reconcile_opencode(port, "test-password", &emitter, &AtomicBool::new(false)).unwrap();
        server.join().unwrap();
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Finished
        );
    }

    #[test]
    fn claude_pre_tool_does_not_resolve_a_future_permission() {
        let (store, _checkout, launch) = test_launch();
        let emitter = launch.emitter();
        for event in ["PreToolUse", "PermissionRequest"] {
            handle_claude_event(
                &json!({"hook_event_name": event, "session_id": "s", "tool_use_id": "tool"}),
                &emitter,
            );
        }
        assert_eq!(store.snapshot().attention_count(), 1);
        handle_claude_event(
            &json!({"hook_event_name": "PostToolUse", "session_id": "s", "tool_use_id": "tool"}),
            &emitter,
        );
        assert_eq!(store.snapshot().attention_count(), 0);
    }

    #[test]
    fn claude_user_prompt_establishes_native_identity_for_title_reconciliation() {
        // SessionStart only supports command/mcp_tool hooks, so its HTTP
        // handler never fires. New sessions must still reconcile their
        // "New session" placeholder with the catalog title on refresh, which
        // requires the authoritative id from the first user prompt.
        let (_store, _checkout, launch) = test_launch();
        assert_eq!(launch.native_session(), None);
        handle_claude_event(
            &json!({"hook_event_name": "UserPromptSubmit", "session_id": "new-id"}),
            &launch.emitter(),
        );
        assert_eq!(launch.native_session().as_deref(), Some("new-id"));
    }
}
