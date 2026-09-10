use super::{
    model::{SessionKey, SessionSummary, canonical, title},
    process::{self, Rpc},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};

#[derive(Clone)]
pub(super) struct Source {
    pub provider: &'static str,
    pub root: PathBuf,
}
pub(super) fn sources() -> Vec<Source> {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    vec![
        Source {
            provider: "opencode",
            root: env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share")),
        },
        Source {
            provider: "codex",
            root: env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
        },
        Source {
            provider: "claude",
            root: env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
        },
    ]
    .into_iter()
    .map(|mut s| {
        s.root = canonical(&s.root);
        s
    })
    .collect()
}

pub(super) fn discover(
    source: &Source,
    files: &mut HashMap<PathBuf, Transcript>,
) -> Result<Vec<SessionSummary>> {
    if !source.root.is_dir() {
        return Ok(Vec::new());
    }
    match source.provider {
        "opencode" => opencode(source),
        "codex" => codex(source),
        "claude" => claude(source, files),
        _ => bail!("Unsupported session provider"),
    }
}

/// How many sessions to request from CLIs that only expose a global
/// most-recent listing. The catalog filters by checkout after fetching, so
/// this bound must comfortably exceed any single checkout's recent history.
/// Hitting it exactly would present a truncated list as complete; the bound
/// is documented alongside the other v1 limitations in
/// `docs/agent-sessions-plan.md`.
const OPENCODE_MAX_COUNT: u32 = 2000;

fn opencode(source: &Source) -> Result<Vec<SessionSummary>> {
    if !source.root.join("opencode").is_dir() {
        return Ok(Vec::new());
    }
    let data = process::output(
        Command::new("opencode")
            .args([
                "session",
                "list",
                "--format",
                "json",
                "--max-count",
                &OPENCODE_MAX_COUNT.to_string(),
            ])
            .env("XDG_DATA_HOME", &source.root)
            // Launcher shims (e.g. mise) may print a notice to stdout ahead
            // of the JSON payload; request quiet and strip it defensively.
            .env("MISE_QUIET", "1"),
    )?;
    parse_opencode(source, json_from_stdout(&data)?)
}

/// Parse CLI JSON that may be preceded by launcher-shim notices on stdout.
/// Finds the first `[` or `{` and parses from there so a preamble line
/// cannot fail the whole provider scan.
fn json_from_stdout(data: &[u8]) -> Result<Value> {
    let start = data
        .iter()
        .position(|byte| *byte == b'[' || *byte == b'{')
        .context("Unsupported OpenCode session listing")?;
    serde_json::from_slice(&data[start..]).context("Unsupported OpenCode session listing")
}
fn parse_opencode(source: &Source, data: Value) -> Result<Vec<SessionSummary>> {
    let rows = data
        .as_array()
        .context("Unsupported OpenCode session listing")?;
    rows.iter()
        .filter(|r| {
            r.get("parentID").and_then(Value::as_str).is_none()
                && r.pointer("/time/archived").is_none()
        })
        .map(|r| {
            Ok(SessionSummary {
                key: SessionKey {
                    provider: "opencode".into(),
                    store: source.root.clone(),
                    id: r["id"]
                        .as_str()
                        .context("Missing OpenCode session ID")?
                        .into(),
                },
                checkout: PathBuf::new(),
                cwd: PathBuf::from(
                    r["directory"]
                        .as_str()
                        .context("Missing OpenCode session directory")?,
                ),
                title: title(r["title"].as_str().unwrap_or("")),
                updated: r["updated"]
                    .as_i64()
                    .context("Missing OpenCode update time")?
                    / 1000,
                timestamp_source: "provider_updated".into(),
            })
        })
        .collect()
}

