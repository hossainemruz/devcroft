//! Length-framed JSON-RPC transport over a language server's stdio.
//!
//! The wire format follows the LSP base protocol (`Content-Length: N\r\n\r\n`
//! framing around JSON-RPC 2.0 payloads). This is intentionally separate from
//! the line-delimited helper in `crate::agent_sessions`, which speaks a
//! different framing for a different server.
//!
//! The transport owns no language semantics: it moves framed values, matches
//! responses to request ids, answers supported server requests, and rejects
//! unsupported requests. A bounded writer queue keeps notifications off the UI
//! thread. Request cancellation/timeouts remove pending entries and notify the server.

use std::{
    collections::HashMap,
    ffi::OsString,
    io::{BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Upper bound for one framed message; mirrors the metadata subprocess bound.
const MAX_MESSAGE_BYTES: u64 = 32 * 1024 * 1024;

/// A server push that belongs to no outstanding request.
pub enum ServerMessage {
    Notification { method: String, params: Value },
}

/// A spawned server child that is killed and reaped on drop. This mirrors the
/// terminal session and metadata subprocess teardown: no shutdown path may
/// leak the process, even when the UI drops the client mid-request.
struct WatchedChild(Child);

impl Drop for WatchedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

type Pending = Mutex<HashMap<u64, (mpsc::SyncSender<Result<Value>>, Option<String>)>>;

pub struct Transport {
    writer: mpsc::SyncSender<Value>,
    pending: Pending,
    next_id: AtomicU64,
    protocol_errors: AtomicU64,
    exited: AtomicBool,
    _child: Option<WatchedChild>,
}

impl Transport {
    /// Spawn `program` rooted at `cwd` and take over its piped stdio. The
    /// server's stderr is discarded; servers report work through the protocol.
    pub fn spawn(
        program: &Path,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<(Arc<Self>, mpsc::Receiver<ServerMessage>)> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = WatchedChild(
            command
                .spawn()
                .with_context(|| format!("Could not start {}", program.display()))?,
        );
        let stdout = child
            .0
            .stdout
            .take()
            .context("Language server stdout unavailable")?;
        let stdin = child
            .0
            .stdin
            .take()
            .context("Language server stdin unavailable")?;
        Ok(Self::start(stdout, stdin, Some(child)))
    }

    /// Wire an already-open byte-stream pair. Production uses [`Self::spawn`];
    /// tests wire channel adapters plus a scripted fake server, so framing,
    /// routing, and timeout behavior stay deterministic without a binary.
    #[cfg(test)]
    pub fn new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> (Arc<Self>, mpsc::Receiver<ServerMessage>) {
        Self::start(reader, writer, None)
    }

    fn start(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        child: Option<WatchedChild>,
    ) -> (Arc<Self>, mpsc::Receiver<ServerMessage>) {
        let (write_tx, write_rx) = mpsc::sync_channel::<Value>(128);
        let transport = Arc::new(Self {
            writer: write_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            protocol_errors: AtomicU64::new(0),
            exited: AtomicBool::new(false),
            _child: child,
        });
        let (tx, rx) = mpsc::channel();
        let reader_side = Arc::downgrade(&transport);
        thread::Builder::new()
            .name("lsp-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(reader);
                loop {
                    let frame = read_frame(&mut reader);
                    let Some(transport) = reader_side.upgrade() else {
                        break;
                    };
                    match frame {
                        Ok(Some(frame)) => {
                            if !transport.dispatch(frame, &tx) {
                                transport.fail_pending();
                                break;
                            }
                        }
                        result => {
                            if result.is_err() {
                                transport.protocol_errors.fetch_add(1, Ordering::SeqCst);
                            }
                            transport.fail_pending();
                            break;
                        }
                    }
                }
            })
            .expect("Could not start LSP reader thread");
        let writer_side = Arc::downgrade(&transport);
        thread::Builder::new()
            .name("lsp-writer".into())
            .spawn(move || {
                let mut writer = writer;
                for value in write_rx {
                    let result = (|| -> Result<()> {
                        let body = serde_json::to_vec(&value)?;
                        write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
                        writer.write_all(&body)?;
                        writer.flush()?;
                        Ok(())
                    })();
                    if result.is_err() {
                        if let Some(transport) = writer_side.upgrade() {
                            transport.fail_pending();
                        }
                        break;
                    }
                }
            })
            .expect("Could not start LSP writer thread");
        (transport, rx)
    }

    /// Whether the reader thread is still running. Set when the server's
    /// stdout closes (clean exit or crash); the next write or request then
    /// fails fast through the usual paths.
    pub(crate) fn is_alive(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }

    /// How many malformed frames the reader skipped. Anything non-zero in a
    /// test means the fake server (or the framing code) is corrupt.
    #[cfg(test)]
    pub fn protocol_errors(&self) -> u64 {
        self.protocol_errors.load(Ordering::SeqCst)
    }

    /// Send a request and wait up to `timeout` for the matching response. A
    /// response that arrives after the timeout finds no pending entry and is
    /// dropped; the caller treats the call as failed and stays responsive.
    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.request_scoped(method, params, timeout, None)
    }

    pub fn request_scoped(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        scope: Option<&str>,
    ) -> Result<Value> {
        self.request_scoped_guarded(method, params, timeout, scope, || true)
    }

    pub fn request_scoped_guarded(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        scope: Option<&str>,
        valid: impl FnOnce() -> bool,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .expect("LSP pending map poisoned")
            .insert(id, (tx, scope.map(str::to_owned)));
        if let Err(error) = self.write(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })) {
            self.pending
                .lock()
                .expect("LSP pending map poisoned")
                .remove(&id);
            return Err(error);
        }
        // Covers an edit racing between the client's initial version check and enqueue.
        if !valid() {
            self.pending
                .lock()
                .expect("LSP pending map poisoned")
                .remove(&id);
            let _ = self.notify("$/cancelRequest", serde_json::json!({"id":id}));
            bail!("Language request cancelled: document changed");
        }
        match rx.recv_timeout(timeout) {
            Ok(response) => response,
            Err(_) => {
                self.pending
                    .lock()
                    .expect("LSP pending map poisoned")
                    .remove(&id);
                let _ = self.notify("$/cancelRequest", serde_json::json!({"id": id}));
                bail!("Language server request {method} timed out");
            }
        }
    }

    /// Fire-and-forget notification; the server never answers these.
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn write(&self, value: &Value) -> Result<()> {
        if !self.is_alive() {
            bail!("Language server exited");
        }
        self.writer.try_send(value.clone()).map_err(|_| {
            self.fail_pending();
            anyhow::anyhow!("Language server is unavailable or not reading messages; restart it")
        })
    }

    fn fail_pending(&self) {
        self.exited.store(true, Ordering::SeqCst);
        for (_, (tx, _)) in self
            .pending
            .lock()
            .expect("LSP pending map poisoned")
            .drain()
        {
            let _ = tx.try_send(Err(anyhow::anyhow!("Language server exited")));
        }
    }

    pub fn cancel_document(&self, uri: &str) {
        let cancelled: Vec<_> = {
            let mut pending = self.pending.lock().expect("LSP pending map poisoned");
            let ids: Vec<_> = pending
                .iter()
                .filter(|(_, (_, scope))| scope.as_deref() == Some(uri))
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id).map(|(tx, _)| (id, tx)))
                .collect()
        };
        for (id, tx) in cancelled {
            let _ = tx.try_send(Err(anyhow::anyhow!(
                "Language request cancelled: document changed"
            )));
            let _ = self.notify("$/cancelRequest", serde_json::json!({"id": id}));
        }
    }

    /// Route one parsed value. Returns false when the loop should stop.
    fn dispatch(&self, value: Value, sink: &mpsc::Sender<ServerMessage>) -> bool {
        if let (Some(id), Some(method)) =
            (value.get("id"), value.get("method").and_then(Value::as_str))
        {
            let response = match method {
                "workspace/configuration" => {
                    serde_json::json!({"jsonrpc":"2.0","id":id,"result": value.pointer("/params/items").and_then(Value::as_array).map(|items| vec![Value::Null; items.len()]).unwrap_or_default()})
                }
                "window/workDoneProgress/create" => {
                    serde_json::json!({"jsonrpc":"2.0","id":id,"result":null})
                }
                _ => {
                    serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client does not support this request"}})
                }
            };
            let _ = self.write(&response);
            return true;
        }
        let id = value.get("id").and_then(|id| id.as_u64());
        let method = value
            .get("method")
            .and_then(|method| method.as_str())
            .map(str::to_owned);
        match (id, method) {
            (Some(id), None) => {
                if let Some((tx, _)) = self
                    .pending
                    .lock()
                    .expect("LSP pending map poisoned")
                    .remove(&id)
                {
                    let response = match value.get("error") {
                        Some(error) => Err(anyhow::anyhow!("Language server error: {error}")),
                        None => value
                            .get("result")
                            .cloned()
                            .context("Language server response has no result"),
                    };
                    let _ = tx.send(response);
                }
                // Unknown ids are late responses to timed-out calls: drop.
                true
            }
            (_, Some(method)) => sink
                .send(ServerMessage::Notification {
                    method,
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                })
                .is_ok(),
            (None, None) => true,
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Some(child) = self._child.take() {
            thread::spawn(move || drop(child));
        }
    }
}

