//! Provider-neutral activity tracking for managed Agent panes.
//!
//! The workspace and command palette consume only [`ActivitySnapshot`].
//! Provider-specific terminal layouts and event correlation stay behind this
//! module's interface so another harness does not add state logic to the UI.

mod providers;
mod terminal;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_channel::{Receiver, Sender};
use parking_lot::Mutex;

use crate::agent::AgentKind;

pub(crate) use terminal::TerminalObservation;
use terminal::{TerminalEvidence, classify};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivityState {
    Starting,
    Idle,
    Working,
    NeedsAttention,
    Finished,
    Unavailable,
    NoAgent,
}

impl ActivityState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Idle => "Idle",
            Self::Working => "Working",
            Self::NeedsAttention => "Needs attention",
            Self::Finished => "Finished",
            Self::Unavailable => "Status unavailable",
            Self::NoAgent => "No agent",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentActivity {
    pub(crate) agent: AgentKind,
    pub(crate) state: ActivityState,
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ActivitySnapshot {
    activities: HashMap<PathBuf, AgentActivity>,
    launches: HashMap<u64, (PathBuf, AgentActivity)>,
}

impl ActivitySnapshot {
    pub(crate) fn for_launch(&self, launch: u64) -> Option<&AgentActivity> {
        self.launches.get(&launch).map(|(_, activity)| activity)
    }
    pub(crate) fn for_checkout(&self, checkout: &Path) -> Option<&AgentActivity> {
        self.activities.get(checkout)
    }

    /// One entry per managed agent pane with the checkout it runs in, for
    /// space-filtered attention counts: several panes can share a checkout.
    pub(crate) fn launches_by_checkout(&self) -> impl Iterator<Item = (&Path, &AgentActivity)> {
        self.launches
            .values()
            .map(|(checkout, activity)| (checkout.as_path(), activity))
    }

    #[cfg(test)]
    pub(crate) fn attention_count(&self) -> usize {
        self.launches
            .values()
            .filter(|(_, activity)| activity.state == ActivityState::NeedsAttention)
            .count()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Path, &AgentActivity)> {
        self.activities
            .iter()
            .map(|(checkout, activity)| (checkout.as_path(), activity))
    }
}

#[derive(Clone)]
pub(crate) struct AgentActivityStore {
    inner: Arc<Mutex<StoreInner>>,
    updates: Sender<()>,
}

#[derive(Default)]
struct StoreInner {
    next_generation: u64,
    records: HashMap<(PathBuf, u64), ActivityRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BaseState {
    Starting,
    Idle,
    Working,
}

struct ActivityRecord {
    generation: u64,
    native_session: Option<String>,
    agent: AgentKind,
    base: BaseState,
    working_sessions: HashSet<String>,
    pending_requests: HashSet<String>,
    resolved_requests: HashSet<String>,
    resolved_request_order: VecDeque<String>,
    terminal_blocked: bool,
    observed_work: bool,
    unseen_completion: bool,
    visible: bool,
    observation_available: bool,
    structured_status_available: bool,
    exited: bool,
    detail: Option<String>,
}

impl ActivityRecord {
    const MAX_RESOLVED_REQUESTS: usize = 256;

    fn remember_resolved(&mut self, key: String) {
        if self.resolved_requests.insert(key.clone()) {
            self.resolved_request_order.push_back(key);
        }
        while self.resolved_request_order.len() > Self::MAX_RESOLVED_REQUESTS {
            if let Some(expired) = self.resolved_request_order.pop_front() {
                self.resolved_requests.remove(&expired);
            }
        }
    }

    fn resolve_all_pending(&mut self) {
        let resolved = self.pending_requests.drain().collect::<Vec<_>>();
        for key in resolved {
            self.remember_resolved(key);
        }
    }

    fn projected(&self) -> AgentActivity {
        let state = if self.exited {
            ActivityState::NoAgent
        } else if self.terminal_blocked || !self.pending_requests.is_empty() {
            ActivityState::NeedsAttention
        } else if !self.observation_available {
            ActivityState::Unavailable
        } else if self.unseen_completion {
            ActivityState::Finished
        } else {
            match self.base {
                BaseState::Starting => ActivityState::Starting,
                BaseState::Idle => ActivityState::Idle,
                BaseState::Working => ActivityState::Working,
            }
        };
        AgentActivity {
            agent: self.agent,
            state,
            detail: self.detail.clone(),
        }
    }
}

impl AgentActivityStore {
    pub(crate) fn new() -> (Self, Receiver<()>) {
        let (updates, receiver) = async_channel::unbounded();
        (
            Self {
                inner: Arc::new(Mutex::new(StoreInner::default())),
                updates,
            },
            receiver,
        )
    }

    #[cfg(test)]
    pub(crate) fn start(&self, checkout: &Path, agent: AgentKind) -> PreparedAgentLaunch {
        self.start_with_session(checkout, agent, None)
    }

    #[cfg(test)]
    pub(crate) fn start_with_session(
        &self,
        checkout: &Path,
        agent: AgentKind,
        session: Option<&crate::agent_sessions::SessionSummary>,
    ) -> PreparedAgentLaunch {
        self.start_with_prompt(checkout, agent, session, None)
    }

    pub(crate) fn start_with_prompt(
        &self,
        checkout: &Path,
        agent: AgentKind,
        session: Option<&crate::agent_sessions::SessionSummary>,
        prompt: Option<&str>,
    ) -> PreparedAgentLaunch {
        let checkout = checkout
            .canonicalize()
            .unwrap_or_else(|_| checkout.to_owned());
        let generation = {
            let mut inner = self.inner.lock();
            inner.next_generation = inner.next_generation.wrapping_add(1).max(1);
            let generation = inner.next_generation;
            inner.records.insert(
                (checkout.clone(), generation),
                ActivityRecord {
                    generation,
                    native_session: session.map(|s| s.key.id.clone()),
                    agent,
                    base: BaseState::Starting,
                    working_sessions: HashSet::new(),
                    pending_requests: HashSet::new(),
                    resolved_requests: HashSet::new(),
                    resolved_request_order: VecDeque::new(),
                    terminal_blocked: false,
                    observed_work: false,
                    unseen_completion: false,
                    visible: false,
                    observation_available: agent != AgentKind::Omp,
                    structured_status_available: false,
                    exited: false,
                    detail: (agent == AgentKind::Omp)
                        .then(|| "No activity adapter for OMP".to_owned()),
                },
            );
            generation
        };
        self.notify();

        let emitter = ActivityEmitter {
            store: self.clone(),
            checkout: checkout.clone(),
            generation,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut provider = providers::prepare(agent, generation, &emitter, Arc::clone(&cancelled))
            .unwrap_or_else(|error| {
                emitter.unavailable(format!("Activity setup failed: {error}"));
                providers::ProviderLaunch::default()
            });
        if let Some(session) = session {
            provider.arguments.splice(0..0, session.arguments());
            provider.environment.extend(session.environment());
        }
        if let Some(prompt) = prompt {
            provider.arguments.extend(agent.prompt_arguments(prompt));
        }
        let exit_marker = format!("devcroft-agent-exit-{generation}-");
        let command_line = shell_command(agent, generation, &provider);
        PreparedAgentLaunch {
            command_line,
            exit_marker,
            environment: provider.environment,
            emitter,
            lease: ActivityLease {
                store: self.clone(),
                checkout,
                generation,
                cancelled,
                cleanup_paths: provider.cleanup_paths,
            },
        }
    }

    pub(crate) fn snapshot(&self) -> ActivitySnapshot {
        let inner = self.inner.lock();
        let mut activities: HashMap<PathBuf, AgentActivity> = HashMap::new();
        let mut launches = HashMap::new();
        for ((checkout, generation), record) in &inner.records {
            let activity = record.projected();
            launches.insert(*generation, (checkout.clone(), activity.clone()));
            let priority = |state| match state {
                ActivityState::NeedsAttention => 6,
                ActivityState::Working => 5,
                ActivityState::Finished => 4,
                ActivityState::Starting => 3,
                ActivityState::Unavailable => 2,
                _ => 1,
            };
            if activities
                .get(checkout)
                .is_none_or(|previous| priority(activity.state) > priority(previous.state))
            {
                activities.insert(checkout.clone(), activity);
            }
        }
        ActivitySnapshot {
            activities,
            launches,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_visible_checkout(&self, checkout: Option<&Path>) {
        let launch = checkout.and_then(|path| {
            self.inner
                .lock()
                .records
                .keys()
                .find(|(p, _)| p == path)
                .map(|(_, id)| *id)
        });
        self.set_visible_launch(launch);
    }

    pub(crate) fn set_visible_launch(&self, launch: Option<u64>) {
        let mut changed = false;
        {
            let mut inner = self.inner.lock();
            for ((_, id), record) in &mut inner.records {
                let before = record.projected();
                record.visible = launch == Some(*id);
                if record.visible {
                    record.unseen_completion = false;
                }
                changed |= before != record.projected();
            }
        }
        if changed {
            self.notify();
        }
    }

    fn update(&self, checkout: &Path, generation: u64, mutate: impl FnOnce(&mut ActivityRecord)) {
        let changed = {
            let mut inner = self.inner.lock();
            let Some(record) = inner.records.get_mut(&(checkout.to_owned(), generation)) else {
                return;
            };
            if record.generation != generation {
                return;
            }
            let before = record.projected();
            mutate(record);
            before != record.projected()
        };
        if changed {
            self.notify();
        }
    }

    fn remove(&self, checkout: &Path, generation: u64) {
        let removed = {
            let mut inner = self.inner.lock();
            if inner
                .records
                .get(&(checkout.to_owned(), generation))
                .is_some_and(|record| record.generation == generation)
            {
                inner.records.remove(&(checkout.to_owned(), generation));
                true
            } else {
                false
            }
        };
        if removed {
            self.notify();
        }
    }

    fn notify(&self) {
        let _ = self.updates.try_send(());
    }
}

pub(crate) struct PreparedAgentLaunch {
    command_line: String,
    exit_marker: String,
    environment: Vec<(String, String)>,
    emitter: ActivityEmitter,
    lease: ActivityLease,
}

impl PreparedAgentLaunch {
    pub(crate) fn id(&self) -> u64 {
        self.emitter.generation
    }
    pub(crate) fn native_session(&self) -> Option<String> {
        self.emitter
            .store
            .inner
            .lock()
            .records
            .get(&(self.emitter.checkout.clone(), self.emitter.generation))
            .and_then(|r| r.native_session.clone())
    }
    pub(crate) fn command_line(&self) -> &str {
        &self.command_line
    }

    pub(crate) fn environment(&self) -> &[(String, String)] {
        &self.environment
    }

    pub(crate) fn observe(&self, observation: TerminalObservation) {
        if observation.title.starts_with(&self.exit_marker) {
            let success = observation.title.ends_with("-0");
            self.emitter.exited(success);
            return;
        }
        let Some(evidence) = classify(self.lease.agent(), &observation) else {
            return;
        };
        self.emitter.terminal_evidence(evidence);
    }

    /// Observe raw PTY output for the exit marker emitted by the launch
    /// wrapper. Titles can be overwritten by an agent in the same output
    /// batch, so the raw stream is the authoritative exit signal.
    pub(crate) fn observe_output(&self, output: &[u8]) {
        let v2_marker = format!(
            "\u{1b}]0;devcroft-opencode-v2-{}\u{7}",
            self.emitter.generation
        );
        if self.lease.agent() == AgentKind::Opencode
            && find_bytes(output, v2_marker.as_bytes()).is_some()
            && !self.lease.cancelled.swap(true, Ordering::AcqRel)
        {
            self.emitter
                .unavailable("OpenCode v2 uses terminal activity observation".to_owned());
        }
        let marker = format!("\u{1b}]0;{}", self.exit_marker);
        let Some(start) = find_bytes(output, marker.as_bytes()) else {
            return;
        };
        let status = &output[start + marker.len()..];
        let Some(end) = status.iter().position(|byte| *byte == 7 || *byte == 27) else {
            return;
        };
        self.emitter.exited(&status[..end] == b"0");
    }

    pub(crate) fn unavailable(&self, detail: impl Into<String>) {
        self.emitter.unavailable(detail.into());
    }

    pub(crate) fn exited(&self) {
        self.emitter.exited(false);
    }

    #[cfg(test)]
    fn emitter(&self) -> ActivityEmitter {
        self.emitter.clone()
    }
}

struct ActivityLease {
    store: AgentActivityStore,
    checkout: PathBuf,
    generation: u64,
    cancelled: Arc<AtomicBool>,
    cleanup_paths: Vec<PathBuf>,
}

impl ActivityLease {
    fn agent(&self) -> AgentKind {
        self.store
            .inner
            .lock()
            .records
            .get(&(self.checkout.clone(), self.generation))
            .filter(|record| record.generation == self.generation)
            .map(|record| record.agent)
            .unwrap_or(AgentKind::DEFAULT)
    }
}

impl Drop for ActivityLease {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        for path in &self.cleanup_paths {
            let _ = std::fs::remove_dir_all(path);
        }
        self.store.remove(&self.checkout, self.generation);
    }
}

#[derive(Clone)]
pub(crate) struct ActivityEmitter {
    store: AgentActivityStore,
    checkout: PathBuf,
    generation: u64,
}

impl ActivityEmitter {
    pub(super) fn session_selected(&self, id: &str) {
        if id != "terminal" {
            self.store.update(&self.checkout, self.generation, |r| {
                r.native_session = Some(id.to_owned())
            });
        }
    }
    #[cfg(test)]
    pub(crate) fn working(&self, detail: Option<String>) {
        self.store
            .update(&self.checkout, self.generation, |record| {
                if record.exited {
                    return;
                }
                record.observation_available = true;
                record.base = BaseState::Working;
                record.observed_work = true;
                record.unseen_completion = false;
                record.detail = detail;
            });
    }

    pub(crate) fn session_working(&self, session: &str, detail: Option<String>) {
        let session = session.to_owned();
        self.store
            .update(&self.checkout, self.generation, |record| {
                if record.exited {
                    return;
                }
                record.observation_available = true;
                record.structured_status_available = true;
                record.working_sessions.insert(session);
                record.base = BaseState::Working;
                record.observed_work = true;
                record.unseen_completion = false;
                record.detail = detail;
            });
    }

    #[cfg(test)]
    pub(crate) fn idle(&self, detail: Option<String>) {
        self.store
            .update(&self.checkout, self.generation, |record| {
                if record.exited {
                    return;
                }
                let completed = record.observed_work && record.base == BaseState::Working;
                record.observation_available = true;
                record.working_sessions.clear();
                record.base = BaseState::Idle;
                record.unseen_completion |= completed && !record.visible;
                record.detail = detail;
            });
    }

    pub(crate) fn session_idle(&self, session: &str, detail: Option<String>) {
        self.store
            .update(&self.checkout, self.generation, |record| {
                if record.exited {
                    return;
                }
                record.observation_available = true;
                record.structured_status_available = true;
                record.working_sessions.remove(session);
                if record.working_sessions.is_empty() {
                    let completed = record.observed_work && record.base == BaseState::Working;
                    record.base = BaseState::Idle;
                    record.unseen_completion |= completed && !record.visible;
                    record.detail = detail;
                } else {
                    record.base = BaseState::Working;
                    record.detail = Some("Agent is working".to_owned());
                }
            });
    }

    pub(crate) fn reconcile_sessions(&self, working_sessions: HashSet<String>, detail: String) {
        self.store
            .update(&self.checkout, self.generation, |record| {
                if record.exited {
                    return;
                }
                let completed = record.observed_work
                    && record.base == BaseState::Working
                    && working_sessions.is_empty();
                record.observation_available = true;
                record.structured_status_available = true;
                record.working_sessions = working_sessions;
                if record.working_sessions.is_empty() {
                    record.base = BaseState::Idle;
                    record.unseen_completion |= completed && !record.visible;
                } else {
                    record.base = BaseState::Working;
                    record.observed_work = true;
                    record.unseen_completion = false;
                }
                record.detail = Some(detail);
            });
    }

    pub(crate) fn request_opened(&self, session: &str, request: &str) {
        let key = format!("{session}\u{1f}{request}");
        self.store
            .update(&self.checkout, self.generation, |record| {
                if record.exited || record.resolved_requests.contains(&key) {
                    return;
                }
                record.observation_available = true;
                record.pending_requests.insert(key);
                record.unseen_completion = false;
                record.detail = Some("Waiting for input".to_owned());
            });
    }

    pub(crate) fn request_resolved(&self, session: &str, request: &str) {
        let key = format!("{session}\u{1f}{request}");
        self.store
            .update(&self.checkout, self.generation, |record| {
                record.pending_requests.remove(&key);
                record.remember_resolved(key);
                if record.pending_requests.is_empty() {
                    record.detail = None;
                }
            });
    }

    pub(crate) fn resolve_session(&self, session: &str) {
        let prefix = format!("{session}\u{1f}");
        self.store
            .update(&self.checkout, self.generation, |record| {
                let resolved = record
                    .pending_requests
                    .iter()
                    .filter(|key| key.starts_with(&prefix))
                    .cloned()
                    .collect::<Vec<_>>();
                for key in resolved {
                    record.pending_requests.remove(&key);
                    record.remember_resolved(key);
                }
                if record.pending_requests.is_empty() {
                    record.detail = None;
                }
            });
    }

    pub(crate) fn unavailable(&self, detail: String) {
        self.store
            .update(&self.checkout, self.generation, |record| {
                if record.exited {
                    return;
                }
                record.observation_available = false;
                record.structured_status_available = false;
                record.detail = Some(detail);
            });
    }

    pub(crate) fn failed(&self, detail: String) {
        self.store
            .update(&self.checkout, self.generation, |record| {
                if record.exited {
                    return;
                }
                record.observation_available = true;
                record.base = BaseState::Idle;
                record.working_sessions.clear();
                record.observed_work = false;
                record.unseen_completion = false;
                record.terminal_blocked = false;
                record.detail = Some(detail);
            });
    }

    pub(crate) fn exited(&self, success: bool) {
        self.store
            .update(&self.checkout, self.generation, |record| {
                record.exited = true;
                record.working_sessions.clear();
                record.pending_requests.clear();
                record.terminal_blocked = false;
                record.unseen_completion = false;
                record.detail = Some(if success {
                    "Agent exited".to_owned()
                } else {
                    "Agent exited with an error or was interrupted".to_owned()
                });
            });
    }

    fn terminal_evidence(&self, evidence: TerminalEvidence) {
        self.store
            .update(&self.checkout, self.generation, |record| {
                if record.exited {
                    return;
                }
                record.observation_available = true;
                match evidence {
                    TerminalEvidence::NeedsAttention(detail) => {
                        record.terminal_blocked = true;
                        record.unseen_completion = false;
                        record.detail = Some(detail);
                    }
                    TerminalEvidence::Working(detail) => {
                        if record.agent == AgentKind::Opencode && record.structured_status_available
                        {
                            record.terminal_blocked = false;
                            return;
                        }
                        record.resolve_all_pending();
                        record.terminal_blocked = false;
                        record.base = BaseState::Working;
                        record.observed_work = true;
                        record.unseen_completion = false;
                        record.detail = detail;
                    }
                    TerminalEvidence::Idle(detail) => {
                        if record.agent == AgentKind::Opencode && record.structured_status_available
                        {
                            record.terminal_blocked = false;
                            return;
                        }
                        if !record.working_sessions.is_empty() {
                            return;
                        }
                        record.resolve_all_pending();
                        record.working_sessions.clear();
                        let completed = record.observed_work && record.base == BaseState::Working;
                        record.terminal_blocked = false;
                        record.base = BaseState::Idle;
                        record.unseen_completion |= completed && !record.visible;
                        record.detail = detail;
                    }
                    TerminalEvidence::Interrupted(detail) => {
                        record.resolve_all_pending();
                        record.working_sessions.clear();
                        record.terminal_blocked = false;
                        record.base = BaseState::Idle;
                        record.observed_work = false;
                        record.unseen_completion = false;
                        record.detail = Some(detail);
                    }
                }
            });
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
}

fn shell_command(
    agent: AgentKind,
    generation: u64,
    provider: &providers::ProviderLaunch,
) -> String {
    let marker = format!("devcroft-agent-exit-{generation}-");
    let unset_environment = if provider.environment.is_empty() {
        String::new()
    } else {
        format!(
            "; unset {}",
            provider
                .environment
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    let arguments = provider
        .arguments
        .iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let invocation = [shell_quote(agent.command()), arguments]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let invocation = if agent == AgentKind::Opencode && !provider.opencode_v1_arguments.is_empty() {
        let v1_arguments = provider
            .opencode_v1_arguments
            .iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        // The login shell may resolve a different OpenCode binary than the
        // GUI process. Check the command used for the actual launch before
        // applying v1-only server flags. OpenCode v2 replaced the server
        // flags but may still print --hostname in its help (2.0.11 did), so
        // the v2-only --standalone flag is the discriminator. Keep the
        // password in the process environment so it is never written into
        // terminal input.
        format!(
            "case \"$({} --help 2>/dev/null)\" in *--standalone*) printf '\\033]0;devcroft-opencode-v2-{}\\007'; {} ;; *) OPENCODE_SERVER_PASSWORD=\"$DEVCROFT_OPENCODE_SERVER_PASSWORD\" {} {} ;; esac",
            shell_quote(agent.command()),
            generation,
            invocation,
            invocation,
            v1_arguments
        )
    } else {
        invocation
    };
    format!(
        "{}; __devcroft_agent_status=$?{}; printf '\\033]0;{}%s\\007' \"$__devcroft_agent_status\"; unset __devcroft_agent_status",
        invocation, unset_environment, marker
    )
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn interactive_guide_prompts_preserve_provider_flags_and_shell_argument_boundaries() {
        use super::*;
        for agent in AgentKind::ALL {
            let prompt = "Read the guide's instructions at /a path/guide.md; $(printf injected)\nContinue chatting.";
            let mut provider = providers::ProviderLaunch {
                arguments: vec!["--existing-setting".into(), "normal profile".into()],
                ..Default::default()
            };
            provider.arguments.extend(agent.prompt_arguments(prompt));
            let command = format!(
                "{}() {{ printf '%s\\0' \"$@\"; }}; {}",
                agent.command(),
                shell_command(agent, 7, &provider)
            );
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &command])
                .output()
                .unwrap();
            assert!(output.status.success());
            let mut expected = Vec::new();
            for argument in &provider.arguments {
                expected.extend_from_slice(argument.as_bytes());
                expected.push(0);
            }
            assert!(
                output.stdout.starts_with(&expected),
                "{} lost interactive prompt arguments",
                agent.id()
            );
            assert!(!provider.arguments.iter().any(|a| {
                [
                    "--print",
                    "exec",
                    "--ignore-user-config",
                    "--ephemeral",
                    "--auto",
                ]
                .contains(&a.as_str())
            }));
        }
    }
    use super::*;

    fn store() -> (AgentActivityStore, PathBuf) {
        let (store, _) = AgentActivityStore::new();
        (store, PathBuf::from("/tmp/devcroft-agent-activity-test"))
    }

    #[test]
    fn background_work_completion_and_acknowledgement() {
        let (store, checkout) = store();
        let launch = store.start(&checkout, AgentKind::Codex);
        let emitter = launch.emitter();
        emitter.working(None);
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Working
        );
        emitter.idle(None);
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Finished
        );
        store.set_visible_checkout(Some(&checkout));
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Idle
        );
    }

    #[test]
    fn one_session_finishing_does_not_finish_another_working_session() {
        let (store, checkout) = store();
        let launch = store.start(&checkout, AgentKind::Codex);
        let emitter = launch.emitter();
        emitter.session_working("parent", None);
        emitter.session_working("child", None);
        emitter.session_idle("child", None);
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Working
        );
        emitter.session_idle("parent", None);
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Finished
        );
    }

    #[test]
    fn opencode_completion_is_not_overwritten_by_a_late_terminal_frame() {
        let (store, checkout) = store();
        let launch = store.start(&checkout, AgentKind::Codex);
        store
            .inner
            .lock()
            .records
            .get_mut(&(checkout.clone(), launch.id()))
            .unwrap()
            .agent = AgentKind::Opencode;
        let emitter = launch.emitter();
        emitter.session_working("s", None);
        emitter.session_idle("s", None);
        emitter.terminal_evidence(TerminalEvidence::Working(None));
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Finished
        );
        store.set_visible_checkout(Some(&checkout));
        emitter.terminal_evidence(TerminalEvidence::Working(None));
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Idle
        );
    }

    #[test]
    fn attention_counts_agents_and_clears_requests_individually() {
        let (store, first) = store();
        let second = PathBuf::from("/tmp/devcroft-agent-activity-second");
        let first_launch = store.start(&first, AgentKind::Claude);
        let second_launch = store.start(&second, AgentKind::Opencode);
        first_launch.emitter().request_opened("s", "one");
        first_launch.emitter().request_opened("s", "two");
        second_launch.emitter().request_opened("s", "one");
        assert_eq!(store.snapshot().attention_count(), 2);
        first_launch.emitter().request_resolved("s", "one");
        assert_eq!(store.snapshot().attention_count(), 2);
        first_launch.emitter().request_resolved("s", "two");
        assert_eq!(store.snapshot().attention_count(), 1);
    }

    #[test]
    fn late_duplicate_request_cannot_reopen_resolved_attention() {
        let (store, checkout) = store();
        let launch = store.start(&checkout, AgentKind::Codex);
        let emitter = launch.emitter();
        emitter.request_resolved("s", "permission");
        emitter.request_opened("s", "permission");
        assert_eq!(store.snapshot().attention_count(), 0);
    }

    #[test]
    fn terminal_prompt_does_not_finish_structured_background_work() {
        let (store, checkout) = store();
        let launch = store.start(&checkout, AgentKind::Codex);
        launch.emitter().session_working("background", None);
        launch.observe(TerminalObservation {
            title: "Codex".into(),
            screen: "ready".into(),
        });
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Working
        );
    }

    #[test]
    fn telemetry_loss_does_not_claim_a_blocker_was_resolved() {
        let (store, checkout) = store();
        let launch = store.start(&checkout, AgentKind::Codex);
        let emitter = launch.emitter();
        emitter.request_opened("s", "permission");
        emitter.unavailable("stream disconnected".to_owned());
        assert_eq!(store.snapshot().attention_count(), 1);

        launch.observe(TerminalObservation {
            title: "Codex".to_owned(),
            screen: "ready".to_owned(),
        });
        assert_eq!(store.snapshot().attention_count(), 0);
    }

    #[test]
    fn stale_generation_cannot_change_replacement() {
        let (store, checkout) = store();
        let old = store.start(&checkout, AgentKind::Claude);
        let replacement = store.start(&checkout, AgentKind::Codex);
        old.emitter().request_opened("s", "old");
        let activity = store.snapshot().for_checkout(&checkout).unwrap().clone();
        assert_eq!(activity.agent, AgentKind::Claude);
        assert_eq!(activity.state, ActivityState::NeedsAttention);
        assert_eq!(
            store.snapshot().for_launch(replacement.id()).unwrap().state,
            ActivityState::Starting
        );
        drop(old);
        assert!(store.snapshot().for_checkout(&checkout).is_some());
        drop(replacement);
        assert!(store.snapshot().for_checkout(&checkout).is_none());
    }

    #[test]
    fn unsupported_agent_reports_unavailable() {
        let (store, checkout) = store();
        let _launch = store.start(&checkout, AgentKind::Omp);
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Unavailable
        );
    }

    #[test]
    fn shell_quoting_handles_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn launch_command_does_not_echo_provider_secrets() {
        let provider = providers::ProviderLaunch {
            environment: vec![("SECRET_TOKEN".into(), "private-value".into())],
            ..Default::default()
        };
        let command = shell_command(AgentKind::Claude, 1, &provider);
        assert!(!command.contains("private-value"));
        assert!(command.contains("unset SECRET_TOKEN"));
    }

    #[cfg(unix)]
    #[test]
    fn opencode_launch_uses_flags_supported_by_the_shell_binary() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("opencode");
        std::fs::write(
            &binary,
            "#!/bin/sh\nif [ \"$1\" = --help ]; then printf '%s\\n' \"$MOCK_OPENCODE_HELP\"; exit 0; fi\nprintf '%s\\n' \"$@\" > \"$MOCK_OPENCODE_ARGS\"\nprintf '%s' \"${OPENCODE_SERVER_PASSWORD-}\" > \"$MOCK_OPENCODE_PASSWORD\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut provider = providers::ProviderLaunch {
            arguments: vec!["--session".into(), "ses_123".into()],
            opencode_v1_arguments: vec![
                "--hostname".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "4096".into(),
            ],
            environment: vec![("DEVCROFT_OPENCODE_SERVER_PASSWORD".into(), "secret".into())],
            ..Default::default()
        };
        let command = shell_command(AgentKind::Opencode, 1, &provider);
        assert!(!command.contains("secret"));
        let args_path = dir.path().join("args");
        let password_path = dir.path().join("password");
        for (help, expected_args, expected_password, is_v2) in [
            (
                "--hostname string --port integer",
                "--session\nses_123\n--hostname\n127.0.0.1\n--port\n4096\n",
                "secret",
                false,
            ),
            (
                "--standalone --server",
                "--session\nses_123\n",
                "user-password",
                true,
            ),
            (
                // OpenCode 2.0.11 printed --hostname in its help while being
                // v2; the v2-only flag must win so it never receives the v1
                // server flags.
                "--hostname string --port integer --standalone --server",
                "--session\nses_123\n",
                "user-password",
                true,
            ),
        ] {
            let output = Command::new("sh")
                .arg("-c")
                .arg(&command)
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        dir.path().display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .env("MOCK_OPENCODE_HELP", help)
                .env("MOCK_OPENCODE_ARGS", &args_path)
                .env("MOCK_OPENCODE_PASSWORD", &password_path)
                .env("OPENCODE_SERVER_PASSWORD", "user-password")
                .env("DEVCROFT_OPENCODE_SERVER_PASSWORD", "secret")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(std::fs::read_to_string(&args_path).unwrap(), expected_args);
            assert_eq!(
                std::fs::read_to_string(&password_path).unwrap(),
                expected_password
            );
            assert_eq!(
                output
                    .stdout
                    .windows(b"devcroft-opencode-v2-1".len())
                    .any(|window| window == b"devcroft-opencode-v2-1"),
                is_v2
            );
        }
        provider.opencode_v1_arguments.clear();
        let plain = shell_command(AgentKind::Opencode, 1, &provider);
        assert!(!plain.contains("--hostname"));
    }

    #[test]
    fn v2_launch_marker_stops_unused_structured_observer() {
        let (store, checkout) = store();
        // Use a provider-free launch so this parser test does not bind a port.
        let launch = store.start(&checkout, AgentKind::Codex);
        store.update(&checkout, launch.id(), |record| {
            record.agent = AgentKind::Opencode
        });
        let marker = format!("\u{1b}]0;devcroft-opencode-v2-{}\u{7}", launch.id());
        launch.observe_output(marker.as_bytes());
        assert!(launch.lease.cancelled.load(Ordering::Acquire));
        assert_eq!(
            store.snapshot().for_launch(launch.id()).unwrap().state,
            ActivityState::Unavailable
        );
    }

    #[test]
    fn raw_exit_marker_wins_over_later_terminal_evidence() {
        let (store, checkout) = store();
        let launch = store.start(&checkout, AgentKind::Codex);
        launch.emitter().working(None);
        launch.observe_output(launch.command_line().as_bytes());
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Working
        );
        let output = format!("\u{1b}]0;{}0\u{7}", launch.exit_marker);
        launch.observe_output(output.as_bytes());
        launch.observe(TerminalObservation {
            title: "Codex".to_owned(),
            screen: "ready".to_owned(),
        });
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::NoAgent
        );
    }

    #[test]
    fn interrupted_turn_does_not_become_finished() {
        let (store, checkout) = store();
        let launch = store.start(&checkout, AgentKind::Codex);
        launch.emitter().working(None);
        launch.observe(TerminalObservation {
            title: "Codex".to_owned(),
            screen: "Conversation interrupted".to_owned(),
        });
        assert_eq!(
            store.snapshot().for_checkout(&checkout).unwrap().state,
            ActivityState::Idle
        );
    }
}