fn codex(source: &Source) -> Result<Vec<SessionSummary>> {
    if !source.root.join("sessions").is_dir() {
        return Ok(Vec::new());
    }
    let mut rpc = Rpc::new(
        Command::new("codex")
            .arg("app-server")
            .env("CODEX_HOME", &source.root)
            .env("MISE_QUIET", "1"),
    )?;
    let mut sessions = Vec::new();
    let mut cursor = Value::Null;
    let mut cursors = HashSet::new();
    loop {
        let result=rpc.call("thread/list",json!({"limit":100,"sortKey":"updated_at","archived":false,"sourceKinds":["cli","vscode","appServer","unknown"],"cursor":cursor}))?;
        for row in result["data"]
            .as_array()
            .context("Unsupported Codex thread listing")?
        {
            if row["ephemeral"].as_bool() == Some(true) || row["parentThreadId"].as_str().is_some()
            {
                continue;
            }
            sessions.push(SessionSummary {
                key: SessionKey {
                    provider: "codex".into(),
                    store: source.root.clone(),
                    id: row["id"]
                        .as_str()
                        .context("Missing Codex thread ID")?
                        .into(),
                },
                checkout: PathBuf::new(),
                cwd: PathBuf::from(
                    row["cwd"]
                        .as_str()
                        .context("Missing Codex working directory")?,
                ),
                title: title(
                    row["name"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .or_else(|| row["preview"].as_str())
                        .unwrap_or(""),
                ),
                updated: row["updatedAt"]
                    .as_i64()
                    .context("Missing Codex update time")?,
                timestamp_source: "provider_updated".into(),
            });
        }
        cursor = result["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
        if !cursors.insert(cursor.to_string()) {
            bail!("Codex repeated a pagination cursor");
        }
        if sessions.len() > 100_000 {
            bail!("Codex session scan exceeded its metadata limit");
        }
    }
    Ok(sessions)
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Transcript {
    size: u64,
    modified: u128,
    summary: Option<SessionSummary>,
}
fn claude(
    source: &Source,
    cache: &mut HashMap<PathBuf, Transcript>,
) -> Result<Vec<SessionSummary>> {
    let root = source.root.join("projects");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for project in fs::read_dir(&root)? {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(project.path())? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") || !entry.file_type()?.is_file() {
                continue;
            }
            let metadata = entry.metadata()?;
            let modified = metadata
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            seen.insert(path.clone());
            let cached = cache
                .get(&path)
                .filter(|c| c.size == metadata.len() && c.modified == modified);
            let parsed = match cached {
                Some(c) => c.clone(),
                None => Transcript {
                    size: metadata.len(),
                    modified,
                    summary: parse_claude(source, &path, modified)?,
                },
            };
            if let Some(summary) = &parsed.summary {
                result.push(summary.clone());
            }
            cache.insert(path, parsed);
        }
    }
    cache.retain(|path, _| !path.starts_with(&root) || seen.contains(path));
    Ok(result)
}
fn parse_claude(source: &Source, path: &Path, modified: u128) -> Result<Option<SessionSummary>> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut id = None;
    let mut cwd = None;
    let mut preview = None;
    let mut custom = None;
    let mut generated = None;
    let mut updated = 0;
    let mut sidechain = false;
    loop {
        let mut line = Vec::new();
        let count = (&mut reader)
            .take(4 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if count > 4 * 1024 * 1024 {
            bail!("Oversized Claude transcript record");
        }
        // An unfinished append is retried when its fingerprint changes.
        if line.last() != Some(&b'\n') {
            break;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            bail!("Malformed Claude transcript record");
        };
        sidechain |= v["isSidechain"].as_bool() == Some(true);
        if let Some(value) = v["sessionId"].as_str() {
            id = Some(value.to_owned());
        }
        if let Some(value) = v["cwd"].as_str() {
            cwd = Some(PathBuf::from(value));
        }
        if let Some(time) = v["timestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        {
            updated = updated.max(time.timestamp());
        }
        match v["type"].as_str() {
            Some("custom-title") => custom = v["customTitle"].as_str().map(title),
            Some("ai-title") => generated = v["aiTitle"].as_str().map(title),
            Some("summary") if generated.is_none() => generated = v["summary"].as_str().map(title),
            Some("user") if preview.is_none() && v["isMeta"].as_bool() != Some(true) => {
                let content = &v["message"]["content"];
                preview = content.as_str().map(title).or_else(|| {
                    content.as_array()?.iter().find_map(|part| {
                        if part["type"] == "text" {
                            part["text"].as_str().map(title)
                        } else {
                            None
                        }
                    })
                });
            }
            _ => {}
        }
    }
    let (Some(id), Some(cwd)) = (id, cwd) else {
        return Ok(None);
    };
    if sidechain || path.file_stem().and_then(|s| s.to_str()) != Some(&id) {
        return Ok(None);
    }
    let fallback = updated == 0;
    Ok(Some(SessionSummary {
        key: SessionKey {
            provider: "claude".into(),
            store: source.root.clone(),
            id,
        },
        cwd,
        checkout: PathBuf::new(),
        title: custom
            .or(generated)
            .or(preview)
            .unwrap_or_else(|| "Untitled session".into()),
        updated: if fallback {
            (modified / 1_000_000_000) as i64
        } else {
            updated
        },
        timestamp_source: if fallback {
            "transcript_mtime"
        } else {
            "conversation_timestamp"
        }
        .into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "reads installed agents' local metadata; requires local provider access"]
    fn installed_provider_metadata_smoke() {
        let mut files = HashMap::new();
        for source in sources() {
            let start = std::time::Instant::now();
            let sessions = discover(&source, &mut files)
                .unwrap_or_else(|error| panic!("{}: {error:#}", source.provider));
            assert!(
                sessions
                    .iter()
                    .all(|s| !s.key.id.is_empty() && s.cwd.is_absolute())
            );
            eprintln!(
                "{}: {} sessions in {:?}",
                source.provider,
                sessions.len(),
                start.elapsed()
            );
        }
    }
    #[test]
    fn opencode_normalizes_milliseconds_and_rejects_unknown_schema() {
        let source = Source {
            provider: "opencode",
            root: "/data".into(),
        };
        let rows=parse_opencode(&source,json!([{"id":"s","directory":"/repo","title":" fix\n bug ","updated":1700000000123_i64}])).unwrap();
        assert_eq!(rows[0].updated, 1700000000);
        assert_eq!(rows[0].title, "fix bug");
        assert!(parse_opencode(&source, json!([{"id":"s"}])).is_err());
    }
    #[test]
    fn opencode_ignores_launcher_preamble_before_json() {
        let source = Source {
            provider: "opencode",
            root: "/data".into(),
        };
        let mut bytes = b"mise ~/.config/mise/config.toml tools: opencode@1.18.30\n".to_vec();
        bytes.extend_from_slice(
            br#"[{"id":"s","directory":"/repo","title":"hi","updated":1700000000123}]"#,
        );
        let rows = parse_opencode(&source, json_from_stdout(&bytes).unwrap()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].updated, 1700000000);
        assert!(json_from_stdout(b"no json here").is_err());
    }
    #[test]
    fn claude_reads_titles_and_excludes_children_and_partial_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let source = Source {
            provider: "claude",
            root: dir.path().into(),
        };
        fs::write(&path,"{\"type\":\"user\",\"sessionId\":\"s\",\"cwd\":\"/repo\",\"timestamp\":\"2026-01-01T12:00:00.123Z\",\"message\":{\"content\":\"initial\"}}\n{\"type\":\"custom-title\",\"customTitle\":\"renamed\"}\n{\"partial\":").unwrap();
        let session = parse_claude(&source, &path, 0).unwrap().unwrap();
        assert_eq!(session.title, "renamed");
        assert!(session.updated > 0);
        fs::write(
            &path,
            "{\"sessionId\":\"s\",\"cwd\":\"/repo\",\"isSidechain\":true}\n",
        )
        .unwrap();
        assert!(parse_claude(&source, &path, 0).unwrap().is_none());
    }
}
