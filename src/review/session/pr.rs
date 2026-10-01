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
    let checkout = cwd.join(".git").exists();
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
                !checkout || !binary.starts_with(&cwd),
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
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_SSH")
        .env_remove("GIT_SSH_COMMAND")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
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

/// Give the normal interactive agent a real checkout, never the bare object
/// database. Prefer the repository's linked checkout without changing it.
pub(crate) fn authoring_checkout(capture: &Capture) -> Result<PathBuf> {
    let metadata = capture
        .pr
        .as_ref()
        .context("Missing PR authoring identity")?;
    let root = crate::data::resolve_data_root()?;
    let (repositories, _) = crate::data::all_repositories(&root)?;
    for repository in repositories {
        let identity = match (&repository.owner, &repository.name) {
            (Some(owner), Some(name)) => format!("{owner}/{name}"),
            _ => continue,
        };
        if identity.eq_ignore_ascii_case(&metadata.repository)
            && let Some(checkout) = crate::data::checkout_for(&root, &repository.key)
            && linked_checkout_matches(&checkout, &metadata.repository)
        {
            return checkout
                .canonicalize()
                .context("Opening linked PR agent checkout");
        }
    }
    let identity = Identity::parse(&metadata.url)?;
    let objects = identity.directory()?;
    // Different captured heads have separate working directories. Reopening
    // never resets an existing checkout or discards agent/user edits.
    let checkout = root
        .root()
        .join("review-checkouts")
        .join(digest(&metadata.url))
        .join(sha(&capture.head)?);
    ensure_authoring_checkout(&objects, &checkout, &capture.head)?;
    Ok(checkout)
}
fn linked_checkout_matches(checkout: &Path, repository: &str) -> bool {
    let result = (|| -> Result<bool> {
        let top = crate::pull_requests::run(
            git_command(checkout).args(["rev-parse", "--show-toplevel"]),
            Duration::from_secs(5),
        )?;
        let top = PathBuf::from(String::from_utf8(top)?.trim());
        ensure!(
            top.canonicalize()? == checkout.canonicalize()?,
            "Linked path is not a repository root"
        );
        let remotes = crate::pull_requests::run(
            git_command(checkout).args(["config", "--get-regexp", "^remote\\..*\\.url$"]),
            Duration::from_secs(5),
        )?;
        Ok(String::from_utf8(remotes)?.lines().any(|line| {
            let Some((_, url)) = line.split_once(' ') else {
                return false;
            };
            let path = [
                "https://github.com/",
                "http://github.com/",
                "git@github.com:",
                "ssh://git@github.com/",
                "ssh://github.com/",
            ]
            .iter()
            .find_map(|prefix| url.strip_prefix(prefix));
            path.is_some_and(|path| {
                path.trim_end_matches('/')
                    .trim_end_matches(".git")
                    .eq_ignore_ascii_case(repository)
            })
        }))
    })();
    result.unwrap_or(false)
}
pub(crate) fn checkout_head(checkout: &Path) -> Option<String> {
    let bytes = crate::pull_requests::run(
        git_command(checkout).args(["rev-parse", "HEAD"]),
        Duration::from_secs(5),
    )
    .ok()?;
    let head = String::from_utf8(bytes).ok()?.trim().to_owned();
    sha(&head).ok()?;
    Some(head)
}
fn ensure_authoring_checkout(objects: &Path, checkout: &Path, head: &str) -> Result<()> {
    if checkout.exists() {
        let common = crate::pull_requests::run(
            git_command(checkout).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]),
            Duration::from_secs(5),
        ).with_context(|| format!("Managed PR checkout is incomplete or unavailable at {}. Link an existing checkout in repository settings, then try generation again", checkout.display()))?;
        ensure!(
            PathBuf::from(String::from_utf8(common)?.trim()).canonicalize()?
                == objects.canonicalize()?,
            "Managed PR checkout belongs to another repository"
        );
        return Ok(());
    }
    std::fs::create_dir_all(
        checkout
            .parent()
            .context("Missing review checkout parent")?,
    )?;
    crate::pull_requests::run(
        git_command(objects).args(["worktree", "add", "--detach", "--"]).arg(checkout).arg(sha(head)?),
        Duration::from_secs(30),
    ).context("Could not create the captured PR's agent checkout. Link a checkout in repository settings or refresh unavailable Git objects")?;
    Ok(())
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
    #[serde(default)]
    pub remote_author: Option<u64>,
    #[serde(default)]
    pub event_sent: bool,
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
            && bounded(&latest, "/base/sha", 64)? == capture.pr.as_ref().unwrap().target_tip
            && bounded(&latest, "/base/ref", 240)? == capture.pr.as_ref().unwrap().base_branch
            && bounded(&latest, "/base/repo/full_name", 240)?
                .eq_ignore_ascii_case(&identity.repository),
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
    let Some(remote) = matches.into_iter().next() else {
        return Ok(None);
    };
    let id = remote["id"].as_u64().context("Remote review ID missing")?;
    let pages = gh(
        &format!("{}/reviews/{id}/comments?per_page=100", identity.endpoint()),
        "GET",
        None,
        true,
    )?;
    let comments = pages
        .as_array()
        .context("Invalid review comments")?
        .iter()
        .flat_map(|p| p.as_array().into_iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        comments_match(&submission.preview, &comments)?,
        "Remote inline comments differ from the approved preview. Inspect GitHub; this review will not be submitted"
    );
    Ok(Some(remote))
}
pub fn create_pending(capture: &Capture, preview: &Preview) -> Result<Value> {
    verify_head(capture).map_err(|e| CreationNotSent(format!("{e:#}")))?;
    gh(
        &format!("{}/reviews", identity_for(capture)?.endpoint()),
        "POST",
        Some(&preview.payload),
        false,
    )
}

