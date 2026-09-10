//! Bounded local metadata subprocesses. Never submits prompts or turns.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

const LIMIT: u64 = 32 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn output(command: &mut Command) -> Result<Vec<u8>> {
    let mut child = Process(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting local session reader")?,
    );
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    let bytes = rx
        .recv_timeout(TIMEOUT)
        .context("session listing timed out")??;
    if bytes.len() as u64 > LIMIT {
        bail!("Session listing exceeded the metadata limit");
    }
    // EOF may precede process exit. Bound that wait too.
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                bail!("Session reader exited with {status}");
            }
            break;
        }
        if start.elapsed() > TIMEOUT {
            bail!("Session reader did not exit");
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(bytes)
}

pub(super) struct Rpc {
    child: Process,
    lines: mpsc::Receiver<Result<Value>>,
    next: u64,
}
impl Rpc {
    pub fn new(command: &mut Command) -> Result<Self> {
        let mut child = Process(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let stdout = child.0.stdout.take().unwrap();
        let (tx, lines) = mpsc::sync_channel(32);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut bytes = Vec::new();
                let result = (&mut reader).take(LIMIT + 1).read_until(b'\n', &mut bytes);
                if matches!(result, Ok(0)) {
                    break;
                }
                // Launcher shims may print a notice ahead of the protocol
                // stream. JSON-RPC messages are objects, so skip blank and
                // preamble lines instead of failing the whole metadata scan.
                // Read errors still fall through and fail fast below.
                if result.is_ok()
                    && !bytes
                        .iter()
                        .find(|byte| !byte.is_ascii_whitespace())
                        .is_some_and(|byte| *byte == b'{')
                {
                    continue;
                }
                let value = result.map_err(anyhow::Error::from).and_then(|_| {
                    if bytes.len() as u64 > LIMIT {
                        bail!("Oversized session metadata response");
                    }
                    Ok(serde_json::from_slice(&bytes)?)
                });
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut rpc = Self {
            child,
            lines,
            next: 0,
        };
        rpc.call(
            "initialize",
            serde_json::json!({"clientInfo":{"name":"devcroft","version":"0.1.0"}}),
        )?;
        rpc.write(serde_json::json!({"method":"initialized"}))?;
        Ok(rpc)
    }
    fn write(&mut self, value: Value) -> Result<()> {
        let stdin = self.child.0.stdin.as_mut().unwrap();
        serde_json::to_writer(&mut *stdin, &value)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        self.write(serde_json::json!({"id":self.next,"method":method,"params":params}))?;
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            let value = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .context("Codex session metadata timed out")??;
            if value["id"].as_u64() != Some(self.next) {
                continue;
            }
            if value.get("error").is_some() {
                bail!("Codex rejected session metadata request {method}");
            }
            return value
                .get("result")
                .cloned()
                .context("Missing Codex metadata result");
        }
    }
}