/// Read one framed message. `Ok(None)` is a clean EOF before any header byte.
pub(crate) fn read_frame(reader: &mut BufReader<impl Read>) -> Result<Option<Value>> {
    use std::io::BufRead as _;

    let mut content_length: Option<u64> = None;
    let mut header_bytes = 0;
    loop {
        let mut line = Vec::new();
        let bytes = std::io::Read::by_ref(reader)
            .take(8192)
            .read_until(b'\n', &mut line)?;
        header_bytes += bytes;
        anyhow::ensure!(
            header_bytes < 8192,
            "Language server headers exceed size bound"
        );
        if bytes == 0 {
            return Ok(None);
        }
        let line = String::from_utf8(line).context("Language server sent non-UTF-8 headers")?;
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(length) = line.strip_prefix("Content-Length:") {
            content_length = Some(
                length
                    .trim()
                    .parse()
                    .context("Language server sent an invalid Content-Length")?,
            );
        }
    }
    let length = content_length.context("Language server frame has no Content-Length")?;
    if length > MAX_MESSAGE_BYTES {
        bail!("Language server message exceeds the size bound");
    }
    let mut body = vec![0u8; length as usize];
    reader
        .read_exact(&mut body)
        .context("Language server frame truncated")?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
pub(crate) mod test_util {
    //! Byte-stream adapters over channels so tests run a scripted fake
    //! server on threads instead of spawning a binary.

    use super::*;

    pub(crate) struct ChannelReader {
        rx: mpsc::Receiver<Vec<u8>>,
        buffer: Vec<u8>,
        cursor: usize,
    }

    impl ChannelReader {
        pub(crate) fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
            Self {
                rx,
                buffer: Vec::new(),
                cursor: 0,
            }
        }
    }

    impl Read for ChannelReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            while self.cursor >= self.buffer.len() {
                // A dropped sender reads as EOF, like a closed pipe: returning
                // `Ok(0)` (rather than an error) lets the reader drain pending
                // calls and exit instead of spinning on failures.
                let Some(chunk) = self.rx.recv().ok() else {
                    return Ok(0);
                };
                self.buffer = chunk;
                self.cursor = 0;
                if self.buffer.is_empty() {
                    continue;
                }
            }
            let available = self.buffer.len() - self.cursor;
            let take = available.min(out.len());
            out[..take].copy_from_slice(&self.buffer[self.cursor..self.cursor + take]);
            self.cursor += take;
            Ok(take)
        }
    }

    pub(crate) struct ChannelWriter {
        tx: mpsc::Sender<Vec<u8>>,
    }

    impl ChannelWriter {
        pub(crate) fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
            Self { tx }
        }
    }

    impl Write for ChannelWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut message = Vec::with_capacity(bytes.len());
            message.extend_from_slice(bytes);
            self.tx
                .send(message)
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Encode one framed message, mirroring [`Transport::write`].
    pub(crate) fn encode_frame(value: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).unwrap();
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        frame
    }

    /// A stub language server for UI-level tests: answers `initialize` with
    /// UTF-16 capabilities, answers any other request with null, and ignores
    /// notifications. Anything fancier belongs in the client tests' fake.
    pub(crate) fn stub_server() -> (ChannelReader, ChannelWriter) {
        let (client_to_server_tx, client_to_server_rx) = mpsc::channel();
        let (server_to_client_tx, server_to_client_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(ChannelReader::new(client_to_server_rx));
            let mut writer = ChannelWriter::new(server_to_client_tx);
            loop {
                let frame = match super::read_frame(&mut reader) {
                    Ok(Some(value)) => value,
                    Ok(None) | Err(_) => break,
                };
                if frame.get("method").and_then(|m| m.as_str()) == Some("exit") {
                    break;
                }
                if let Some(id) = frame.get("id").and_then(|id| id.as_u64()) {
                    let result = if frame.get("method").and_then(|m| m.as_str())
                        == Some("initialize")
                    {
                        serde_json::json!({"capabilities": {"positionEncoding": "utf-16", "textDocumentSync": {"openClose": true, "change": 1}, "hoverProvider": true, "definitionProvider": true, "completionProvider": {}}})
                    } else {
                        Value::Null
                    };
                    let _ = writer.write_all(&encode_frame(&serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": result,
                    })));
                    let _ = writer.flush();
                }
            }
        });
        (
            ChannelReader::new(server_to_client_rx),
            ChannelWriter::new(client_to_server_tx),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    /// A scripted fake server: answers `echo` with its params, answers every
    /// server-to-client expectation implicitly, and records what it saw.
    fn echo_server() -> (
        ChannelReader,
        ChannelWriter,
        mpsc::Receiver<Value>,
        mpsc::Sender<bool>,
    ) {
        let (client_to_server_tx, client_to_server_rx) = mpsc::channel();
        let (server_to_client_tx, server_to_client_rx) = mpsc::channel();
        let (seen_tx, seen_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel::<bool>();
        thread::spawn(move || {
            let mut reader = BufReader::new(ChannelReader::new(client_to_server_rx));
            let mut writer = ChannelWriter::new(server_to_client_tx);
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                let frame = match read_frame(&mut reader) {
                    Ok(Some(value)) => value,
                    Ok(None) => break,
                    Err(_) => break,
                };
                let _ = seen_tx.send(frame.clone());
                if frame.get("method").and_then(|m| m.as_str()) == Some("exit") {
                    break;
                }
                if let Some(id) = frame.get("id").and_then(|id| id.as_u64())
                    && frame.get("method").is_some()
                {
                    let body = serde_json::to_vec(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": frame.get("params").cloned().unwrap_or(Value::Null),
                    }))
                    .unwrap();
                    let _ = write!(writer, "Content-Length: {}\r\n\r\n", body.len());
                    let _ = writer.write_all(&body);
                    let _ = writer.flush();
                }
            }
        });
        (
            ChannelReader::new(server_to_client_rx),
            ChannelWriter::new(client_to_server_tx),
            seen_rx,
            stop_tx,
        )
    }

    #[cfg(unix)]
    #[test]
    fn dropping_transport_kills_and_reaps_its_process() {
        let (transport, _) = Transport::spawn(
            Path::new("/bin/sh"),
            &["-c".into(), "sleep 30".into()],
            &std::env::temp_dir(),
        )
        .unwrap();
        let pid = transport._child.as_ref().unwrap().0.id() as i32;
        drop(transport);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "language server process was not reaped"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn full_writer_queue_fails_without_blocking_the_caller() {
        struct BlockedWriter(mpsc::Receiver<()>);
        impl Write for BlockedWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.recv();
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (release, blocked) = mpsc::channel();
        let (_out, input) = mpsc::channel();
        let (transport, _) = Transport::new(ChannelReader::new(input), BlockedWriter(blocked));
        let start = std::time::Instant::now();
        let failed = (0..140).any(|_| transport.notify("test", Value::Null).is_err());
        assert!(failed && !transport.is_alive());
        assert!(start.elapsed() < Duration::from_secs(1));
        release.send(()).ok();
    }

    #[test]
    fn cancelling_a_document_wakes_waiter_and_sends_cancel() {
        let (in_tx, in_rx) = mpsc::channel();
        let (_out_tx, out_rx) = mpsc::channel();
        let (transport, _) = Transport::new(ChannelReader::new(out_rx), ChannelWriter::new(in_tx));
        let mut reader = BufReader::new(ChannelReader::new(in_rx));
        let client = transport.clone();
        let waiter = thread::spawn(move || {
            client.request_scoped(
                "textDocument/hover",
                Value::Null,
                Duration::from_secs(5),
                Some("file:///a"),
            )
        });
        let request = read_frame(&mut reader).unwrap().unwrap();
        transport.cancel_document("file:///a");
        assert!(
            waiter
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        let cancel = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], request["id"]);
    }

    #[test]
    fn reader_does_not_keep_transport_alive_when_owner_drops() {
        let (in_tx, _in_rx) = mpsc::channel();
        let (_out_tx, out_rx) = mpsc::channel();
        let (transport, _) = Transport::new(ChannelReader::new(out_rx), ChannelWriter::new(in_tx));
        let weak = Arc::downgrade(&transport);
        drop(transport);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn request_round_trip_keeps_unicode_and_spaces_together() {
        let (reader, writer, seen, _stop) = echo_server();
        let (transport, _messages) = Transport::new(reader, writer);
        let params = serde_json::json!({"path": "/tmp/project space/src/বাংলা file.rs:12:3"});
        let result = transport
            .request("echo", params.clone(), Duration::from_secs(5))
            .unwrap();
        assert_eq!(result, params);
        let sent = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(sent.get("id").and_then(|id| id.as_u64()).is_some());
        assert_eq!(transport.protocol_errors(), 0);
    }

    #[test]
    fn notifications_arrive_without_ids() {
        let (reader, writer, seen, _stop) = echo_server();
        let (transport, messages) = Transport::new(reader, writer);
        transport.notify("exit", Value::Null).unwrap();
        let seen_exit = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            seen_exit.get("method").and_then(|m| m.as_str()),
            Some("exit")
        );
        // The fake server stops on `exit`; the reader then sees EOF and the
        // notification channel stays silent (no response exists for notifies).
        assert!(messages.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn timed_out_request_leaves_no_pending_entry() {
        let (_reader, writer, _seen, _stop) = echo_server();
        // A reader that never answers: the sender stays alive but silent, so
        // the reader blocks and the request must time out rather than hang.
        // (Dropping the sender would read as EOF and race the drain against
        // the insert; the crash path has its own test below.)
        let (_stalled_tx, stalled_rx) = mpsc::channel::<Vec<u8>>();
        let (transport, _messages) = Transport::new(ChannelReader::new(stalled_rx), writer);
        let error = transport
            .request("echo", Value::Null, Duration::from_millis(100))
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(transport.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn server_exit_fails_outstanding_requests() {
        let (_echo_reader, writer, _seen, _stop) = echo_server();
        // The transport reads from a channel we control; the writer still
        // feeds the echo server, whose replies go nowhere the transport can
        // see. Dropping our sender simulates the crash: the reader hits EOF
        // and every outstanding call fails fast instead of timing out.
        let (crash_tx, crash_rx) = mpsc::channel::<Vec<u8>>();
        let (transport, _messages) = Transport::new(ChannelReader::new(crash_rx), writer);
        let caller = Arc::clone(&transport);
        let handle =
            thread::spawn(move || caller.request("echo", Value::Null, Duration::from_secs(30)));
        assert!(transport.is_alive());
        thread::sleep(Duration::from_millis(200));
        drop(crash_tx);
        let error = handle.join().expect("request thread panicked").unwrap_err();
        assert!(error.to_string().contains("exited"));
        assert!(!transport.is_alive());
    }
}
