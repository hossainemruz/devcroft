//! GitHub acquisition and exact, revision-bound review publication.
//! Credentials remain with gh; remote source is never checked out or executed.
use super::{Capture, Review, digest};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Metadata {
    pub url: String,
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub description: String,
    pub author: String,
    pub target_tip: String,
    pub base_branch: String,
    pub head_branch: String,
    pub captured_at: i64,
    pub checks: Vec<Value>,
    pub checks_error: Option<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct Identity {
    pub url: String,
    pub repository: String,
    pub number: u64,
}
impl Identity {
    pub fn parse(url: &str) -> Result<Self> {
        let url = crate::data::dashboard::github_pr_url(url)?;
        let parts = url
            .trim_start_matches("https://github.com/")
            .split('/')
            .collect::<Vec<_>>();
        Ok(Self {
            repository: format!("{}/{}", parts[0], parts[1]),
            number: parts[3].parse()?,
            url,
        })
    }
    pub fn endpoint(&self) -> String {
        format!("repos/{}/pulls/{}", self.repository, self.number)
    }
    pub fn directory(&self) -> Result<PathBuf> {
        Ok(crate::data::resolve_data_root()?
            .root()
            .join("review-objects")
            .join(digest(&self.repository)))
    }
    pub fn store(&self) -> Result<super::Store> {
        let directory = self.directory()?;
        std::fs::create_dir_all(&directory)?;
        super::Store::open(&directory, &self.url)
    }
}
fn trusted_gh() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME unavailable")?);
    let cwd = std::env::current_dir()?.canonicalize()?;
    for path in [
        home.join(".local/share/mise/installs/gh/latest/bin/gh"),
        home.join(".local/bin/gh"),
        PathBuf::from("/opt/homebrew/bin/gh"),
        PathBuf::from("/usr/local/bin/gh"),
        PathBuf::from("/usr/bin/gh"),
    ] {
        if path.is_file() {
            let binary = path.canonicalize()?;
            ensure!(
                !binary.starts_with(&cwd),
                "GitHub CLI must be installed outside the active checkout"
            );
            return Ok(binary);
        }
    }
    anyhow::bail!("Install GitHub CLI using mise, Homebrew or ~/.local/bin and run gh auth login")
}
fn gh(endpoint: &str, method: &str, payload: Option<&Value>, paginate: bool) -> Result<Value> {
    let mut command = Command::new(trusted_gh()?);
    command
        .args([
            "api",
            "--hostname",
            "github.com",
            "--method",
            method,
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2026-03-10",
            endpoint,
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env_remove("GH_DEBUG");
    let mut input = None;
    if let Some(payload) = payload {
        use std::io::Write as _;
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(&serde_json::to_vec(payload)?)?;
        file.flush()?;
        command.arg("--input").arg(file.path());
        input = Some(file);
    }
    if paginate {
        command.args(["--paginate", "--slurp"]);
    }
    let bytes = crate::pull_requests::run(&mut command, Duration::from_secs(30))
        .context("GitHub review request failed")?;
    drop(input);
    serde_json::from_slice(&bytes).context("GitHub returned invalid review data")
}
fn bounded<'a>(v: &'a Value, pointer: &str, limit: usize) -> Result<&'a str> {
    let s = v
        .pointer(pointer)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing PR field {pointer}"))?;
    ensure!(s.len() <= limit, "Oversized PR field");
    Ok(s)
}
fn sha(s: &str) -> Result<&str> {
    ensure!(
        s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid GitHub commit SHA"
    );
    Ok(s)
}
fn git_command(directory: &Path) -> Command {
    let mut command = Command::new(if cfg!(target_os = "macos") {
        "/usr/bin/git"
    } else {
        "git"
    });
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.file.allow=never",
            "-c",
            "credential.helper=",
        ])
        .arg("-C")
        .arg(directory);
    command
}
fn fetch_objects(identity: &Identity, target: &str, head: &str) -> Result<PathBuf> {
    let directory = identity.directory()?;
    std::fs::create_dir_all(&directory)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("capture.lock"))?;
    lock.lock()?;
    if !directory.join("HEAD").is_file() {
        crate::pull_requests::run(
            git_command(&directory).args(["init", "--bare"]),
            Duration::from_secs(30),
        )?;
    }
    let gh = trusted_gh()?;
    // Git invokes only this installed credential helper, never remote code.
    let escaped = gh.to_string_lossy().replace('\'', "'\\''");
    let helper = format!("credential.helper=!'{}' auth git-credential", escaped);
    let remote = format!("https://github.com/{}.git", identity.repository);
    for object in [target, head] {
        crate::pull_requests::run(git_command(&directory).arg("-c").arg(&helper).args(["fetch","--no-tags","--no-write-fetch-head","--no-recurse-submodules","--",&remote,sha(object)?]),Duration::from_secs(60))
            .context("Could not fetch the exact PR objects. Refresh after a force push, or check gh authentication and repository access")?;
    }
    Ok(directory)
}
pub fn acquire(url: &str) -> Result<Capture> {
    let identity = Identity::parse(url)?;
    let data = gh(&identity.endpoint(), "GET", None, false)?;
    ensure!(
        bounded(&data, "/base/repo/full_name", 240)?.eq_ignore_ascii_case(&identity.repository),
        "PR repository identity mismatch"
    );
    let head = sha(bounded(&data, "/head/sha", 64)?)?.to_owned();
    let target = sha(bounded(&data, "/base/sha", 64)?)?.to_owned();
    let directory = fetch_objects(&identity, &target, &head)?;
    let (base, files) = crate::review::git::commit_files(&directory, &target, &head)?;
    let checks = gh(
        &format!(
            "repos/{}/commits/{head}/check-runs?per_page=100",
            identity.repository
        ),
        "GET",
        None,
        true,
    );
    let (checks,checks_error)=match checks {
        Ok(pages)=>(pages.as_array().context("Invalid CI pages")?.iter().flat_map(|p|p.get("check_runs").and_then(Value::as_array).into_iter().flatten()).take(500).map(|c|json!({"name":c.get("name"),"head_sha":c.get("head_sha"),"status":c.get("status"),"conclusion":c.get("conclusion"),"completed_at":c.get("completed_at")})).collect(),None),
        Err(e)=>(vec![],Some(format!("CI report unavailable: {e:#}"))),
    };
    let metadata = Metadata {
        url: identity.url,
        repository: identity.repository.clone(),
        number: identity.number,
        title: bounded(&data, "/title", 1000)?.into(),
        description: data
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(20000)
            .collect(),
        author: bounded(&data, "/user/login", 240)?.into(),
        target_tip: target,
        base_branch: bounded(&data, "/base/ref", 240)?.into(),
        head_branch: bounded(&data, "/head/ref", 240)?.into(),
        captured_at: crate::relative_time::current_unix_secs(),
        checks,
        checks_error,
    };
    Capture::from_files(
        &format!("GitHub PR · {} #{}", metadata.repository, metadata.number),
        &base,
        &head,
        Some(metadata.head_branch.clone()),
        files,
    )?
    .with_pr(metadata)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Preview {
    pub capture: String,
    pub version: u64,
    pub hash: String,
    pub event: String,
    pub payload: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Submission {
    pub preview: Preview,
    pub remote_id: Option<u64>,
    pub remote_url: Option<String>,
    pub state: String,
    pub error: Option<String>,
}
pub fn preview(review: &Review, event: &str, body: &str) -> Result<Preview> {
    ensure!(
        ["COMMENT", "REQUEST_CHANGES", "APPROVE"].contains(&event),
        "Invalid review event"
    );
    ensure!(body.len() <= 12000, "Review body too long");
    let capture = &review.active().capture;
    let metadata = capture
        .pr
        .as_ref()
        .context("Publication requires a captured GitHub PR")?;
    let mut text = body.trim().to_owned();
    text.push_str(&format!(
        "\n\nReviewed {} at `{}` (merge base `{}`).",
        metadata.url, capture.head, capture.base
    ));
    let mut comments = vec![];
    for finding in review
        .findings
        .iter()
        .filter(|f| !f.resolved && f.capture == capture.id)
    {
        let evidence = finding
            .evidence
            .as_deref()
            .and_then(|id| capture.evidence(id).ok());
        let range = finding.range.as_ref();
        let inline = evidence.zip(range).filter(|(e, r)| {
            let file = capture.files.iter().find(|f| f.path == e.path);
            r.start <= r.end
                && r.start >= e.start
                && r.end <= e.end
                && file.is_some_and(|f| {
                    !f.truncated
                        && f.unavailable.is_none()
                        && (r.start..=r.end).all(|n| {
                            f.lines.iter().any(|l| match e.side {
                                super::Side::Old => l.old == Some(n) && l.tag != "addition",
                                super::Side::New => l.new == Some(n) && l.tag != "deletion",
                            })
                        })
                })
        });
        if let Some((e, r)) = inline {
            let side = if e.side == super::Side::Old {
                "LEFT"
            } else {
                "RIGHT"
            };
            let mut comment = json!({"path":e.path,"line":r.end,"side":side,"body":finding.body});
            if r.start != r.end {
                comment["start_line"] = json!(r.start);
                comment["start_side"] = json!(side);
            }
            comments.push(comment);
        } else {
            text.push_str("\n\n- ");
            if let Some(e) = evidence {
                text.push_str(&format!(
                    "`{}` ({} {}–{}): ",
                    e.path,
                    if e.side == super::Side::Old {
                        "base"
                    } else {
                        "head"
                    },
                    e.start,
                    e.end
                ));
            }
            text.push_str(&finding.body);
        }
    }
    let payload = json!({"commit_id":capture.head,"body":text,"comments":comments});
    ensure!(
        payload["comments"].as_array().unwrap().len() <= 100
            && serde_json::to_vec(&payload)?.len() <= 512 * 1024,
        "Review exceeds publication size limits; resolve or reduce findings before publishing"
    );
    let hash = digest(serde_json::to_vec(&(
        &capture.id,
        review.version,
        event,
        &payload,
    ))?);
    let mut payload = payload;
    payload["body"] = json!(format!(
        "{}\n\n<!-- devcroft-review:{hash} -->",
        payload["body"].as_str().unwrap()
    ));
    Ok(Preview {
        capture: capture.id.clone(),
        version: review.version,
        hash,
        event: event.into(),
        payload,
    })
}
fn identity_for(capture: &Capture) -> Result<Identity> {
    Identity::parse(&capture.pr.as_ref().context("Not a GitHub PR")?.url)
}
pub fn verify_head(capture: &Capture) -> Result<()> {
    let identity = identity_for(capture)?;
    let latest = gh(&identity.endpoint(), "GET", None, false)?;
    ensure!(
        bounded(&latest, "/head/sha", 64)? == capture.head
            && bounded(&latest, "/base/sha", 64)? == capture.pr.as_ref().unwrap().target_tip,
        "PR head or target changed. Capture a new revision and inspect it before publishing"
    );
    Ok(())
}
/// Reconcile ambiguous requests by their exact commit/body and remote ID. A
/// timeout during creation is never retried with another POST, avoiding duplicates.
pub fn reconcile(capture: &Capture, submission: &Submission) -> Result<Option<Value>> {
    let identity = identity_for(capture)?;
    let pages = gh(
        &format!("{}/reviews?per_page=100", identity.endpoint()),
        "GET",
        None,
        true,
    )?;
    let body = submission.preview.payload["body"]
        .as_str()
        .context("Invalid saved publication")?;
    let matches = pages
        .as_array()
        .context("Invalid review list")?
        .iter()
        .flat_map(|p| p.as_array().into_iter().flatten())
        .filter(|r| {
            r.get("body").and_then(Value::as_str) == Some(body)
                && r.get("commit_id").and_then(Value::as_str) == Some(&capture.head)
                && submission
                    .remote_id
                    .is_none_or(|id| r.get("id").and_then(Value::as_u64) == Some(id))
        })
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        matches.len() <= 1,
        "Multiple matching remote reviews; inspect GitHub before proceeding"
    );
    Ok(matches.into_iter().next())
}
pub fn create_pending(capture: &Capture, preview: &Preview) -> Result<Value> {
    verify_head(capture)?;
    gh(
        &format!("{}/reviews", identity_for(capture)?.endpoint()),
        "POST",
        Some(&preview.payload),
        false,
    )
}
pub fn submit_pending(capture: &Capture, submission: &Submission) -> Result<Value> {
    verify_head(capture)?;
    let id = submission
        .remote_id
        .context("Remote review ID is unknown; reconcile before continuing")?;
    gh(
        &format!("{}/reviews/{id}/events", identity_for(capture)?.endpoint()),
        "POST",
        Some(&json!({"event":submission.preview.event,"body":submission.preview.payload["body"]})),
        false,
    )
}
