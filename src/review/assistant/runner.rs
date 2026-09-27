//! Transport for dedicated, read-only review conversations.
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_channel::Sender;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Provider {
    #[default]
    Codex,
    Claude,
    Opencode,
    Omp,
}
impl Provider {
    /// Every harness the review assistant can drive, in
    /// [`AgentKind::ALL`] display order.
    pub const ALL: [Self; 4] = [Self::Opencode, Self::Claude, Self::Codex, Self::Omp];
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Opencode => "Opencode",
            Self::Omp => "Omp",
        }
    }
    fn command(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Omp => "omp",
        }
    }
    fn from_agent(agent: crate::agent::AgentKind) -> Self {
        match agent {
            crate::agent::AgentKind::Codex => Self::Codex,
            crate::agent::AgentKind::Claude => Self::Claude,
            crate::agent::AgentKind::Opencode => Self::Opencode,
            crate::agent::AgentKind::Omp => Self::Omp,
        }
    }
    /// Providers the user left enabled (Settings > Agent), in display order.
    /// Never empty: unresolvable or fully-disabled state falls back to every
    /// harness so the assistant cannot dead-end.
    pub fn enabled_providers() -> Vec<Self> {
        let agents = crate::data::resolve_data_root()
            .ok()
            .and_then(|root| crate::data::DeviceStore::new(&root).load().ok())
            .map(|state| state.enabled_agents_or_default())
            .unwrap_or_else(|| crate::agent::AgentKind::ALL.to_vec());
        if agents.is_empty() {
            return Self::ALL.to_vec();
        }
        agents.into_iter().map(Self::from_agent).collect()
    }
    /// Whether reasoning effort maps to a CLI flag for this provider.
    /// Opencode's `run` has no effort control, so the setup control is
    /// disabled when it is selected rather than silently accepted.
    pub(crate) fn supports_effort(self) -> bool {
        !matches!(self, Self::Opencode)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Effort {
    #[default]
    Default,
    Low,
    Medium,
    High,
    Xhigh,
}
impl Effort {
    pub const ALL: [Self; 5] = [
        Self::Default,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Agent default",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Xhigh => "Extra high",
        }
    }
    fn argument(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Xhigh => Some("xhigh"),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Options {
    pub provider: Provider,
    pub model: String,
    pub effort: Effort,
    pub include_concepts: bool,
}
impl Options {
    pub fn load() -> Self {
        crate::data::resolve_data_root()
            .ok()
            .and_then(|root| crate::data::DeviceStore::new(&root).load().ok())
            .and_then(|state| state.extra.get("review_assistant").cloned())
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) -> anyhow::Result<()> {
        let root = crate::data::resolve_data_root()?;
        let value = serde_json::to_value(self)?;
        crate::data::DeviceStore::new(&root).update(|state| {
            state.extra.insert("review_assistant".into(), value);
        })
    }
    pub fn label(&self) -> String {
        format!(
            "{} · {} · {}",
            self.provider.label(),
            if self.model.is_empty() {
                "default model"
            } else {
                &self.model
            },
            self.effort.label()
        )
    }
}

pub(crate) enum Event {
    Started(String),
    Progress(String),
    Finished(Result<String, String>),
}

pub(crate) struct Request {
    child: Arc<Mutex<Option<Child>>>,
    stopped: Arc<AtomicBool>,
}
fn kill(child: &mut Child) {
    // Each request owns a process group, so tool children cannot outlive Stop.
    #[cfg(unix)]
    {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}
impl Request {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock()
            && let Some(child) = child.as_mut()
        {
            kill(child);
        }
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A temp file holding an omp prompt, deleted on drop so failed spawns and
/// finished runs alike never leak prompt text into the temp dir.
struct PromptFile {
    path: PathBuf,
}
impl Drop for PromptFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A fully planned agent invocation: the process to spawn, how the prompt
/// reaches it, and what to clean up afterwards.
struct Invocation {
    command: Command,
    /// Written to stdin when the CLI honors it (codex, claude, opencode).
    /// Omp instead reads the prompt from a file (see below), so its stdin
    /// is closed and `None` here means "send nothing".
    stdin_prompt: Option<String>,
    /// Omp only persists print-mode sessions for argument prompts, and large
    /// review prompts can exceed OS single-argument limits, so the prompt
    /// travels in a temp file referenced as `@path`.
    prompt_file: Option<PromptFile>,
}

fn command(cwd: &Path, options: &Options, prompt: &str, conversation: Option<&str>) -> Invocation {
    let mut invocation = Invocation {
        command: Command::new(options.provider.command()),
        stdin_prompt: None,
        prompt_file: None,
    };
    let command = &mut invocation.command;
    match options.provider {
        Provider::Codex => {
            command.arg("exec");
            if conversation.is_some() {
                command.arg("resume");
            }
            command.args([
                "--json",
                "-c",
                "sandbox_mode=\"read-only\"",
                "-c",
                "approval_policy=\"never\"",
            ]);
            if !options.model.is_empty() {
                command.args(["--model", &options.model]);
            }
            if let Some(effort) = options.effort.argument() {
                command.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
            }
            if let Some(id) = conversation {
                command.arg(id);
            } else {
                command.arg("-C").arg(cwd);
            }
            command.arg("-");
            invocation.stdin_prompt = Some(prompt.to_owned());
        }
        Provider::Claude => {
            // Only built-in file readers; safe mode disables hooks/plugins/MCP.
            command.args([
                "--print",
                "--output-format",
                "stream-json",
                "--verbose",
                "--safe-mode",
                "--strict-mcp-config",
                "--tools",
                "Read,Grep,Glob",
                "--allowedTools",
                "Read,Grep,Glob",
            ]);
            if !options.model.is_empty() {
                command.args(["--model", &options.model]);
            }
            if let Some(effort) = options.effort.argument() {
                command.args(["--effort", effort]);
            }
            if let Some(id) = conversation {
                command.args(["--resume", id]);
            }
            invocation.stdin_prompt = Some(prompt.to_owned());
        }
        Provider::Opencode => {
            // No sandbox flags exist for `opencode run`: read-only rests on
            // the prompt instruction plus never passing `--auto`, so writes
            // still need an approval the non-interactive run cannot grant.
            // There is also no reasoning-effort flag; effort is ignored.
            // The prompt travels over stdin (no message argument), which
            // `run` honors, and `--session` resumes the previous turn.
            command.args(["run", "--format", "json"]);
            if !options.model.is_empty() {
                command.args(["--model", &options.model]);
            }
            if let Some(id) = conversation {
                command.args(["--session", id]);
            }
            invocation.stdin_prompt = Some(prompt.to_owned());
        }
        Provider::Omp => {
            // Read-only tool set mirroring Claude's readers (plus LSP for
            // code intelligence); `always-ask` keeps anything outside that
            // set denied. `--resume` needs the session file path, which the
            // previous run resolves (see `find_omp_session`); anything else
            // starts fresh rather than failing the request.
            command.args([
                "-p",
                "--mode",
                "json",
                "--tools",
                "read,grep,glob,lsp",
                "--approval-mode",
                "always-ask",
            ]);
            if !options.model.is_empty() {
                command.args(["--model", &options.model]);
            }
            if let Some(effort) = options.effort.argument() {
                command.args(["--thinking", effort]);
            }
            if let Some(path) = conversation.filter(|path| Path::new(path).is_file()) {
                command.args(["--resume", path]);
            }
            // Omp only persists print-mode sessions for argument prompts, so
            // the prompt travels in a temp file referenced as `@path` (which
            // also sidesteps OS single-argument size limits for large diffs).
            // Stdin stays closed: piping the prompt there too would duplicate
            // it into the message alongside the file.
            match write_prompt_file(prompt) {
                Some(file) => {
                    command.arg(format!("@{}", file.path.display()));
                    invocation.prompt_file = Some(file);
                }
                None => {
                    command.arg(prompt);
                }
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        invocation.command.process_group(0);
    }
    invocation
        .command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    invocation
}

/// Stage an omp prompt in the temp dir. `None` means the write failed and the
/// caller falls back to a verbatim argument.
fn write_prompt_file(prompt: &str) -> Option<PromptFile> {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "devcroft-review-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::write(&path, prompt).ok()?;
    Some(PromptFile { path })
}

/// Resolve an omp session id to the transcript path `--resume` needs.
/// Session files live as `*<id>.jsonl` under `~/.omp/agent/sessions` (plus
/// `$OMP_SESSION_DIR` when set); `None` means "resume unavailable, start
/// fresh". Bounded so a huge store cannot stall the worker thread.
fn find_omp_session(id: &str) -> Option<PathBuf> {
    if id.is_empty() || id.contains('/') || id.contains('\0') {
        return None;
    }
    let mut roots = Vec::new();
    if let Some(dir) = std::env::var_os("OMP_SESSION_DIR").map(PathBuf::from) {
        roots.push(dir);
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".omp/agent/sessions"));
    }
    let target = format!("_{id}.jsonl");
    let mut visited = 0usize;
    for root in roots {
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            visited += 1;
            if visited > 2000 {
                return None;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(&target))
                {
                    return Some(path);
                }
                if path.is_dir() && visited <= 2000 {
                    stack.push(path);
                }
            }
        }
    }
    None
}

#[derive(Default)]
struct Output {
    answer: Option<String>,
    error: Option<String>,
    /// Omp `session`-event id, resolved to a resumable path after the run.
    omp_session_id: Option<String>,
    /// Non-JSON stdout lines (omp prints some failures as plain text while
    /// still exiting 0). Capped; only surfaced when no answer or JSON error
    /// was captured.
    plain_lines: Vec<String>,
    plain_bytes: usize,
}
impl Output {
    /// Pull the final assistant text out of an omp `message_end`,
    /// `turn_end`, or `agent_end` payload. Thinking and tool-call items
    /// carry no text, so intermediate tool chatter never poses as the
    /// answer; last non-empty assistant message wins.
    fn omp_answer(event: &Value) -> Option<String> {
        let message = if event["message"].is_object() {
            &event["message"]
        } else {
            event
        };
        if message["role"].as_str() != Some("assistant") {
            return None;
        }
        let text = message["content"]
            .as_array()?
            .iter()
            .filter(|item| item["type"].as_str() == Some("text"))
            .filter_map(|item| item["text"].as_str())
            .collect::<String>();
        (!text.trim().is_empty()).then_some(text)
    }
    fn event(&mut self, event: &Value, sender: &Sender<Event>) {
        // Opencode tags every JSON line with its session id; the first one
        // resumes follow-up turns via `--session`.
        if let Some(id) = event.get("sessionID").and_then(Value::as_str) {
            let _ = sender.send_blocking(Event::Started(id.to_owned()));
        }
        match event["type"].as_str().unwrap_or_default() {
            "thread.started" | "system" => {
                if let Some(id) = event["thread_id"]
                    .as_str()
                    .or_else(|| event["session_id"].as_str())
                {
                    let _ = sender.send_blocking(Event::Started(id.to_owned()));
                }
            }
            "session" => {
                // Omp's id is not itself resumable (`--resume` wants the
                // transcript path), so it is resolved after the run instead
                // of being emitted here.
                if self.omp_session_id.is_none() {
                    self.omp_session_id = event["id"].as_str().map(str::to_owned);
                }
            }
            "item.started" | "assistant" => {
                let _ = sender
                    .send_blocking(Event::Progress("Reading and organizing the change…".into()));
            }
            "item.completed" if event["item"]["type"] == "agent_message" => {
                self.answer = event["item"]["text"].as_str().map(str::to_owned);
            }
            "text" => {
                // Opencode assistant message part. Last non-empty part wins,
                // matching the last-wins posture of the other transports.
                if event["part"]["type"].as_str() == Some("text")
                    && let Some(text) = event["part"]["text"].as_str()
                    && !text.trim().is_empty()
                {
                    self.answer = Some(text.to_owned());
                }
            }
            "message_end" | "turn_end" | "agent_end" => {
                if let Some(answer) = Self::omp_answer(event) {
                    self.answer = Some(answer);
                }
            }
            "result" => {
                if event["is_error"].as_bool() == Some(true) {
                    self.error = Some(
                        event["result"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| event["errors"].to_string()),
                    );
                } else {
                    self.answer = event["result"].as_str().map(str::to_owned);
                }
            }
            "turn.failed" | "error" => {
                self.error = Some(
                    event["error"]["message"]
                        .as_str()
                        .or_else(|| event["message"].as_str())
                        .unwrap_or("Agent reported a failure")
                        .to_owned(),
                );
            }
            _ => {}
        }
    }
    fn plain(&mut self, line: &[u8]) {
        if self.plain_bytes >= 4 * 1024 || self.plain_lines.len() >= 20 {
            return;
        }
        let text = String::from_utf8_lossy(line).trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.plain_bytes += text.len();
        self.plain_lines.push(text);
    }
}

pub(crate) fn start(
    cwd: PathBuf,
    options: Options,
    prompt: String,
    conversation: Option<String>,
    sender: Sender<Event>,
) -> Request {
    let provider = options.provider;
    let mut invocation = command(&cwd, &options, &prompt, conversation.as_deref());
    // Omp stages its prompt in a temp file (or a fallback argument), so only
    // stdin-planned harnesses carry the prompt into the worker thread.
    let stdin_prompt = invocation.stdin_prompt.take();
    run(invocation, provider, stdin_prompt, sender)
}

fn run(
    mut invocation: Invocation,
    provider: Provider,
    prompt: Option<String>,
    sender: Sender<Event>,
) -> Request {
    let label = provider.label();
    let resolve_omp = provider == Provider::Omp;
    let child = Arc::new(Mutex::new(None));
    let running = child.clone();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_flag = stopped.clone();
    std::thread::spawn(move || {
        // Hold the prompt temp file until the child exits.
        let _prompt_file = invocation.prompt_file.take();
        let mut process = match invocation.command.spawn() {
            Ok(process) => process,
            Err(error) => {
                let _ = sender
                    .send_blocking(Event::Finished(Err(format!("Starting {label}: {error}",))));
                return;
            }
        };
        let stdout = process.stdout.take().expect("piped stdout");
        let stdin = process.stdin.take().expect("piped stdin");
        let stderr = process.stderr.take().expect("piped stderr");
        *running.lock().unwrap() = Some(process);
        if stop_flag.load(Ordering::SeqCst) {
            kill(running.lock().unwrap().as_mut().unwrap());
        }
        // Drain output concurrently with the potentially large prompt write.
        // `None` closes stdin unread so file-prompted harnesses never see a
        // duplicate piped prompt.
        let input = std::thread::spawn(move || match prompt {
            Some(prompt) => {
                let mut stdin = stdin;
                stdin.write_all(prompt.as_bytes())
            }
            None => Ok(()),
        });
        let errors = std::thread::spawn(move || {
            let mut output = String::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.by_ref().take(64 * 1024).read_to_string(&mut output);
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
            output
        });
        let mut output = Output::default();
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = Vec::new();
            match reader
                .by_ref()
                .take(8 * 1024 * 1024 + 1)
                .read_until(b'\n', &mut line)
            {
                Ok(0) => break,
                Ok(_) if line.len() > 8 * 1024 * 1024 => {
                    output.error = Some("Agent response exceeded the output limit".into());
                    kill(running.lock().unwrap().as_mut().unwrap());
                    break;
                }
                Ok(_) => match serde_json::from_slice(&line) {
                    Ok(event) => output.event(&event, &sender),
                    Err(_) => output.plain(&line),
                },
                Err(error) => {
                    output.error = Some(format!("Reading agent output: {error}"));
                    kill(running.lock().unwrap().as_mut().unwrap());
                    break;
                }
            }
        }
        // Keep ownership visible to Stop even if stdout closes before exit.
        let status = loop {
            match running.lock().unwrap().as_mut().unwrap().try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(error) => break Err(error),
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        if let Some(mut process) = running.lock().unwrap().take() {
            if status.is_err() {
                kill(&mut process);
            }
            let _ = process.wait();
        }
        let stderr = errors.join().unwrap_or_default();
        let input_error = input.join().ok().and_then(Result::err);
        // Omp reports its resumable identity as a transcript path, discovered
        // here on the worker thread so resume never scans the session store
        // on the UI thread. A late Started still lands before Finished, which
        // is all the view needs to attach the next turn.
        if resolve_omp
            && let Some(id) = output.omp_session_id.take()
            && let Some(path) = find_omp_session(&id)
        {
            let _ = sender.send_blocking(Event::Started(path.to_string_lossy().into_owned()));
        }
        let result = if stop_flag.load(Ordering::SeqCst) {
            Err("Review stopped".into())
        } else if let Some(error) = output.error {
            Err(error)
        } else if let Some(error) = input_error {
            Err(format!("Sending prompt: {error}"))
        } else {
            match (status, output.answer) {
                (Ok(status), Some(answer)) if status.success() && !answer.trim().is_empty() => {
                    Ok(answer)
                }
                (Ok(status), _) => {
                    // Some harnesses print failures as plain text while
                    // exiting 0 (e.g. omp's missing-model notice on stdout),
                    // leaving stderr empty: prefer that captured text over a
                    // bare exit status.
                    let detail = if stderr.trim().is_empty() && !output.plain_lines.is_empty() {
                        output.plain_lines.join("\n")
                    } else {
                        stderr.trim().to_owned()
                    };
                    Err(format!("{label} exited with {status}. {detail}"))
                }
                (Err(error), _) => Err(format!("Waiting for agent: {error}")),
            }
        };
        let _ = sender.send_blocking(Event::Finished(result));
    });
    Request { child, stopped }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    fn fixture(script: &str) -> Invocation {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Invocation {
            command,
            stdin_prompt: None,
            prompt_file: None,
        }
    }

    fn completion(receiver: &async_channel::Receiver<Event>) -> Result<String, String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Ok(Event::Finished(result)) = receiver.try_recv() {
                return result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "request did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn stop_terminates_group_even_after_stdout_closes() {
        let (sender, receiver) = async_channel::unbounded();
        let request = run(
            fixture("cat >/dev/null; exec 1>&-; sleep 30"),
            Provider::Codex,
            Some("prompt".into()),
            sender,
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        request.stop();
        assert_eq!(completion(&receiver).unwrap_err(), "Review stopped");
    }

    #[cfg(unix)]
    #[test]
    fn streams_output_while_writing_a_large_prompt() {
        let (sender, receiver) = async_channel::unbounded();
        let script = r#"head -c 200000 /dev/zero | tr '\000' ' '; echo; cat >/dev/null; printf '%s\n' '{"type":"result","result":"answer","is_error":false}'"#;
        let _request = run(
            fixture(script),
            Provider::Codex,
            Some("x".repeat(200000)),
            sender,
        );
        assert_eq!(completion(&receiver).unwrap(), "answer");
    }

    #[cfg(unix)]
    #[test]
    fn plain_text_failures_name_the_problem() {
        let (sender, receiver) = async_channel::unbounded();
        let _request = run(
            fixture("echo 'Model \"bogus\" not found'; exit 0"),
            Provider::Omp,
            None,
            sender,
        );
        let error = completion(&receiver).unwrap_err();
        assert!(error.contains("bogus"), "{error}");
    }

    #[test]
    fn provider_arguments_keep_options_and_read_only_on_resume() {
        for provider in Provider::ALL {
            let options = Options {
                provider,
                model: "chosen-model".into(),
                effort: Effort::High,
                include_concepts: true,
            };
            // Omp resume wants a real transcript path; anything else must
            // start fresh rather than fail.
            let missing = Path::new("/definitely/not/a/session.jsonl");
            assert!(!missing.is_file());
            let invocation = command(Path::new("/tmp"), &options, "prompt", Some("session-id"));
            let args = invocation
                .command
                .get_args()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            match provider {
                Provider::Codex => assert!(
                    args.contains("chosen-model")
                        && args.contains("high")
                        && args.contains("session-id")
                        && args.contains("read-only")
                        && args.contains("approval_policy=\"never\"")
                ),
                Provider::Claude => assert!(
                    args.contains("chosen-model")
                        && args.contains("high")
                        && args.contains("session-id")
                        && args.contains("--safe-mode")
                        && args.contains("--tools Read,Grep,Glob")
                ),
                // Opencode has no effort flag; the prompt still travels over
                // stdin and the previous turn resumes via `--session`.
                Provider::Opencode => {
                    assert!(args.contains("chosen-model") && args.contains("session-id"));
                    assert!(!args.contains("high") && !args.contains("--auto"));
                    assert!(invocation.stdin_prompt.is_some());
                }
                Provider::Omp => {
                    assert!(
                        args.contains("chosen-model")
                            && args.contains("--thinking")
                            && args.contains("high")
                            && args.contains("--tools")
                            && args.contains("read,grep,glob,lsp")
                            && args.contains("always-ask")
                            && !args.contains("session-id")
                    );
                    // Prompt travels as `@file` (or a fallback argument), so
                    // stdin stays closed and the message is never duplicated.
                    assert!(invocation.stdin_prompt.is_none());
                    assert!(invocation.prompt_file.is_some() || args.contains("prompt"));
                }
            }
        }
    }
    #[test]
    fn omp_resume_uses_transcript_paths_only() {
        let options = Options {
            provider: Provider::Omp,
            ..Options::default()
        };
        let file = write_prompt_file("prompt").unwrap();
        let path = file.path.to_string_lossy().into_owned();
        let invocation = command(Path::new("/tmp"), &options, "prompt", Some(&path));
        let args = invocation
            .command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(args.contains("--resume") && args.contains(&path));
    }
    #[test]
    fn opencode_events_carry_session_and_answer() {
        let (sender, _) = async_channel::unbounded();
        let mut output = Output::default();
        output.event(
            &serde_json::json!({"type":"step_start","sessionID":"ses_1"}),
            &sender,
        );
        output.event(
            &serde_json::json!({"type":"text","sessionID":"ses_1","part":{"type":"text","text":"{\"title\":\"t\"}"}}),
            &sender,
        );
        assert_eq!(output.answer.as_deref(), Some("{\"title\":\"t\"}"));
        // Empty parts never wipe a captured answer.
        output.event(
            &serde_json::json!({"type":"text","sessionID":"ses_1","part":{"type":"text","text":"  "}}),
            &sender,
        );
        assert_eq!(output.answer.as_deref(), Some("{\"title\":\"t\"}"));
        output.event(
            &serde_json::json!({"type":"error","sessionID":"ses_1","error":{"type":"no-route","message":"bad model"}}),
            &sender,
        );
        assert_eq!(output.error.as_deref(), Some("bad model"));
    }
    #[test]
    fn omp_events_answer_only_from_assistant_text() {
        let (sender, _) = async_channel::unbounded();
        let mut output = Output::default();
        // Tool-call chatter without text never poses as the answer.
        output.event(
            &serde_json::json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"toolcall","call":{"tool":"read"}}]}}),
            &sender,
        );
        assert!(output.answer.is_none());
        output.event(
            &serde_json::json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan"},{"type":"text","text":"final"}]}}),
            &sender,
        );
        assert_eq!(output.answer.as_deref(), Some("final"));
        output.event(&serde_json::json!({"type":"session","id":"01abc"}), &sender);
        assert_eq!(output.omp_session_id.as_deref(), Some("01abc"));
    }
    #[test]
    fn transport_errors_are_not_successful_answers() {
        let (sender, _) = async_channel::unbounded();
        let mut output = Output::default();
        output.event(&serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":"partial"}}), &sender);
        output.event(
            &serde_json::json!({"type":"turn.failed","error":{"message":"failure"}}),
            &sender,
        );
        assert_eq!(output.error.as_deref(), Some("failure"));
        output.event(
            &serde_json::json!({"type":"result","is_error":true,"errors":["denied"]}),
            &sender,
        );
        assert!(output.error.unwrap().contains("denied"));
    }
}
