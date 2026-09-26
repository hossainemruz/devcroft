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
}
impl Provider {
    pub const ALL: [Self; 2] = [Self::Codex, Self::Claude];
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }
    fn command(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
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

fn command(cwd: &Path, options: &Options, conversation: Option<&str>) -> Command {
    let mut command = Command::new(options.provider.command());
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
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[derive(Default)]
struct Output {
    answer: Option<String>,
    error: Option<String>,
}
impl Output {
    fn event(&mut self, event: &Value, sender: &Sender<Event>) {
        match event["type"].as_str().unwrap_or_default() {
            "thread.started" | "system" => {
                if let Some(id) = event["thread_id"]
                    .as_str()
                    .or_else(|| event["session_id"].as_str())
                {
                    let _ = sender.send_blocking(Event::Started(id.to_owned()));
                }
            }
            "item.started" | "assistant" => {
                let _ = sender
                    .send_blocking(Event::Progress("Reading and organizing the change…".into()));
            }
            "item.completed" if event["item"]["type"] == "agent_message" => {
                self.answer = event["item"]["text"].as_str().map(str::to_owned);
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
}

pub(crate) fn start(
    cwd: PathBuf,
    options: Options,
    prompt: String,
    conversation: Option<String>,
    sender: Sender<Event>,
) -> Request {
    run(
        command(&cwd, &options, conversation.as_deref()),
        options.provider.label(),
        prompt,
        sender,
    )
}

fn run(
    mut command: Command,
    provider: &'static str,
    prompt: String,
    sender: Sender<Event>,
) -> Request {
    let child = Arc::new(Mutex::new(None));
    let running = child.clone();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_flag = stopped.clone();
    std::thread::spawn(move || {
        let mut process = match command.spawn() {
            Ok(process) => process,
            Err(error) => {
                let _ = sender.send_blocking(Event::Finished(Err(format!(
                    "Starting {}: {error}",
                    provider
                ))));
                return;
            }
        };
        let stdout = process.stdout.take().expect("piped stdout");
        let mut stdin = process.stdin.take().expect("piped stdin");
        let stderr = process.stderr.take().expect("piped stderr");
        *running.lock().unwrap() = Some(process);
        if stop_flag.load(Ordering::SeqCst) {
            kill(running.lock().unwrap().as_mut().unwrap());
        }
        // Drain output concurrently with the potentially large prompt write.
        let input = std::thread::spawn(move || stdin.write_all(prompt.as_bytes()));
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
                Ok(_) => {
                    if let Ok(event) = serde_json::from_slice(&line) {
                        output.event(&event, &sender);
                    }
                }
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
                (Ok(status), _) => Err(format!(
                    "{} exited with {status}. {}",
                    provider,
                    stderr.trim()
                )),
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
    fn fixture(script: &str) -> Command {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
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
            "fixture",
            "prompt".into(),
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
        let _request = run(fixture(script), "fixture", "x".repeat(200000), sender);
        assert_eq!(completion(&receiver).unwrap(), "answer");
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
            let command = command(Path::new("/tmp"), &options, Some("session-id"));
            let args = command
                .get_args()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                args.contains("chosen-model")
                    && args.contains("high")
                    && args.contains("session-id")
            );
            match provider {
                Provider::Codex => assert!(
                    args.contains("read-only") && args.contains("approval_policy=\"never\"")
                ),
                Provider::Claude => {
                    assert!(args.contains("--safe-mode") && args.contains("--tools Read,Grep,Glob"))
                }
            }
        }
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
