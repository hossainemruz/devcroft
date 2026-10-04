//! One bounded workspace client with capability-gated requests, per-document
//! versions, negotiated UTF-16 positions, full or whole-range incremental
//! synchronization, and diagnostic forwarding. Providers run requests off
//! the UI thread; notification writes are queued by the transport.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::Value;

use super::transport::{ServerMessage, Transport};

/// How long `initialize` may take; cold servers compile indices first.
const INIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for one interactive request (hover, completion, definition).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Diagnostics pushed by the server, forwarded to the UI thread for
/// application into the editor's diagnostic set.
pub struct DiagnosticEvent {
    pub uri: String,
    pub version: Option<i32>,
    pub diagnostics: Vec<lsp_types::Diagnostic>,
}

struct DocState {
    version: i32,
    text: String,
}

pub struct Client {
    transport: Arc<Transport>,
    docs: Mutex<HashMap<String, DocState>>,
    capabilities: Value,
    next_version: AtomicI32,
    position_encoding: Mutex<String>,
    last_error: Mutex<Option<String>>,
    shut_down: AtomicBool,
    _root: PathBuf,
}

impl Client {
    /// Spawn `program` rooted at `cwd`, run the `initialize` handshake, and
    /// forward `publishDiagnostics` notifications to `diagnostics`.
    pub fn start(
        program: &Path,
        cwd: &Path,
        diagnostics: async_channel::Sender<DiagnosticEvent>,
    ) -> Result<Arc<Self>> {
        let (transport, messages) = Transport::spawn(program, &[], cwd)?;
        Self::handshake(transport, cwd, messages, diagnostics)
    }

