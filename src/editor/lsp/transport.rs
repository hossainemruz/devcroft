//! Length-framed JSON-RPC transport over a language server's stdio.
//!
//! The wire format follows the LSP base protocol (`Content-Length: N\r\n\r\n`
//! framing around JSON-RPC 2.0 payloads). This is intentionally separate from
//! the line-delimited helper in `crate::agent_sessions`, which speaks a
//! different framing for a different server.
//!
//! The transport owns no language semantics: it moves framed values, matches
//! responses to request ids, answers server-to-client requests with a null
//! result, and forwards anything else as [`ServerMessage`]. Timeouts and
//! stale-response handling live one layer up, in the client.

use std::{
    collections::HashMap,
    ffi::OsString,
    io::{BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
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
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

type Pending = Mutex<HashMap<u64, mpsc::SyncSender<Result<Value>>>>;

pub struct Transport {
    writer: Mutex<Box<dyn Write + Send>>,
    pending: Pending,
    next_id: AtomicU64,
    protocol_errors: AtomicU64,
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
        let mut child = WatchedChild(
            Command::new(program)
                .args(args)
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
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
        let transport = Arc::new(Self {
            writer: Mutex::new(Box::new(writer)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            protocol_errors: AtomicU64::new(0),
            _child: child,
        });
        let (tx, rx) = mpsc::channel();
        let reader_side = Arc::clone(&transport);
        thread::Builder::new()
            .name("lsp-reader".to_owned())
            .spawn(move || reader_side.read_loop(reader, tx))
            .expect("Could not start LSP reader thread");
        (transport, rx)
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
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .expect("LSP pending map poisoned")
            .insert(id, tx);
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
        match rx.recv_timeout(timeout) {
            Ok(response) => response,
            Err(_) => {
                self.pending
                    .lock()
                    .expect("LSP pending map poisoned")
                    .remove(&id);
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
        let body = serde_json::to_vec(value)?;
        let mut writer = self.writer.lock().expect("LSP writer poisoned");
        write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
        writer.write_all(&body)?;
        writer.flush()?;
        Ok(())
    }

    fn read_loop(&self, reader: impl Read, sink: mpsc::Sender<ServerMessage>) {
        let mut reader = BufReader::new(reader);
        loop {
            let frame = match read_frame(&mut reader) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(_) => {
                    // A corrupt frame desynchronizes nothing: the next read
                    // re-syncs on the following Content-Length header, so
                    // count the incident and keep reading.
                    self.protocol_errors.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
            };
            if !self.dispatch(frame, &sink) {
                break;
            }
        }
        // The server is gone (or nobody listens): wake every outstanding
        // caller so a crashed server can never hang the UI thread's task.
        let mut pending = self.pending.lock().expect("LSP pending map poisoned");
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(anyhow::anyhow!("Language server exited")));
        }
    }

    /// Route one parsed value. Returns false when the loop should stop.
    fn dispatch(&self, value: Value, sink: &mpsc::Sender<ServerMessage>) -> bool {
        let id = value.get("id").and_then(|id| id.as_u64());
        let method = value
            .get("method")
            .and_then(|method| method.as_str())
            .map(str::to_owned);
        match (id, method) {
            (Some(id), Some(_method)) => {
                // A server-to-client request (workspace/configuration,
                // window/workDoneProgress/create, ...). The spike answers
                // with a null result, which the protocol accepts as "no
                // configuration / default handling".
                let _ = self.write(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": Value::Null,
                }));
                true
            }
            (Some(id), None) => {
                if let Some(tx) = self
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
            (None, Some(method)) => sink
                .send(ServerMessage::Notification {
                    method,
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                })
                .is_ok(),
            (None, None) => true,
        }
    }
}

/// Read one framed message. `Ok(None)` is a clean EOF before any header byte.
pub(crate) fn read_frame(reader: &mut BufReader<impl Read>) -> Result<Option<Value>> {
    use std::io::BufRead as _;

    let mut content_length: Option<u64> = None;
    loop {
        let mut line = Vec::new();
        let bytes = reader.read_until(b'\n', &mut line)?;
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
        thread::sleep(Duration::from_millis(200));
        drop(crash_tx);
        let error = handle.join().expect("request thread panicked").unwrap_err();
        assert!(error.to_string().contains("exited"));
    }
}
