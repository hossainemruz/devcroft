//! Read-only GitHub status snapshots. Credentials stay with `gh`; fetched data
//! stays in memory and never rewrites the user's portable PR assignments.
use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
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
    let mut command = Command::new("gh");
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

fn run(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not start GitHub CLI. Install gh and run gh auth login")?;
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
}
