//! Read-only GitHub status snapshots. Credentials stay with `gh`; fetched data
//! stays in memory and never rewrites the user's portable PR assignments.
use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const SHELL_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ci {
    Passed,
    Failed,
    Pending,
    None,
    Unknown,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Status {
    pub(crate) title: String,
    pub(crate) state: String,
    pub(crate) review_decision: Option<String>,
    pub(crate) is_draft: bool,
    status_check_rollup: Option<Vec<Check>>,
    #[serde(default)]
    latest_reviews: Vec<Review>,
}

#[derive(Clone, Debug, Deserialize)]
struct Review {
    state: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Check {
    #[serde(rename = "__typename")]
    kind: String,
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
}

impl Status {
    pub(crate) fn ci(&self) -> Ci {
        let Some(checks) = self.status_check_rollup.as_ref().filter(|c| !c.is_empty()) else {
            return Ci::None;
        };
        let mut pending = false;
        let mut unknown = false;
        for check in checks {
            let state = match check.kind.as_str() {
                "CheckRun" if check.status.as_deref() != Some("COMPLETED") => {
                    pending = true;
                    continue;
                }
                "CheckRun" => check.conclusion.as_deref(),
                "StatusContext" => check.state.as_deref(),
                _ => None,
            };
            match state {
                Some(
                    "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
                    | "STARTUP_FAILURE" | "STALE",
                ) => return Ci::Failed,
                Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => {}
                Some("PENDING" | "EXPECTED") => pending = true,
                _ => unknown = true,
            }
        }
        if unknown {
            Ci::Unknown
        } else if pending {
            Ci::Pending
        } else {
            Ci::Passed
        }
    }

    pub(crate) fn approval(&self) -> &'static str {
        match self.review_decision.as_deref() {
            Some("APPROVED") => "Approved",
            Some("CHANGES_REQUESTED") => "Changes requested",
            Some("REVIEW_REQUIRED") => "Approval required",
            // GitHub leaves reviewDecision empty when no approval rule is
            // configured. Latest effective reviews still carry useful status.
            None | Some("")
                if self
                    .latest_reviews
                    .iter()
                    .any(|review| review.state == "CHANGES_REQUESTED") =>
            {
                "Changes requested"
            }
            None | Some("")
                if self
                    .latest_reviews
                    .iter()
                    .any(|review| review.state == "APPROVED") =>
            {
                "Approved"
            }
            None | Some("") => "Not reviewed",
            _ => "Review unknown",
        }
    }
}

#[derive(Default)]
pub(crate) struct Entry {
    pub(crate) status: Option<Status>,
    pub(crate) error: Option<String>,
    pub(crate) fetching: bool,
    pub(crate) updated_at: Option<i64>,
    attempted_at: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct Cache {
    pub(crate) entries: HashMap<String, Entry>,
}

impl Cache {
    pub(crate) fn begin(&mut self, urls: &[String], force: bool) -> Vec<String> {
        self.entries.retain(|url, _| urls.contains(url));
        let mut pending = Vec::new();
        for url in urls {
            let entry = self.entries.entry(url.clone()).or_default();
            if !entry.fetching
                && (force
                    || entry
                        .attempted_at
                        .is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL))
            {
                entry.fetching = true;
                entry.attempted_at = Some(Instant::now());
                pending.push(url.clone());
            }
        }
        pending
    }

    pub(crate) fn finish(&mut self, url: &str, result: Result<Status>) {
        let Some(entry) = self.entries.get_mut(url) else {
            return;
        };
        entry.fetching = false;
        entry.attempted_at = Some(Instant::now());
        match result {
            Ok(status) => {
                entry.status = Some(status);
                entry.error = None;
                entry.updated_at = Some(crate::relative_time::current_unix_secs());
            }
            Err(error) => entry.error = Some(error.to_string()),
        }
    }
}

pub(crate) fn fetch(url: &str) -> Result<Status> {
    let url = crate::data::dashboard::github_pr_url(url)?;
    let mut command = Command::new(resolve_gh()?);
    command
        .args([
            "pr",
            "view",
            &url,
            "--json",
            "title,state,reviewDecision,isDraft,statusCheckRollup,latestReviews",
            "--jq",
            "{title,state,reviewDecision,isDraft,statusCheckRollup,latestReviews:[.latestReviews[] | {state}]}",
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env_remove("GH_DEBUG");
    let bytes = run(&mut command, REQUEST_TIMEOUT)?;
    serde_json::from_slice(&bytes).context("GitHub returned an unreadable PR status")
}

/// Locate `gh` the way an interactive terminal would, not just via the
/// app's inherited `PATH`. A Finder/Dock-launched macOS app gets a minimal
/// system `PATH` without mise shims, `~/.local/bin`, or Homebrew, while the
/// terminal tabs launch a login shell that sources all of that. Without
/// this, macOS users with a working terminal `gh` still see "Could not
/// start GitHub CLI".
pub(crate) fn resolve_gh() -> Result<PathBuf> {
    resolve_gh_from(
        env::var_os("PATH").as_deref(),
        env::var_os("HOME").as_deref(),
        || login_shell_lookup(SHELL_LOOKUP_TIMEOUT),
    )
}

fn resolve_gh_from(
    path_var: Option<&OsStr>,
    home: Option<&OsStr>,
    shell_lookup: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf> {
    let home_path;
    let home = match home {
        Some(home) if !home.is_empty() => {
            home_path = PathBuf::from(home);
            Some(home_path.as_path())
        }
        _ => None,
    };
    if let Some(found) =
        find_on_path(path_var).or_else(|| first_executable(well_known_candidates(home)))
    {
        return Ok(found);
    }
    if let Some(found) = shell_lookup() {
        return Ok(found);
    }
    bail!(
        "Could not start GitHub CLI (gh not found on PATH, in ~/.local/share/mise/shims, \
         ~/.local/bin, /opt/homebrew/bin, /usr/local/bin, or via your login shell). \
         Install gh and run gh auth login, then refresh"
    )
}

fn find_on_path(path_var: Option<&OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    for dir in env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("gh");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn well_known_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/gh"),
        PathBuf::from("/usr/local/bin/gh"),
        PathBuf::from("/opt/local/bin/gh"),
    ];
    if let Some(home) = home {
        candidates.insert(0, home.join(".local/bin/gh"));
        candidates.insert(0, home.join(".local/share/mise/shims/gh"));
    }
    candidates
}

fn first_executable(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| is_executable(path))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.is_file()
        && path
            .metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Ask the user's login shell where `gh` is, matching the environment the
/// terminal tabs already run with. Returns the first absolute executable
/// path printed by `command -v gh`, if any.
fn login_shell_lookup(timeout: Duration) -> Option<PathBuf> {
    let shell = env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_owned());
    let mut child = Command::new(shell)
        .args(["-lc", "command -v gh 2>/dev/null"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(8192 + 1).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let exit = loop {
        match child.try_wait() {
            Ok(Some(exit)) => break Some(exit),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    if !exit?.success() {
        return None;
    }
    let bytes = reader.join().ok()?.ok()?;
    parse_command_v_output(&bytes)
}

fn parse_command_v_output(bytes: &[u8]) -> Option<PathBuf> {
    let line = String::from_utf8_lossy(bytes)
        .lines()
        .next()?
        .trim()
        .to_owned();
    if line.is_empty() {
        return None;
    }
    // `command -v` can print a shell function body or alias instead of a
    // path; only accept absolute paths that actually execute.
    let path = PathBuf::from(line);
    if path.is_absolute() && is_executable(&path) {
        Some(path)
    } else {
        None
    }
}

pub(crate) fn run(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            anyhow::anyhow!(
                "Could not start GitHub CLI ({error}). Install gh and run gh auth login"
            )
        })?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    // Drain both pipes concurrently so large responses cannot deadlock `gh`.
    let read = |stream: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stream
                .take(OUTPUT_LIMIT + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let deadline = Instant::now() + timeout;
    let exit = loop {
        match child.try_wait() {
            Ok(Some(exit)) => break exit,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                if let Err(error) = result {
                    return Err(error.into());
                }
                bail!("GitHub request timed out. Retry when your connection is available");
            }
        }
    };
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("Could not read GitHub response"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("Could not read GitHub response"))??;
    if !exit.success() {
        let message = String::from_utf8_lossy(&stderr).to_lowercase();
        // Do not expose arbitrary CLI stderr (which may contain credentials).
        if exit.code() == Some(4)
            || message.contains("auth login")
            || message.contains("authentication")
            || message.contains("401")
        {
            bail!("GitHub authentication required. Run gh auth login, then refresh");
        }
        if message.contains("rate limit") {
            bail!("GitHub rate limit reached. Try again later");
        }
        bail!(
            "Could not fetch PR status. Check your connection and GitHub repository access, then refresh"
        );
    }
    if stdout.len() as u64 > OUTPUT_LIMIT {
        bail!("GitHub response exceeded the size limit")
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(checks: serde_json::Value) -> Status {
        serde_json::from_value(json!({"title":"PR", "state":"OPEN", "isDraft":false,
            "reviewDecision":"APPROVED", "statusCheckRollup":checks}))
        .unwrap()
    }

    #[test]
    fn ci_combines_check_runs_and_legacy_statuses_conservatively() {
        let pass = json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"});
        let pending = json!({"__typename":"StatusContext","state":"PENDING"});
        let fail = json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"});
        assert_eq!(status(json!([pass])).ci(), Ci::Passed);
        assert_eq!(status(json!([pass, pending])).ci(), Ci::Pending);
        assert_eq!(status(json!([pass, pending, fail])).ci(), Ci::Failed);
        assert_eq!(status(json!([])).ci(), Ci::None);
        assert_eq!(status(json!(null)).ci(), Ci::None);
        assert_eq!(
            status(json!([{"__typename":"FutureCheck"}])).ci(),
            Ci::Unknown
        );
        for conclusion in [
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
            "STALE",
        ] {
            assert_eq!(status(json!([{"__typename":"CheckRun", "status":"COMPLETED", "conclusion":conclusion}])).ci(), Ci::Failed);
        }
        for conclusion in ["NEUTRAL", "SKIPPED"] {
            assert_eq!(status(json!([{"__typename":"CheckRun", "status":"COMPLETED", "conclusion":conclusion}])).ci(), Ci::Passed);
        }
        assert_eq!(
            status(json!([{"__typename":"CheckRun", "status":"IN_PROGRESS", "conclusion":""}]))
                .ci(),
            Ci::Pending
        );
        assert_eq!(
            status(json!([{"__typename":"StatusContext", "state":"ERROR"}])).ci(),
            Ci::Failed
        );
    }

    #[test]
    fn approvals_use_github_decision_then_effective_reviews() {
        let mut pr = status(json!([]));
        assert_eq!(pr.approval(), "Approved");
        pr.review_decision = Some("REVIEW_REQUIRED".into());
        pr.latest_reviews = vec![Review {
            state: "APPROVED".into(),
        }];
        assert_eq!(pr.approval(), "Approval required");
        pr.review_decision = Some("".into());
        assert_eq!(pr.approval(), "Approved");
        pr.latest_reviews.push(Review {
            state: "CHANGES_REQUESTED".into(),
        });
        assert_eq!(pr.approval(), "Changes requested");
        pr.latest_reviews = vec![Review {
            state: "DISMISSED".into(),
        }];
        assert_eq!(pr.approval(), "Not reviewed");
        for state in ["OPEN", "CLOSED", "MERGED"] {
            let parsed: Status =
                serde_json::from_value(json!({"title":"PR", "state":state, "isDraft":false,
                "reviewDecision":null, "statusCheckRollup":null}))
                .unwrap();
            assert_eq!(parsed.state, state);
            assert_eq!(parsed.approval(), "Not reviewed");
        }
    }

    #[test]
    fn cache_throttles_retains_stale_status_and_discards_removed_urls() {
        let mut cache = Cache::default();
        let urls = vec!["a".to_owned()];
        assert_eq!(cache.begin(&urls, false), urls);
        assert!(cache.begin(&urls, true).is_empty());
        cache.finish("a", Ok(status(json!([]))));
        assert!(cache.begin(&urls, false).is_empty());
        assert_eq!(cache.begin(&urls, true), urls);
        cache.finish("a", Err(anyhow::anyhow!("Offline")));
        assert!(cache.entries["a"].status.is_some());
        assert_eq!(cache.entries["a"].error.as_deref(), Some("Offline"));
        cache.begin(&[], false);
        cache.finish("a", Ok(status(json!([]))));
        assert!(cache.entries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_times_out_and_reports_auth_without_echoing_stderr() {
        let error = run(
            Command::new("sh").args(["-c", "exec sleep 5"]),
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let error = run(
            Command::new("sh").args(["-c", "echo secret >&2; exit 4"]),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("gh auth login"));
        assert!(!error.to_string().contains("secret"));
    }

    #[cfg(unix)]
    fn fake_gh(dir: &std::path::Path, executable: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("gh");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
        .unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn gh_lookup_prefers_path_then_known_locations_then_shell() {
        let path_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let path_gh = fake_gh(path_dir.path(), true);
        let _other_gh = fake_gh(other_dir.path(), true);
        let shim_dir = home_dir.path().join(".local/share/mise/shims");
        std::fs::create_dir_all(&shim_dir).unwrap();
        let shim_gh = fake_gh(&shim_dir, true);

        // Inherited PATH wins.
        let path_var = std::env::join_paths([path_dir.path(), other_dir.path()]).unwrap();
        let found = resolve_gh_from(
            Some(path_var.as_os_str()),
            Some(home_dir.path().as_os_str()),
            || panic!("shell lookup must not run when PATH already resolves gh"),
        )
        .unwrap();
        assert_eq!(found, path_gh);

        // Mise shim covers GUI launches whose PATH lacks user tooling.
        let found = resolve_gh_from(
            Some(OsStr::new("/nonexistent")),
            Some(home_dir.path().as_os_str()),
            || panic!("shell lookup must not run when a known location resolves gh"),
        )
        .unwrap();
        assert_eq!(found, shim_gh);

        // Login-shell fallback for install locations we do not know about.
        let shell_gh = path_gh.clone();
        let found = resolve_gh_from(Some(OsStr::new("/nonexistent")), None, || {
            Some(shell_gh.clone())
        })
        .unwrap();
        assert_eq!(found, path_gh);

        // Total miss stays actionable and keeps the auth hint.
        let error = resolve_gh_from(Some(OsStr::new("/nonexistent")), None, || None).unwrap_err();
        assert!(error.to_string().contains("gh not found"));
        assert!(error.to_string().contains("gh auth login"));
    }

    #[cfg(unix)]
    #[test]
    fn gh_lookup_ignores_non_executable_and_shell_noise() {
        let dir = tempfile::tempdir().unwrap();
        let _plain_file = fake_gh(dir.path(), false);
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        assert!(find_on_path(Some(path_var.as_os_str())).is_none());

        // `command -v` may print an alias or function instead of a path.
        assert!(parse_command_v_output(b"alias gh='gh --paginate'\n").is_none());
        assert!(parse_command_v_output(b"gh () {\n  command gh \"$@\"\n}\n").is_none());
        assert!(parse_command_v_output(b"relative/gh\n").is_none());
        assert!(parse_command_v_output(b"\n").is_none());

        let executable = fake_gh(dir.path(), true);
        let output = format!("{}\n", executable.display());
        assert_eq!(parse_command_v_output(output.as_bytes()), Some(executable));
    }
}