    /// Test seam: run the handshake over an already-open transport (channel
    /// adapters plus a stub server) instead of spawning a binary.
    pub(crate) fn handshake(
        transport: Arc<Transport>,
        cwd: &Path,
        messages: mpsc::Receiver<ServerMessage>,
        diagnostics: async_channel::Sender<DiagnosticEvent>,
    ) -> Result<Arc<Self>> {
        let root_uri = url_of(cwd)?;
        let response = transport.request(
            "initialize",
            serde_json::json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "initializationOptions": {"checkOnSave": false, "cargo": {"buildScripts": {"enable": false}}, "procMacro": {"enable": false}},
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "completion": {},
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "definition": {},
                        "publishDiagnostics": {
                            "relatedInformation": true,
                            "versionSupport": true,
                        },
                    },
                },
                "clientInfo": { "name": "devcroft", "version": "0.1.0" },
            }),
            INIT_TIMEOUT,
        )?;
        let encoding = response
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("positionEncoding"))
            .and_then(|encoding| encoding.as_str())
            .unwrap_or("utf-16")
            .to_owned();
        anyhow::ensure!(
            encoding == "utf-16",
            "Unsupported server position encoding: {encoding}; expected utf-16"
        );
        let capabilities = response.get("capabilities").cloned().unwrap_or(Value::Null);
        transport.notify("initialized", serde_json::json!({}))?;

        let client = Arc::new(Self {
            transport,
            docs: Mutex::new(HashMap::new()),
            capabilities,
            next_version: AtomicI32::new(1),
            position_encoding: Mutex::new(encoding),
            last_error: Mutex::new(None),
            shut_down: AtomicBool::new(false),
            _root: cwd.to_owned(),
        });
        thread::Builder::new()
            .name("lsp-diagnostics".to_owned())
            .spawn(move || {
                for message in messages {
                    if let ServerMessage::Notification { method, params } = message
                        && method == "textDocument/publishDiagnostics"
                        && let Ok(event) = parse_diagnostics(&params)
                    {
                        let _ = diagnostics.send_blocking(event);
                    }
                }
            })
            .context("Could not start LSP diagnostics thread")?;
        Ok(client)
    }

    /// The client advertises UTF-16 only and rejects incompatible servers.
    pub fn position_encoding(&self) -> String {
        self.position_encoding
            .lock()
            .expect("LSP state poisoned")
            .clone()
    }

    /// Latest transport-level failure, for status surfaces and tests.
    /// Plain editing stays available while the status surface reports failures.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().expect("LSP state poisoned").clone()
    }

    /// Whether the server connection is still up. A crashed server keeps
    /// degrading gracefully, and reopening the file restarts it.
    pub(crate) fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }

    fn record_error(&self, error: &anyhow::Error) {
        *self.last_error.lock().expect("LSP state poisoned") = Some(format!("{error:#}"));
    }

    pub fn doc_version(&self, uri: &str) -> i32 {
        self.docs
            .lock()
            .expect("LSP state poisoned")
            .get(uri)
            .map_or(0, |doc| doc.version)
    }

    pub fn supports(&self, method: &str) -> bool {
        let name = match method {
            "textDocument/hover" => "hoverProvider",
            "textDocument/completion" => "completionProvider",
            "textDocument/definition" => "definitionProvider",
            _ => return false,
        };
        self.capabilities
            .get(name)
            .is_some_and(|v| v == true || v.is_object())
    }

    fn sync_kind(&self) -> i64 {
        let sync = &self.capabilities["textDocumentSync"];
        sync.as_i64()
            .or_else(|| sync["change"].as_i64())
            .unwrap_or(0)
    }

    fn open_close(&self) -> bool {
        let sync = &self.capabilities["textDocumentSync"];
        sync.is_number() && self.sync_kind() != 0 || sync["openClose"] == true
    }

    /// Each buffer stays open while its tab exists, including inactive drafts.
    pub fn did_open(&self, uri: &lsp_types::Uri, language_id: &str, text: &str) -> Result<()> {
        anyhow::ensure!(self.is_alive(), "Language server exited; restart it");
        let mut docs = self.docs.lock().expect("LSP state poisoned");
        if docs.contains_key(uri.as_str()) {
            return Ok(());
        }
        let version = self.next_version.fetch_add(1, Ordering::SeqCst);
        docs.insert(
            uri.to_string(),
            DocState {
                version,
                text: text.to_owned(),
            },
        );
        if self.open_close() {
            self.transport.notify("textDocument/didOpen", serde_json::json!({
                "textDocument": { "uri":uri, "languageId":language_id, "version":version, "text":text }
            })).inspect_err(|e| self.record_error(e))?;
        }
        Ok(())
    }

    pub fn did_change(&self, uri: &str, text: &str) -> Result<i32> {
        anyhow::ensure!(self.is_alive(), "Language server exited; restart it");
        let mut docs = self.docs.lock().expect("LSP state poisoned");
        let doc = docs.get_mut(uri).context("Document is not open")?;
        if doc.text == text {
            return Ok(doc.version);
        }
        self.transport.cancel_document(uri);
        doc.version = self.next_version.fetch_add(1, Ordering::SeqCst);
        // Incremental-only servers receive a valid range replacing the old text.
        let end = end_position(&doc.text);
        doc.text = text.to_owned();
        let change = match self.sync_kind() {
            1 => serde_json::json!({"text":text}),
            2 => {
                serde_json::json!({"range":{"start":{"line":0,"character":0},"end":end},"text":text})
            }
            _ => return Ok(doc.version),
        };
        self.transport
            .notify(
                "textDocument/didChange",
                serde_json::json!({
                    "textDocument":{"uri":uri,"version":doc.version}, "contentChanges":[change]
                }),
            )
            .inspect_err(|e| self.record_error(e))?;
        Ok(doc.version)
    }

    pub fn did_save(&self, uri: &str) -> Result<()> {
        let save = &self.capabilities["textDocumentSync"]["save"];
        if save != true && !save.is_object() {
            return Ok(());
        }
        let docs = self.docs.lock().expect("LSP state poisoned");
        let Some(doc) = docs.get(uri) else {
            return Ok(());
        };
        let mut params = serde_json::json!({"textDocument":{"uri":uri}});
        if save["includeText"] == true {
            params["text"] = doc.text.clone().into();
        }
        self.transport
            .notify("textDocument/didSave", params)
            .inspect_err(|e| self.record_error(e))
    }

    pub fn did_close(&self, uri: &str) {
        self.transport.cancel_document(uri);
        if self
            .docs
            .lock()
            .expect("LSP state poisoned")
            .remove(uri)
            .is_some()
            && self.open_close()
        {
            let _ = self.transport.notify(
                "textDocument/didClose",
                serde_json::json!({"textDocument":{"uri":uri}}),
            );
        }
    }

    pub fn hover(
        &self,
        uri: &lsp_types::Uri,
        version: i32,
        position: lsp_types::Position,
    ) -> Result<Option<lsp_types::Hover>> {
        let result = self.request_at(
            "textDocument/hover",
            uri,
            version,
            position,
            REQUEST_TIMEOUT,
        )?;
        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value(result).context("Language server sent an invalid hover")
    }

    pub fn definition(
        &self,
        uri: &lsp_types::Uri,
        version: i32,
        position: lsp_types::Position,
    ) -> Result<Vec<lsp_types::LocationLink>> {
        let result = self.request_at(
            "textDocument/definition",
            uri,
            version,
            position,
            REQUEST_TIMEOUT,
        )?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        let response: lsp_types::GotoDefinitionResponse =
            serde_json::from_value(result).context("Language server sent an invalid definition")?;
        Ok(match response {
            lsp_types::GotoDefinitionResponse::Scalar(location) => {
                vec![lsp_types::LocationLink {
                    origin_selection_range: None,
                    target_uri: location.uri,
                    target_range: location.range,
                    target_selection_range: location.range,
                }]
            }
            lsp_types::GotoDefinitionResponse::Array(locations) => locations
                .into_iter()
                .map(|location| lsp_types::LocationLink {
                    origin_selection_range: None,
                    target_uri: location.uri,
                    target_range: location.range,
                    target_selection_range: location.range,
                })
                .collect(),
            lsp_types::GotoDefinitionResponse::Link(links) => links,
        })
    }

    pub fn completion(
        &self,
        uri: &lsp_types::Uri,
        version: i32,
        position: lsp_types::Position,
    ) -> Result<lsp_types::CompletionResponse> {
        let result = self.request_at(
            "textDocument/completion",
            uri,
            version,
            position,
            REQUEST_TIMEOUT,
        )?;
        if result.is_null() {
            return Ok(lsp_types::CompletionResponse::Array(Vec::new()));
        }
        serde_json::from_value(result).context("Language server sent an invalid completion")
    }

    /// Best-effort `shutdown` + `exit`. The transport's drop kills and reaps
    /// the child regardless, so this path only aims for a clean goodbye.
    #[cfg(test)]
    pub fn shutdown(&self) {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self
            .transport
            .request("shutdown", Value::Null, Duration::from_secs(3));
        let _ = self.transport.notify("exit", Value::Null);
    }

    fn request_at(
        &self,
        method: &str,
        uri: &lsp_types::Uri,
        version: i32,
        position: lsp_types::Position,
        timeout: Duration,
    ) -> Result<Value> {
        if !self.supports(method) {
            return Ok(Value::Null);
        }
        anyhow::ensure!(
            self.doc_version(uri.as_str()) == version && version > 0,
            "Discarded a stale document request"
        );
        let response = self
            .transport
            .request_scoped_guarded(
                method,
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": position,
                }),
                timeout,
                Some(uri.as_str()),
                || self.doc_version(uri.as_str()) == version,
            )
            .inspect_err(|error| {
                if !error.to_string().contains("cancelled") {
                    self.record_error(error);
                }
            })?;
        if self.doc_version(uri.as_str()) != version {
            let stale = anyhow::anyhow!("Discarded a stale {method} response (document moved on)");
            return Err(stale);
        }
        *self.last_error.lock().expect("LSP state poisoned") = None;
        Ok(response)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if !self.shut_down.swap(true, Ordering::SeqCst) {
            let transport = self.transport.clone();
            thread::spawn(move || {
                let _ = transport.request("shutdown", Value::Null, Duration::from_secs(1));
                let _ = transport.notify("exit", Value::Null);
            });
        }
    }
}