#[derive(Debug)]
struct CreationNotSent(String);
impl std::fmt::Display for CreationNotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for CreationNotSent {}

trait Remote {
    fn create(&self, capture: &Capture, preview: &Preview) -> Result<Value>;
    fn reconcile(&self, capture: &Capture, submission: &Submission) -> Result<Option<Value>>;
    fn submit(&self, capture: &Capture, submission: &Submission) -> Result<Value>;
    fn discard(&self, capture: &Capture, submission: &Submission) -> Result<()>;
    fn known_missing(&self, capture: &Capture, submission: &Submission) -> Result<bool>;
}
struct GitHub;
impl Remote for GitHub {
    fn create(&self, capture: &Capture, preview: &Preview) -> Result<Value> {
        create_pending(capture, preview)
    }
    fn reconcile(&self, capture: &Capture, submission: &Submission) -> Result<Option<Value>> {
        reconcile(capture, submission)
    }
    fn submit(&self, capture: &Capture, submission: &Submission) -> Result<Value> {
        submit_pending(capture, submission)
    }
    fn discard(&self, capture: &Capture, submission: &Submission) -> Result<()> {
        let id = submission.remote_id.context("Pending review ID missing")?;
        let deleted = gh(
            &format!("{}/reviews/{id}", identity_for(capture)?.endpoint()),
            "DELETE",
            None,
            false,
        )?;
        ensure!(
            deleted["id"].as_u64() == Some(id)
                && deleted["body"] == submission.preview.payload["body"]
                && deleted["commit_id"] == submission.preview.payload["commit_id"]
                && deleted["user"]["id"].as_u64() == submission.remote_author,
            "Unexpected deleted review response; reconcile its status"
        );
        Ok(())
    }
    fn known_missing(&self, capture: &Capture, submission: &Submission) -> Result<bool> {
        let identity = identity_for(capture)?;
        let id = submission.remote_id.context("Pending review ID missing")?;
        match gh(
            &format!("{}/reviews/{id}", identity.endpoint()),
            "GET",
            None,
            false,
        ) {
            Ok(_) => Ok(false),
            Err(error)
                if error
                    .downcast_ref::<crate::pull_requests::GitHubRejection>()
                    .is_some_and(|e| e.0 == 404) =>
            {
                // A hidden private resource or a different gh account is not
                // proof that the old account's pending review was removed.
                gh(&identity.endpoint(), "GET", None, false)?;
                let user = gh("user", "GET", None, false)?;
                ensure!(
                    submission.remote_author.is_some()
                        && user["id"].as_u64() == submission.remote_author,
                    "Use the GitHub account that created this draft to confirm its removal"
                );
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }
}
pub fn submit_pending(capture: &Capture, submission: &Submission) -> Result<Value> {
    verify_head(capture).map_err(|e| EventNotSent(format!("{e:#}")))?;
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

#[derive(Debug)]
struct EventNotSent(String);
impl std::fmt::Display for EventNotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for EventNotSent {}

fn comments_match(preview: &Preview, comments: &[Value]) -> Result<bool> {
    fn canonical(c: &Value) -> Value {
        json!({"path":c["path"],"body":c["body"],"line":c["line"],"side":c["side"],
            "start_line":c.get("start_line").filter(|v| !v.is_null()),
            "start_side":c.get("start_side").filter(|v| !v.is_null())})
    }
    let expected = preview.payload["comments"]
        .as_array()
        .context("Invalid preview comments")?;
    if expected.len() != comments.len()
        || comments
            .iter()
            .any(|c| c["commit_id"] != preview.payload["commit_id"])
    {
        return Ok(false);
    }
    let mut a = expected
        .iter()
        .map(|c| canonical(c).to_string())
        .collect::<Vec<_>>();
    let mut b = comments
        .iter()
        .map(|c| canonical(c).to_string())
        .collect::<Vec<_>>();
    a.sort();
    b.sort();
    Ok(a == b)
}
fn expected_state(event: &str) -> Result<&str> {
    match event {
        "APPROVE" => Ok("APPROVED"),
        "COMMENT" => Ok("COMMENTED"),
        "REQUEST_CHANGES" => Ok("CHANGES_REQUESTED"),
        _ => anyhow::bail!("Invalid saved review event"),
    }
}
fn record_remote(submission: &mut Submission, remote: &Value) -> Result<()> {
    ensure!(
        !matches!(
            submission.state.as_str(),
            "submitted" | "rejected" | "discarded"
        ),
        "Publication already completed in another operation"
    );
    ensure!(
        remote["body"] == submission.preview.payload["body"]
            && remote["commit_id"] == submission.preview.payload["commit_id"],
        "Remote review differs from the approved preview"
    );
    let id = remote["id"].as_u64().context("Remote review ID missing")?;
    let author = remote["user"]["id"]
        .as_u64()
        .context("Remote review author missing")?;
    ensure!(
        submission.remote_id.is_none_or(|old| old == id)
            && submission.remote_author.is_none_or(|old| old == author),
        "Remote review identity changed"
    );
    submission.remote_id = Some(id);
    submission.remote_author = Some(author);
    submission.remote_url = remote["html_url"].as_str().map(str::to_owned);
    let state = remote["state"]
        .as_str()
        .context("Remote review state missing")?;
    submission.state = if state == "PENDING" {
        if submission.state == "discarding" {
            "discarding"
        } else if submission.event_sent {
            "submitting"
        } else {
            "pending"
        }
    } else {
        ensure!(
            state == expected_state(&submission.preview.event)?,
            "Remote review has a different decision; inspect GitHub before continuing"
        );
        "submitted"
    }
    .into();
    submission.error = None;
    Ok(())
}
fn update_submission(
    store: &super::Store,
    hash: &str,
    update: impl Fn(&mut Submission) -> Result<()>,
) -> Result<Review> {
    // Preserve simultaneous reviewer edits; only this frozen intent is changed.
    for _ in 0..3 {
        let mut review = store.load()?.context("Saved review missing")?;
        let submission = review
            .submissions
            .iter_mut()
            .find(|s| s.preview.hash == hash)
            .context("Saved publication missing")?;
        update(submission)?;
        match store.save(&mut review) {
            Ok(()) => return Ok(review),
            Err(_)
                if store
                    .load()?
                    .is_some_and(|latest| latest.version != review.version) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    anyhow::bail!(
        "Review changed repeatedly while recording publication. Reload and reconcile the saved intent"
    )
}
/// The caller persists the intent before entering this operation. Only a new
/// explicit publish creates a draft. Recovery always starts with a GET.
pub fn complete_publication(
    store: &super::Store,
    hash: &str,
    create: bool,
    submit: bool,
) -> Result<Review> {
    complete_with_remote(store, hash, create, submit, &GitHub)
}

/// Only an explicit reviewer action can discard a known pending draft. An
/// unknown creation result cannot be abandoned locally to permit another POST.
pub fn discard_publication(store: &super::Store, hash: &str) -> Result<Review> {
    discard_with_remote(store, hash, &GitHub)
}
fn discard_with_remote(store: &super::Store, hash: &str, remote: &impl Remote) -> Result<Review> {
    let _operation = store.publication_lock()?;
    let review = store.load()?.context("Saved review missing")?;
    let submission = review
        .submissions
        .iter()
        .find(|s| s.preview.hash == hash)
        .context("Saved publication missing")?
        .clone();
    ensure!(
        !matches!(
            submission.state.as_str(),
            "submitted" | "rejected" | "discarded"
        ),
        "This publication is already complete"
    );
    ensure!(
        submission.remote_id.is_some(),
        "The remote draft ID is unknown. Reconcile creation before discarding; another creation remains prohibited"
    );
    let capture = &review
        .revisions
        .iter()
        .find(|r| r.capture.id == submission.preview.capture)
        .context("Publication capture missing")?
        .capture;
    let result = (|| {
        if let Some(found) = remote.reconcile(capture, &submission)? {
            let mut verified = submission.clone();
            record_remote(&mut verified, &found)?;
            if verified.state == "submitted" {
                return update_submission(store, hash, |s| record_remote(s, &found));
            }
            ensure!(
                found["state"] == "PENDING",
                "Only an exact pending review can be discarded"
            );
            // Save deletion intent before the mutation. A lost response is
            // recovered by checking this exact ID, never by creating a review.
            update_submission(store, hash, |s| {
                ensure!(
                    !matches!(s.state.as_str(), "submitted" | "rejected" | "discarded"),
                    "Publication completed before discard"
                );
                record_remote(s, &found)?;
                s.state = "discarding".into();
                Ok(())
            })?;
            remote.discard(capture, &verified)?;
        } else {
            ensure!(
                remote.known_missing(capture, &submission)?,
                "The known draft still exists but differs from the saved preview. Inspect GitHub before discarding"
            );
        }
        update_submission(store, hash, |s| {
            ensure!(
                !matches!(s.state.as_str(), "submitted" | "rejected"),
                "Publication completed before discard"
            );
            s.state = "discarded".into();
            s.error = None;
            Ok(())
        })
    })();
    if let Err(error) = &result {
        update_submission(store, hash, |s| {
            s.error = Some(format!("{error:#}"));
            Ok(())
        })?;
    }
    result
}
fn complete_with_remote(
    store: &super::Store,
    hash: &str,
    create: bool,
    submit: bool,
    remote: &impl Remote,
) -> Result<Review> {
    let _operation = store.publication_lock()?;
    let mut rejected = false;
    let mut event_not_sent = false;
    let result = (|| {
        let mut review = store.load()?.context("Saved review missing")?;
        let mut submission = review
            .submissions
            .iter()
            .find(|s| s.preview.hash == hash)
            .context("Saved publication missing")?
            .clone();
        ensure!(
            !matches!(
                submission.state.as_str(),
                "submitted" | "rejected" | "discarded"
            ),
            "This publication is complete or was rejected; create a fresh preview"
        );
        let capture = review
            .revisions
            .iter()
            .find(|r| r.capture.id == submission.preview.capture)
            .context("Publication capture missing")?
            .capture
            .clone();
        if submission.state == "discarding" {
            if let Some(found) = remote.reconcile(&capture, &submission)? {
                return update_submission(store, hash, |s| record_remote(s, &found));
            }
            ensure!(
                remote.known_missing(&capture, &submission)?,
                "Draft removal is unresolved; inspect and retry deleting this exact draft"
            );
            return update_submission(store, hash, |s| {
                s.state = "discarded".into();
                s.error = None;
                Ok(())
            });
        }
        if create {
            ensure!(
                submission.state == "creating" && submission.remote_id.is_none(),
                "This intent must be reconciled; creating another review is prohibited"
            );
            let created = remote
                .create(&capture, &submission.preview)
                .inspect_err(|e| {
                    rejected = e.downcast_ref::<CreationNotSent>().is_some()
                        || e.downcast_ref::<crate::pull_requests::GitHubRejection>()
                            .is_some();
                })?;
            review = update_submission(store, hash, |s| record_remote(s, &created))?;
            submission = review
                .submissions
                .iter()
                .find(|s| s.preview.hash == hash)
                .unwrap()
                .clone();
        }
        let reconciled = remote.reconcile(&capture, &submission)?.context("No matching remote review was found. Its creation may have failed or may still be processing. Inspect GitHub and reconcile again; no duplicate will be created")?;
        review = update_submission(store, hash, |s| record_remote(s, &reconciled))?;
        submission = review
            .submissions
            .iter()
            .find(|s| s.preview.hash == hash)
            .unwrap()
            .clone();
        if submission.state == "pending" && submit {
            // A one-shot event intent is durable before POST. An ambiguous
            // response permits GET reconciliation, never another event POST.
            update_submission(store, hash, |s| {
                ensure!(
                    s.state == "pending" && !s.event_sent,
                    "The final event is already in progress or unresolved"
                );
                s.event_sent = true;
                s.state = "submitting".into();
                Ok(())
            })?;
            let submitted = remote.submit(&capture, &submission).inspect_err(|e| {
                event_not_sent = e.downcast_ref::<EventNotSent>().is_some()
                    || e.downcast_ref::<crate::pull_requests::GitHubRejection>()
                        .is_some();
            })?;
            review = update_submission(store, hash, |s| record_remote(s, &submitted))?;
        }
        Ok(review)
    })();
    if let Err(e) = &result {
        let message = format!("{e:#}");
        update_submission(store, hash, |s| {
            if !matches!(
                s.state.as_str(),
                "submitted" | "rejected" | "discarded" | "discarding"
            ) {
                if event_not_sent {
                    s.event_sent = false;
                }
                s.state = if rejected && s.remote_id.is_none() {
                    "rejected"
                } else {
                    "uncertain"
                }
                .into();
                s.error = Some(message.clone());
            }
            Ok(())
        })
        .with_context(|| {
            format!("{message}; could not save the publication status. Reload and reconcile")
        })?;
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn linked_agent_checkout_must_be_a_git_root_with_matching_github_remote() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!linked_checkout_matches(tmp.path(), "owner/repo"));
        let run = |arguments: &[&str]| {
            let output = Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .arg("-C")
                .arg(tmp.path())
                .args(arguments)
                .output()
                .unwrap();
            assert!(output.status.success());
        };
        run(&["init", "-q"]);
        run(&[
            "remote",
            "add",
            "origin",
            "https://github.com/unrelated/repo.git",
        ]);
        assert!(!linked_checkout_matches(tmp.path(), "owner/repo"));
        run(&["remote", "add", "upstream", "git@github.com:owner/repo.git"]);
        assert!(linked_checkout_matches(tmp.path(), "owner/repo"));
        let nested = tmp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert!(!linked_checkout_matches(&nested, "owner/repo"));
    }
    #[test]
    fn managed_agent_checkout_uses_exact_head_without_resetting_follow_up_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let run = |arguments: &[&str]| {
            let output = Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .arg("-C")
                .arg(&source)
                .args(arguments)
                .output()
                .unwrap();
            assert!(output.status.success());
        };
        run(&["init", "-q"]);
        std::fs::write(source.join("retry.rs"), "captured source").unwrap();
        run(&["add", "retry.rs"]);
        run(&[
            "-c",
            "user.name=QA",
            "-c",
            "user.email=qa@example.test",
            "commit",
            "-qm",
            "synthetic capture",
        ]);
        let head = checkout_head(&source).unwrap();
        let objects = tmp.path().join("objects.git");
        let output = Command::new("git")
            .args(["clone", "--bare", "--quiet"])
            .arg(&source)
            .arg(&objects)
            .output()
            .unwrap();
        assert!(output.status.success());
        let checkout = tmp.path().join("managed");
        ensure_authoring_checkout(&objects, &checkout, &head).unwrap();
        assert_eq!(checkout_head(&checkout).as_deref(), Some(head.as_str()));
        assert_eq!(
            std::fs::read_to_string(checkout.join("retry.rs")).unwrap(),
            "captured source"
        );
        std::fs::write(checkout.join("retry.rs"), "ongoing agent edit").unwrap();
        ensure_authoring_checkout(&objects, &checkout, &head).unwrap();
        assert_eq!(
            std::fs::read_to_string(checkout.join("retry.rs")).unwrap(),
            "ongoing agent edit"
        );
        assert!(ensure_authoring_checkout(&source.join(".git"), &checkout, &head).is_err());
    }
    use super::*;
    use std::cell::{Cell, RefCell};
    struct FakeRemote {
        create_count: Cell<usize>,
        submit_count: Cell<usize>,
        discard_count: Cell<usize>,
        response: RefCell<Option<Value>>,
        reject: bool,
        lose_create_response: bool,
        lose_submit_response: bool,
        lose_discard_response: bool,
        leave_pending_on_loss: bool,
        head_changed: Cell<bool>,
        store: super::super::Store,
    }
    impl Remote for FakeRemote {
        fn create(&self, _: &Capture, preview: &Preview) -> Result<Value> {
            self.create_count.set(self.create_count.get() + 1);
            if self.reject {
                return Err(crate::pull_requests::GitHubRejection(422).into());
            }
            let response = json!({"id":42,"user":{"id":7},"body":preview.payload["body"],
                "commit_id":preview.payload["commit_id"],"state":"PENDING"});
            *self.response.borrow_mut() = Some(response.clone());
            if self.lose_create_response {
                anyhow::bail!("Synthetic lost response");
            }
            Ok(response)
        }
        fn reconcile(&self, _: &Capture, _: &Submission) -> Result<Option<Value>> {
            Ok(self.response.borrow().clone())
        }
        fn submit(&self, _: &Capture, submission: &Submission) -> Result<Value> {
            if self.head_changed.get() {
                return Err(EventNotSent("Synthetic PR head changed".into()).into());
            }
            // A crash at this point must still leave the remote ID recoverable.
            assert_eq!(
                self.store.load()?.unwrap().submissions[0].remote_id,
                Some(42)
            );
            assert!(self.store.load()?.unwrap().submissions[0].event_sent);
            self.submit_count.set(self.submit_count.get() + 1);
            ensure!(
                !(self.lose_submit_response && self.leave_pending_on_loss),
                "Synthetic lost event response with stale pending GET"
            );
            let mut response = self.response.borrow().clone().unwrap();
            response["state"] = json!(expected_state(&submission.preview.event)?);
            *self.response.borrow_mut() = Some(response.clone());
            if self.lose_submit_response {
                anyhow::bail!("Synthetic lost event response");
            }
            Ok(response)
        }
        fn discard(&self, _: &Capture, submission: &Submission) -> Result<()> {
            let saved = self.store.load()?.unwrap();
            assert_eq!(saved.submissions[0].state, "discarding");
            assert_eq!(submission.remote_id, Some(42));
            self.discard_count.set(self.discard_count.get() + 1);
            self.response.borrow_mut().take();
            ensure!(
                !self.lose_discard_response,
                "Synthetic lost deletion response"
            );
            Ok(())
        }
        fn known_missing(&self, _: &Capture, submission: &Submission) -> Result<bool> {
            Ok(submission.remote_id == Some(42)
                && submission.remote_author == Some(7)
                && self.response.borrow().is_none())
        }
    }
    fn saved_intent() -> (tempfile::TempDir, super::super::Store, String) {
        let cwd = tempfile::tempdir().unwrap();
        let store = super::super::Store::at(cwd.path().join("review"));
        let mut review = review();
        let preview = preview(&review, "COMMENT", "Synthetic review").unwrap();
        let hash = preview.hash.clone();
        review.submissions.push(Submission {
            preview,
            remote_id: None,
            remote_author: None,
            event_sent: false,
            remote_url: None,
            state: "creating".into(),
            error: None,
        });
        store.save(&mut review).unwrap();
        (cwd, store, hash)
    }
    fn fake(store: &super::super::Store) -> FakeRemote {
        FakeRemote {
            create_count: Cell::new(0),
            submit_count: Cell::new(0),
            discard_count: Cell::new(0),
            response: RefCell::new(None),
            reject: false,
            lose_create_response: false,
            lose_submit_response: false,
            lose_discard_response: false,
            leave_pending_on_loss: false,
            head_changed: Cell::new(false),
            store: store.clone(),
        }
    }
    #[test]
    fn stale_known_pending_draft_can_be_deliberately_discarded() {
        let (_cwd, store, hash) = saved_intent();
        let remote = fake(&store);
        remote.head_changed.set(true);
        assert!(complete_with_remote(&store, &hash, true, true, &remote).is_err());
        let saved = store.load().unwrap().unwrap();
        assert_eq!(saved.submissions[0].remote_id, Some(42));
        assert!(!saved.submissions[0].event_sent);
        let saved = discard_with_remote(&store, &hash, &remote).unwrap();
        assert_eq!(saved.submissions[0].state, "discarded");
        assert_eq!(remote.discard_count.get(), 1);
        assert_eq!(remote.submit_count.get(), 0);
    }
    #[test]
    fn lost_deletion_response_reconciles_exact_id_without_another_delete() {
        let (_cwd, store, hash) = saved_intent();
        let mut remote = fake(&store);
        complete_with_remote(&store, &hash, true, false, &remote).unwrap();
        remote.lose_discard_response = true;
        assert!(discard_with_remote(&store, &hash, &remote).is_err());
        assert_eq!(
            store.load().unwrap().unwrap().submissions[0].state,
            "discarding"
        );
        let saved = complete_with_remote(&store, &hash, false, true, &remote).unwrap();
        assert_eq!(saved.submissions[0].state, "discarded");
        assert_eq!(remote.discard_count.get(), 1);
        assert_eq!(remote.create_count.get(), 1);
        assert_eq!(remote.submit_count.get(), 0);
    }
    #[test]
    fn discard_rejects_unknown_identity_and_changed_remote_payload() {
        let (_cwd, store, hash) = saved_intent();
        let remote = fake(&store);
        assert!(discard_with_remote(&store, &hash, &remote).is_err());
        complete_with_remote(&store, &hash, true, false, &remote).unwrap();
        remote.response.borrow_mut().as_mut().unwrap()["body"] = json!("Edited elsewhere");
        assert!(discard_with_remote(&store, &hash, &remote).is_err());
        assert_eq!(remote.discard_count.get(), 0);
        remote.response.borrow_mut().as_mut().unwrap()["body"] =
            store.load().unwrap().unwrap().submissions[0]
                .preview
                .payload["body"]
                .clone();
        remote.response.borrow_mut().as_mut().unwrap()["state"] = json!("COMMENTED");
        let saved = discard_with_remote(&store, &hash, &remote).unwrap();
        assert_eq!(saved.submissions[0].state, "submitted");
        assert_eq!(remote.discard_count.get(), 0);
    }
    #[test]
    fn stale_pending_get_after_lost_event_never_repeats_the_post() {
        let (_cwd, store, hash) = saved_intent();
        let mut remote = fake(&store);
        remote.lose_submit_response = true;
        remote.leave_pending_on_loss = true;
        assert!(complete_with_remote(&store, &hash, true, true, &remote).is_err());
        for _ in 0..2 {
            let saved = complete_with_remote(&store, &hash, false, true, &remote).unwrap();
            assert_eq!(saved.submissions[0].state, "submitting");
        }
        assert_eq!(remote.submit_count.get(), 1);
        assert_eq!(remote.create_count.get(), 1);
    }
    #[test]
    fn publication_operations_are_serialized_across_windows() {
        let (_cwd, store, hash) = saved_intent();
        let remote = fake(&store);
        let guard = store.publication_lock().unwrap();
        assert!(complete_with_remote(&store, &hash, true, true, &remote).is_err());
        assert!(discard_with_remote(&store, &hash, &remote).is_err());
        assert_eq!(remote.create_count.get(), 0);
        drop(guard);
        complete_with_remote(&store, &hash, true, true, &remote).unwrap();
        assert_eq!(remote.create_count.get(), 1);
        assert_eq!(remote.submit_count.get(), 1);
    }
    #[test]
    fn lost_creation_response_recovers_without_duplicate_post() {
        let (_cwd, store, hash) = saved_intent();
        let remote = FakeRemote {
            create_count: Cell::new(0),
            submit_count: Cell::new(0),
            discard_count: Cell::new(0),
            response: RefCell::new(None),
            reject: false,
            lose_create_response: true,
            lose_submit_response: false,
            lose_discard_response: false,
            leave_pending_on_loss: false,
            head_changed: Cell::new(false),
            store: store.clone(),
        };
        assert!(complete_with_remote(&store, &hash, true, true, &remote).is_err());
        assert_eq!(
            store.load().unwrap().unwrap().submissions[0].state,
            "uncertain"
        );
        // Retrying a creation itself is prohibited; recovery only inspects.
        assert!(complete_with_remote(&store, &hash, true, true, &remote).is_err());
        let saved = complete_with_remote(&store, &hash, false, true, &remote).unwrap();
        assert_eq!(saved.submissions[0].state, "submitted");
        assert_eq!(remote.create_count.get(), 1);
        assert_eq!(remote.submit_count.get(), 1);
    }
    #[test]
    fn lost_event_response_reconciles_without_resubmission() {
        let (_cwd, store, hash) = saved_intent();
        let remote = FakeRemote {
            create_count: Cell::new(0),
            submit_count: Cell::new(0),
            discard_count: Cell::new(0),
            response: RefCell::new(None),
            reject: false,
            lose_create_response: false,
            lose_submit_response: true,
            lose_discard_response: false,
            leave_pending_on_loss: false,
            head_changed: Cell::new(false),
            store: store.clone(),
        };
        assert!(complete_with_remote(&store, &hash, true, true, &remote).is_err());
        let saved = complete_with_remote(&store, &hash, false, true, &remote).unwrap();
        assert_eq!(saved.submissions[0].state, "submitted");
        assert_eq!(remote.create_count.get(), 1);
        assert_eq!(remote.submit_count.get(), 1);
    }
    #[test]
    fn definite_creation_rejection_is_terminal_and_inspectable() {
        let (_cwd, store, hash) = saved_intent();
        let remote = FakeRemote {
            create_count: Cell::new(0),
            submit_count: Cell::new(0),
            discard_count: Cell::new(0),
            response: RefCell::new(None),
            reject: true,
            lose_create_response: false,
            lose_submit_response: false,
            lose_discard_response: false,
            leave_pending_on_loss: false,
            head_changed: Cell::new(false),
            store: store.clone(),
        };
        assert!(complete_with_remote(&store, &hash, true, true, &remote).is_err());
        let saved = store.load().unwrap().unwrap();
        assert_eq!(saved.submissions[0].state, "rejected");
        assert!(saved.submissions[0].error.as_ref().unwrap().contains("422"));
        assert!(complete_with_remote(&store, &hash, false, true, &remote).is_err());
        assert_eq!(
            store.load().unwrap().unwrap().submissions[0].state,
            "rejected"
        );
        assert_eq!(remote.submit_count.get(), 0);
    }
    fn review() -> Review {
        let (hunks, _) = crate::review::model::diff_text("before\n", "after\n");
        let files = vec![super::super::File {
            path: "a.rs".into(),
            old_path: None,
            status: "Modified".into(),
            additions: 1,
            deletions: 1,
            old: Some("before\n".into()),
            new: Some("after\n".into()),
            lines: hunks
                .into_iter()
                .flat_map(|h| h.lines)
                .map(|l| super::super::SourceLine {
                    old: l.old_no,
                    new: l.new_no,
                    tag: if l.new_no.is_some() {
                        "addition"
                    } else {
                        "deletion"
                    }
                    .into(),
                    text: l.text,
                })
                .collect(),
            unavailable: None,
            truncated: false,
        }];
        let capture = Capture::from_files("PR", &"b".repeat(40), &"a".repeat(40), None, files)
            .unwrap()
            .with_pr(Metadata {
                url: "https://github.com/owner/repo/pull/1".into(),
                repository: "owner/repo".into(),
                number: 1,
                title: "Retry".into(),
                description: "data".into(),
                author: "author".into(),
                target_tip: "c".repeat(40),
                base_branch: "main".into(),
                head_branch: "fix".into(),
                captured_at: 0,
                checks: vec![],
                checks_error: None,
            })
            .unwrap();
        Review::start(capture)
    }
    #[test]
    fn exact_preview_binds_event_version_ranges_and_revision() {
        let mut r = review();
        let e = r
            .active()
            .capture
            .evidence
            .iter()
            .find(|e| e.side == super::super::Side::New)
            .unwrap()
            .id
            .clone();
        r.findings.push(super::super::Finding {
            id: "f".into(),
            capture: r.active().capture.id.clone(),
            chapter: None,
            evidence: Some(e),
            range: Some(super::super::SourceRange { start: 1, end: 1 }),
            body: "Concern".into(),
            resolved: false,
        });
        let a = preview(&r, "COMMENT", "Review").unwrap();
        assert_eq!(a.payload["comments"].as_array().unwrap().len(), 1);
        assert_eq!(a.payload["comments"][0]["line"], 1);
        assert_ne!(a.hash, preview(&r, "APPROVE", "Review").unwrap().hash);
        r.version += 1;
        assert_ne!(a.hash, preview(&r, "COMMENT", "Review").unwrap().hash);
        r.findings[0].range = None;
        assert!(
            preview(&r, "COMMENT", "Review").unwrap().payload["body"]
                .as_str()
                .unwrap()
                .contains("Concern")
        );
        r.findings[0].capture = "old".into();
        assert!(
            !preview(&r, "COMMENT", "Review").unwrap().payload["body"]
                .as_str()
                .unwrap()
                .contains("Concern")
        );
        assert!(preview(&r, "INVALID", "Review").is_err());
    }
    #[test]
    fn reconciliation_requires_exact_comment_multiset() {
        let mut p = preview(&review(), "COMMENT", "").unwrap();
        p.payload["comments"] = json!([{"path":"a.rs","line":1,"side":"RIGHT","body":"Concern"}]);
        let mut remote = p.payload["comments"][0].clone();
        remote["commit_id"] = p.payload["commit_id"].clone();
        assert!(comments_match(&p, &[remote.clone()]).unwrap());
        remote["body"] = json!("Edited");
        assert!(!comments_match(&p, &[remote]).unwrap());
        assert!(!comments_match(&p, &[]).unwrap());
        let mut s = Submission {
            preview: p.clone(),
            remote_id: None,
            remote_author: None,
            event_sent: false,
            remote_url: None,
            state: "creating".into(),
            error: None,
        };
        let response = json!({"id":1,"user":{"id":7},"body":p.payload["body"],"commit_id":p.payload["commit_id"],"state":"APPROVED"});
        assert!(record_remote(&mut s, &response).is_err());
    }
}