fn end_position(text: &str) -> lsp_types::Position {
    let line = text.bytes().filter(|b| *b == b'\n').count() as u32;
    let character = text
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .encode_utf16()
        .count() as u32;
    lsp_types::Position::new(line, character)
}

fn url_of(path: &Path) -> Result<lsp_types::Uri> {
    super::file_uri(path)
}

fn parse_diagnostics(params: &Value) -> Result<DiagnosticEvent> {
    let parsed: lsp_types::PublishDiagnosticsParams = serde_json::from_value(params.clone())
        .context("Language server sent invalid diagnostics")?;
    Ok(DiagnosticEvent {
        uri: parsed.uri.to_string(),
        version: parsed.version,
        diagnostics: parsed.diagnostics,
    })
}

use std::thread;

#[cfg(test)]
mod tests {
    use super::super::transport::test_util::*;
    use super::*;
    use std::io::{BufReader, Write as _};

    /// A scripted fake language server: answers `initialize` with UTF-16
    /// capabilities, records every method it sees, serves one hover and one
    /// definition, publishes one diagnostic after `didOpen`, and answers
    /// `shutdown`.
    fn fake_server(seen: mpsc::Sender<String>) -> (ChannelReader, ChannelWriter) {
        let (client_to_server_tx, client_to_server_rx) = mpsc::channel();
        let (server_to_client_tx, server_to_client_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(ChannelReader::new(client_to_server_rx));
            let mut writer = ChannelWriter::new(server_to_client_tx);
            let mut opened = false;
            loop {
                let frame = match super::super::transport::read_frame(&mut reader) {
                    Ok(Some(value)) => value,
                    Ok(None) | Err(_) => break,
                };
                let method = frame
                    .get("method")
                    .and_then(|method| method.as_str())
                    .unwrap_or("")
                    .to_owned();
                let _ = seen.send(method.clone());
                let id = frame.get("id").and_then(|id| id.as_u64());
                let reply = |writer: &mut ChannelWriter, id: u64, result: Value| {
                    let _ = writer.write_all(&encode_frame(&serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": result,
                    })));
                    let _ = writer.flush();
                };
                match (method.as_str(), id) {
                    ("initialize", Some(id)) => reply(
                        &mut writer,
                        id,
                        serde_json::json!({"capabilities": {"positionEncoding": "utf-16", "textDocumentSync": {"openClose": true, "change": 1, "save": true}, "hoverProvider": true, "definitionProvider": true, "completionProvider": {}}}),
                    ),
                    ("shutdown", Some(id)) => reply(&mut writer, id, Value::Null),
                    ("textDocument/hover", Some(id)) => reply(
                        &mut writer,
                        id,
                        serde_json::json!({"contents": {"kind": "markdown", "value": "fake hover"}}),
                    ),
                    ("textDocument/definition", Some(id)) => reply(
                        &mut writer,
                        id,
                        serde_json::json!([{
                            "targetUri": "file:///tmp/project%20space/src/%E0%A6%AC%E0%A6%BE%E0%A6%82%E0%A6%B2%E0%A6%BE%20file.rs",
                            "targetRange": {"start": {"line": 11, "character": 2}, "end": {"line": 11, "character": 8}},
                            "targetSelectionRange": {"start": {"line": 11, "character": 2}, "end": {"line": 11, "character": 8}},
                        }]),
                    ),
                    ("textDocument/completion", Some(id)) => reply(
                        &mut writer,
                        id,
                        serde_json::json!({"isIncomplete": false, "items": []}),
                    ),
                    _ => {}
                }
                if method == "textDocument/didOpen" && !opened {
                    opened = true;
                    let _ = writer.write_all(&encode_frame(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": "file:///tmp/project%20space/src/%E0%A6%AC%E0%A6%82%E0%A6%B2%E0%A6%BE%20file.rs",
                            "version": 1,
                            "diagnostics": [{
                                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 6}},
                                "severity": 1,
                                "message": "fake diagnostic",
                            }],
                        },
                    })));
                    let _ = writer.flush();
                }
                if method == "exit" {
                    break;
                }
            }
        });
        (
            ChannelReader::new(server_to_client_rx),
            ChannelWriter::new(client_to_server_tx),
        )
    }

    fn start_client() -> (
        Arc<Client>,
        async_channel::Receiver<DiagnosticEvent>,
        mpsc::Receiver<String>,
    ) {
        let (seen_tx, seen_rx) = mpsc::channel();
        let (reader, writer) = fake_server(seen_tx);
        let (transport, messages) = Transport::new(reader, writer);
        let (diagnostics_tx, diagnostics_rx) = async_channel::unbounded();
        let dir = tempfile::tempdir().unwrap();
        // Transport::spawn is bypassed (channel pair instead of a child), so
        // replicate the handshake entry with the same constructor the
        // production path uses.
        let client = Client::handshake(transport, dir.path(), messages, diagnostics_tx).unwrap();
        // Keep the tempdir alive for the client's lifetime is unnecessary:
        // only the URI string matters after the handshake.
        (client, diagnostics_rx, seen_rx)
    }

    #[test]
    fn handshake_negotiates_utf16_and_opens_documents() {
        let (client, diagnostics, seen) = start_client();
        assert_eq!(client.position_encoding(), "utf-16");
        let uri: lsp_types::Uri =
            "file:///tmp/project%20space/src/%E0%A6%AC%E0%A6%82%E0%A6%B2%E0%A6%BE%20file.rs"
                .parse()
                .unwrap();
        client.did_open(&uri, "rust", "fn main() {}\n").unwrap();
        assert_eq!(client.doc_version(uri.as_str()), 1);
        let version = client.did_change(uri.as_str(), "fn main() { }\n").unwrap();
        assert_eq!(version, 2);

        let hover = client
            .hover(&uri, version, lsp_types::Position::new(0, 3))
            .unwrap()
            .expect("fake server answers hover");
        match hover.contents {
            lsp_types::HoverContents::Markup(markup) => assert_eq!(markup.value, "fake hover"),
            other => panic!("unexpected hover contents: {other:?}"),
        }

        let definitions = client
            .definition(&uri, version, lsp_types::Position::new(0, 3))
            .unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].target_selection_range.start.line, 11);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let event = loop {
            if let Ok(event) = diagnostics.try_recv() {
                break event;
            }
            if std::time::Instant::now() > deadline {
                panic!("fake server publishes diagnostics");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(event.version, Some(1));
        assert_eq!(event.diagnostics.len(), 1);
        assert_eq!(event.diagnostics[0].message, "fake diagnostic");

        let methods: Vec<String> = seen.try_iter().collect();
        for expected in [
            "initialize",
            "initialized",
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/hover",
            "textDocument/definition",
        ] {
            assert!(
                methods.contains(&expected.to_owned()),
                "missing {expected}: {methods:?}"
            );
        }
        client.shutdown();
    }

    #[test]
    fn stale_responses_are_discarded_after_an_edit() {
        let (client, _diagnostics, _seen) = start_client();
        let uri: lsp_types::Uri = "file:///tmp/file.rs".parse().unwrap();
        client.did_open(&uri, "rust", "fn a() {}\n").unwrap();
        // Capture version 1, then move the document on before answering.
        client.did_change(uri.as_str(), "fn a() { }\n").unwrap();
        let error = client
            .hover(&uri, 1, lsp_types::Position::new(0, 0))
            .unwrap_err();
        assert!(error.to_string().contains("stale"));
        assert!(
            client.last_error().is_none(),
            "Stale requests are normal, not a server failure"
        );
    }

    fn configured_client(capabilities: Value) -> (Arc<Client>, mpsc::Receiver<Value>) {
        let (in_tx, in_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let (seen_tx, seen_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(ChannelReader::new(in_rx));
            let mut writer = ChannelWriter::new(out_tx);
            while let Ok(Some(frame)) = super::super::transport::read_frame(&mut reader) {
                seen_tx.send(frame.clone()).ok();
                if let Some(id) = frame.get("id") {
                    let result = if frame["method"] == "initialize" {
                        serde_json::json!({"capabilities":capabilities})
                    } else {
                        Value::Null
                    };
                    writer
                        .write_all(&encode_frame(
                            &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                        ))
                        .unwrap();
                }
                if frame["method"] == "exit" {
                    break;
                }
            }
        });
        let (transport, messages) =
            Transport::new(ChannelReader::new(out_rx), ChannelWriter::new(in_tx));
        let (tx, _) = async_channel::unbounded();
        (
            Client::handshake(transport, &std::env::temp_dir(), messages, tx).unwrap(),
            seen_rx,
        )
    }

    #[test]
    fn multiple_documents_incremental_sync_save_close_and_reopen() {
        let (client, seen) = configured_client(
            serde_json::json!({"textDocumentSync":{"openClose":true,"change":2,"save":{"includeText":true}}}),
        );
        let first: lsp_types::Uri = "file:///tmp/first.rs".parse().unwrap();
        let second: lsp_types::Uri = "file:///tmp/second.rs".parse().unwrap();
        client.did_open(&first, "rust", "a🎉\n終").unwrap();
        client.did_open(&second, "rust", "second").unwrap();
        client
            .did_open(&first, "rust", "ignored duplicate")
            .unwrap();
        let version = client.did_change(first.as_str(), "new").unwrap();
        assert_eq!(client.doc_version(second.as_str()), 2);
        client.did_save(first.as_str()).unwrap();
        client.did_close(first.as_str());
        assert_eq!(client.doc_version(first.as_str()), 0);
        client.did_open(&first, "rust", "reopened").unwrap();
        assert!(client.doc_version(first.as_str()) > version);
        client.shutdown(); // response is a barrier for earlier outbound notifications
        let frames: Vec<_> = seen.try_iter().collect();
        assert_eq!(
            frames
                .iter()
                .filter(|f| f["method"] == "textDocument/didOpen")
                .count(),
            3
        );
        let change = frames
            .iter()
            .find(|f| f["method"] == "textDocument/didChange")
            .unwrap();
        assert_eq!(
            change["params"]["contentChanges"][0]["range"]["end"],
            serde_json::json!({"line":1,"character":1})
        );
        let save = frames
            .iter()
            .find(|f| f["method"] == "textDocument/didSave")
            .unwrap();
        assert_eq!(save["params"]["text"], "new");
        assert!(
            frames
                .iter()
                .any(|f| f["method"] == "textDocument/didClose")
        );
        assert_eq!(end_position("a🎉"), lsp_types::Position::new(0, 3));
    }

    #[test]
    fn unsupported_capabilities_do_not_send_requests_or_notifications() {
        let (client, seen) = configured_client(serde_json::json!({}));
        let uri: lsp_types::Uri = "file:///tmp/a.rs".parse().unwrap();
        client.did_open(&uri, "rust", "a").unwrap();
        let version = client.did_change(uri.as_str(), "b").unwrap();
        assert!(
            client
                .hover(&uri, version, lsp_types::Position::new(0, 0))
                .unwrap()
                .is_none()
        );
        client.did_save(uri.as_str()).unwrap();
        client.did_close(uri.as_str());
        client.shutdown();
        assert!(!seen.try_iter().any(|f| {
            f["method"]
                .as_str()
                .is_some_and(|m| m.starts_with("textDocument/"))
        }));
    }

    /// Live integration smoke test against a real rust-analyzer. Ignored by
    /// default (needs the binary and seconds of analysis time); run
    /// explicitly with `cargo test live_rust_analyzer -- --ignored`.
    #[test]
    #[ignore]
    fn live_rust_analyzer_hover_and_definition() {
        let Some(program) = super::super::discover_rust_analyzer(None) else {
            panic!("rust-analyzer not found: `rustup component add rust-analyzer`");
        };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"lsp-smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let source = "/// Adds one.\nfn add_one(x: i32) -> i32 {\n    x + 1\n}\n\nfn main() {\n    let _ = add_one(41);\n}\n";
        let file = dir.path().join("src").join("main.rs");
        std::fs::write(&file, source).unwrap();
        let uri = super::super::file_uri(&file).unwrap();

        let (diagnostics_tx, diagnostics_rx) = async_channel::unbounded();
        let client = Client::start(&program, dir.path(), diagnostics_tx).unwrap();
        assert_eq!(client.position_encoding(), "utf-16");
        client.did_open(&uri, "rust", source).unwrap();

        // Cold analysis can lag the first request; poll briefly rather than
        // flaking on a single cold call.
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        let hover = loop {
            let result = client.hover(&uri, 1, lsp_types::Position::new(6, 14));
            if let Ok(Some(hover)) = result {
                break hover;
            }
            if std::time::Instant::now() > deadline {
                panic!("rust-analyzer never answered hover: {:?}", result.err());
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        let text = match hover.contents {
            lsp_types::HoverContents::Markup(markup) => markup.value,
            lsp_types::HoverContents::Array(marked) => format!("{marked:?}"),
            lsp_types::HoverContents::Scalar(marked) => format!("{marked:?}"),
        };
        assert!(
            text.contains("Adds one"),
            "hover carries the doc comment: {text}"
        );

        let definitions = client
            .definition(&uri, 1, lsp_types::Position::new(6, 14))
            .unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].target_selection_range.start.line, 1);
        let completion = client
            .completion(&uri, 1, lsp_types::Position::new(6, 17))
            .unwrap();
        let items = match completion {
            lsp_types::CompletionResponse::Array(items) => items,
            lsp_types::CompletionResponse::List(list) => list.items,
        };
        assert!(
            !items.is_empty(),
            "rust-analyzer supplies semantic completion"
        );
        let version = client.did_change(uri.as_str(), "fn main( {\n").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(event) = diagnostics_rx.try_recv()
                && event.uri == uri.as_str()
                && event.version == Some(version)
                && !event.diagnostics.is_empty()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "rust-analyzer did not publish current syntax diagnostics"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        client.did_close(uri.as_str());
        client.shutdown();
    }
}
