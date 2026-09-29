use super::{
    model::{SessionKey, SessionSummary, canonical, title},
    process::Rpc,
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

fn opencode(source: &Source) -> Result<Vec<SessionSummary>> {
    let database = source.root.join("opencode/opencode.db");
    if !database.is_file() {
        return Ok(Vec::new());
    }
    // `opencode session list` only reports the single project containing the
    // caller's working directory, so a subprocess can never cover every
    // checkout at once (verified on 1.18.30: launching from $HOME hides a
    // repository's sessions, launching from the repository hides the rest).
    // The local store is read directly instead: one read-only query across
    // all projects, independent of this process's working directory.
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("opening OpenCode session store {}", database.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    // OpenCode v2 moved sessions to `session_v2` (same database file). Once
    // that table exists the installed binary is v2, so it resumes only v2
    // rows: legacy `session` rows the v2 migration skipped are unreachable
    // from the CLI, and listing them would offer resumes that silently start
    // a new conversation instead. Installations that predate v2 have no
    // `session_v2` table and are read from the legacy table as before.
    let table = if table_exists(&connection, "session_v2")? {
        "session_v2"
    } else {
        "session"
    };
    let mut statement = connection
        .prepare(&format!(
            "SELECT id, directory, title, time_updated FROM {table} WHERE parent_id IS NULL AND time_archived IS NULL"
        ))
        .context("reading OpenCode sessions")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                // `session_v2.title` is nullable while `session.title` is
                // not; read both as optional so one query covers them.
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, i64>(3)?,
            ))
        })
        .context("reading OpenCode sessions")?;
    let mut sessions = Vec::new();
    for row in rows {
        // A malformed row must not fail the whole provider scan the way a
        // malformed CLI payload would: skip it and keep the rest.
        let Ok((id, directory, row_title, updated)) = row else {
            continue;
        };
        if id.is_empty() || directory.is_empty() {
            continue;
        }
        sessions.push(SessionSummary {
            key: SessionKey {
                provider: "opencode".into(),
                store: source.root.clone(),
                id,
            },
            checkout: PathBuf::new(),
            cwd: PathBuf::from(directory),
            title: title(&row_title),
            // The store records milliseconds, matching the old CLI payload.
            updated: updated / 1000,
            timestamp_source: "provider_updated".into(),
        });
    }
    Ok(sessions)
}

/// Whether a table exists in the attached database. A missing table is a
/// normal state (a store that predates the schema that adds it), while a
/// lookup failure on an otherwise-open database is real drift.
fn table_exists(connection: &rusqlite::Connection, table: &str) -> Result<bool> {
    match connection.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        rusqlite::params![table],
        |_| Ok(true),
    ) {
        Ok(found) => Ok(found),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(error) => Err(error).context("reading OpenCode sessions"),
    }
}

fn codex(source: &Source) -> Result<Vec<SessionSummary>> {
    // The GUI process often launches without the user's shell PATH (mise shims,
    // brew, cargo), so `codex app-server` frequently fails with ENOENT even
    // though the CLI works in a terminal. The local state database carries the
    // same thread metadata, so it is the primary source; the app-server RPC
    // remains only for stores predating the sqlite layout.
    if let Some(sessions) = codex_via_sqlite(source)? {
        return Ok(sessions);
    }
    if !source.root.join("sessions").is_dir() {
        return Ok(Vec::new());
    }
    codex_via_rpc(source)
}

fn codex_state_files(source: &Source) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(&source.root) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("state_") || !name.ends_with(".sqlite") {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_file()) {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn codex_via_sqlite(source: &Source) -> Result<Option<Vec<SessionSummary>>> {
    let files = codex_state_files(source);
    if files.is_empty() {
        return Ok(None);
    }
    let mut sessions = Vec::new();
    for database in &files {
        let connection = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .with_context(|| format!("opening Codex session store {}", database.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut statement = connection
            .prepare(
                "SELECT id, cwd, COALESCE(name, ''), COALESCE(preview, ''), COALESCE(title, ''), \
                 COALESCE(first_user_message, ''), COALESCE(updated_at, 0), COALESCE(updated_at_ms, 0), \
                 COALESCE(recency_at, 0), COALESCE(archived, 0), COALESCE(source, ''), \
                 COALESCE(thread_source, '') FROM threads",
            )
            .context("reading Codex sessions")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                ))
            })
            .context("reading Codex sessions")?;
        for row in rows {
            // A malformed row must not fail the whole provider scan the way a
            // malformed RPC payload would: skip it and keep the rest.
            let Ok((
                id,
                cwd,
                name,
                preview,
                title_text,
                first,
                updated,
                updated_ms,
                recency,
                archived,
                source_kind,
                thread_source,
            )) = row
            else {
                continue;
            };
            if id.is_empty() || cwd.is_empty() || archived != 0 {
                continue;
            }
            // Subagent/guardian threads share the store but are never directly
            // resumable root conversations (mirrors the app-server
            // parentThreadId/sourceKinds filter).
            if thread_source == "subagent"
                || source_kind.trim_start().starts_with('{')
                || source_kind.contains("subagent")
            {
                continue;
            }
            let mut updated = updated.max(updated_ms / 1000).max(recency);
            // The RPC contract has returned milliseconds while the store
            // records seconds; normalize either unit to seconds so recency
            // sorting never clamps every row to now.
            if updated > 100_000_000_000 {
                updated /= 1000;
            }
            if updated <= 0 {
                continue;
            }
            let raw_title = [name, preview, title_text, first]
                .into_iter()
                .find(|s| !s.trim().is_empty())
                .unwrap_or_default();
            sessions.push(SessionSummary {
                key: SessionKey {
                    provider: "codex".into(),
                    store: source.root.clone(),
                    id,
                },
                checkout: PathBuf::new(),
                cwd: PathBuf::from(cwd),
                title: title(&raw_title),
                updated,
                timestamp_source: "provider_updated".into(),
            });
        }
    }
    Ok(Some(sessions))
}

fn codex_via_rpc(source: &Source) -> Result<Vec<SessionSummary>> {
    let mut rpc = Rpc::new(
        Command::new("codex")
            .arg("app-server")
            .env("CODEX_HOME", &source.root)
            .env("MISE_QUIET", "1"),
    )
    .map_err(|error| {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        {
            anyhow::anyhow!(
                "Codex CLI not found on PATH (tried `codex app-server`). Install Codex or launch Devcroft from a shell with mise/brew on PATH"
            )
        } else {
            error
        }
    })?;
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
                updated: {
                    let updated = row["updatedAt"]
                        .as_i64()
                        .context("Missing Codex update time")?;
                    // The app-server has returned milliseconds while older
                    // payloads record seconds; normalize to seconds so
                    // recency sorting never clamps every row to now.
                    if updated > 100_000_000_000 {
                        updated / 1000
                    } else {
                        updated
                    }
                },
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
    /// A store that predates OpenCode v2: only the legacy `session` table.
    fn opencode_fixture() -> (tempfile::TempDir, Source) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("opencode")).unwrap();
        let connection =
            rusqlite::Connection::open(dir.path().join("opencode/opencode.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL, title TEXT NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER)",
            )
            .unwrap();
        connection.close().unwrap();
        let source = Source {
            provider: "opencode",
            root: dir.path().into(),
        };
        (dir, source)
    }
    /// A store the v2 binary has touched: both table families exist and v2
    /// rows are the resumable ones.
    fn opencode_v2_fixture() -> (tempfile::TempDir, Source) {
        let (dir, source) = opencode_fixture();
        let connection =
            rusqlite::Connection::open(dir.path().join("opencode/opencode.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL, title TEXT, time_updated INTEGER NOT NULL, time_archived INTEGER)",
            )
            .unwrap();
        connection.close().unwrap();
        (dir, source)
    }
    /// Insert one row into the given OpenCode session table.
    #[allow(clippy::too_many_arguments)]
    fn insert_opencode_row(
        dir: &tempfile::TempDir,
        table: &str,
        id: &str,
        parent: Option<&str>,
        directory: &str,
        title: Option<&str>,
        updated_ms: i64,
        archived: Option<i64>,
    ) {
        let connection =
            rusqlite::Connection::open(dir.path().join("opencode/opencode.db")).unwrap();
        connection
            .execute(
                &format!("INSERT INTO {table} (id, parent_id, directory, title, time_updated, time_archived) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"),
                rusqlite::params![id, parent, directory, title, updated_ms, archived],
            )
            .unwrap();
        connection.close().unwrap();
    }
    #[allow(clippy::too_many_arguments)]
    fn insert_opencode_session(
        dir: &tempfile::TempDir,
        id: &str,
        parent: Option<&str>,
        directory: &str,
        title: &str,
        updated_ms: i64,
        archived: Option<i64>,
    ) {
        insert_opencode_row(
            dir,
            "session",
            id,
            parent,
            directory,
            Some(title),
            updated_ms,
            archived,
        );
    }
    #[test]
    fn opencode_reads_every_project_normalizes_milliseconds_and_titles() {
        // The CLI only reports the project containing the caller's cwd, so
        // the store read must cover sessions from unrelated directories in
        // one pass with no working-directory involvement at all.
        let (dir, source) = opencode_fixture();
        insert_opencode_session(
            &dir,
            "s1",
            None,
            "/repo",
            " fix\n bug ",
            1700000000123,
            None,
        );
        insert_opencode_session(&dir, "s2", None, "/elsewhere", "other", 1700000001000, None);
        let mut files = HashMap::new();
        let sessions = discover(&source, &mut files).unwrap();
        assert_eq!(sessions.len(), 2);
        let first = sessions.iter().find(|s| s.key.id == "s1").unwrap();
        assert_eq!(first.updated, 1700000000);
        assert_eq!(first.title, "fix bug");
        assert_eq!(first.cwd, PathBuf::from("/repo"));
        assert_eq!(first.key.store, source.root);
    }
    #[test]
    fn opencode_excludes_children_and_archived_sessions() {
        let (dir, source) = opencode_fixture();
        insert_opencode_session(&dir, "root", None, "/repo", "keep", 1700000000000, None);
        insert_opencode_session(
            &dir,
            "child",
            Some("root"),
            "/repo",
            "child",
            1700000001000,
            None,
        );
        insert_opencode_session(
            &dir,
            "archived",
            None,
            "/repo",
            "old",
            1700000002000,
            Some(1700000003000),
        );
        let mut files = HashMap::new();
        let sessions = discover(&source, &mut files).unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.key.id.as_str())
                .collect::<Vec<_>>(),
            vec!["root"]
        );
    }
    #[test]
    fn opencode_v2_store_lists_only_resumable_v2_sessions() {
        // Once the store has `session_v2`, the installed binary is v2 and it
        // resumes only v2 rows. Legacy rows the v2 migration skipped (some
        // were written after it ran) are unreachable from the CLI, so listing
        // them would offer resumes that silently start a new conversation.
        let (dir, source) = opencode_v2_fixture();
        insert_opencode_session(
            &dir,
            "v1-only",
            None,
            "/repo",
            "unmigrated",
            1700000000000,
            None,
        );
        insert_opencode_row(
            &dir,
            "session_v2",
            "v2",
            None,
            "/repo",
            Some("current"),
            1700000001000,
            None,
        );
        insert_opencode_row(
            &dir,
            "session_v2",
            "v2-child",
            Some("v2"),
            "/repo",
            Some("child"),
            1700000002000,
            None,
        );
        insert_opencode_row(
            &dir,
            "session_v2",
            "v2-archived",
            None,
            "/repo",
            Some("old"),
            1700000003000,
            Some(1700000004000),
        );
        insert_opencode_row(
            &dir,
            "session_v2",
            "v2-untitled",
            None,
            "/repo",
            None,
            1700000005000,
            None,
        );
        let mut files = HashMap::new();
        let sessions = discover(&source, &mut files).unwrap();
        let ids: Vec<_> = sessions.iter().map(|s| s.key.id.as_str()).collect();
        assert!(!ids.contains(&"v1-only"), "v1-only row must hide: {ids:?}");
        assert!(
            !ids.contains(&"v2-child"),
            "v2 child must stay hidden: {ids:?}"
        );
        assert!(
            !ids.contains(&"v2-archived"),
            "archived v2 session must stay hidden: {ids:?}"
        );
        let live = sessions
            .iter()
            .find(|s| s.key.id == "v2")
            .expect("v2 session should list");
        assert_eq!(live.title, "current");
        let untitled = sessions
            .iter()
            .find(|s| s.key.id == "v2-untitled")
            .expect("nullable v2 title should still list");
        assert_eq!(untitled.title, "Untitled session");
    }
    #[test]
    fn opencode_legacy_store_without_v2_table_still_lists_v1_sessions() {
        // A store the v2 binary never touched has no `session_v2` table; the
        // legacy read must keep working for those installations.
        let (dir, source) = opencode_fixture();
        insert_opencode_session(&dir, "v1", None, "/repo", "legacy", 1700000000000, None);
        let mut files = HashMap::new();
        let sessions = discover(&source, &mut files).unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.key.id.as_str())
                .collect::<Vec<_>>(),
            vec!["v1"]
        );
    }
    #[test]
    fn opencode_missing_store_is_empty_but_broken_schema_is_an_error() {
        let source = Source {
            provider: "opencode",
            root: "/data".into(),
        };
        let mut files = HashMap::new();
        assert!(discover(&source, &mut files).unwrap().is_empty());

        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("opencode")).unwrap();
        fs::write(dir.path().join("opencode/opencode.db"), "not a database").unwrap();
        let source = Source {
            provider: "opencode",
            root: dir.path().into(),
        };
        // Schema drift must surface as a provider error (stale cache plus a
        // sidebar notice), never as a silently empty list.
        assert!(discover(&source, &mut files).is_err());
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
    fn codex_fixture() -> (tempfile::TempDir, Source) {
        let dir = tempfile::tempdir().unwrap();
        let connection = rusqlite::Connection::open(dir.path().join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT NOT NULL, name TEXT, preview TEXT NOT NULL DEFAULT '', title TEXT NOT NULL DEFAULT '', first_user_message TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL DEFAULT 0, updated_at_ms INTEGER NOT NULL DEFAULT 0, recency_at INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0, source TEXT NOT NULL DEFAULT '', thread_source TEXT NOT NULL DEFAULT '')",
            )
            .unwrap();
        connection.close().unwrap();
        let source = Source {
            provider: "codex",
            root: dir.path().into(),
        };
        (dir, source)
    }
    #[allow(clippy::too_many_arguments)]
    fn insert_codex_thread(
        dir: &tempfile::TempDir,
        id: &str,
        cwd: &str,
        name: Option<&str>,
        preview: &str,
        updated_secs: i64,
        archived: i64,
        source: &str,
        thread_source: &str,
    ) {
        let connection = rusqlite::Connection::open(dir.path().join("state_5.sqlite")).unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, cwd, name, preview, title, first_user_message, updated_at, updated_at_ms, recency_at, archived, source, thread_source) VALUES (?1, ?2, ?3, ?4, '', '', ?5, ?6, 0, ?7, ?8, ?9)",
                rusqlite::params![
                    id,
                    cwd,
                    name,
                    preview,
                    updated_secs,
                    updated_secs * 1000,
                    archived,
                    source,
                    thread_source
                ],
            )
            .unwrap();
        connection.close().unwrap();
    }
    #[test]
    fn codex_reads_sqlite_without_sessions_dir_or_binary() {
        // GUI launches often lack mise/brew on PATH, so `codex app-server`
        // fails with ENOENT. The sqlite store must back discovery on its own:
        // no `sessions/` directory and no binary involvement.
        let (dir, source) = codex_fixture();
        assert!(!dir.path().join("sessions").exists());
        insert_codex_thread(
            &dir,
            "root",
            "/repo",
            Some("Short name"),
            "Long preview that should lose to the name",
            1783093401,
            0,
            "vscode",
            "user",
        );
        insert_codex_thread(
            &dir,
            "archived",
            "/repo",
            Some("old"),
            "old",
            1783093402,
            1,
            "vscode",
            "user",
        );
        insert_codex_thread(
            &dir,
            "subagent",
            "/repo",
            None,
            "child work",
            1783093403,
            0,
            r#"{"subagent":{"other":"guardian"}}"#,
            "subagent",
        );
        let mut files = HashMap::new();
        let sessions = discover(&source, &mut files).unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.key.id.as_str())
                .collect::<Vec<_>>(),
            vec!["root"]
        );
        let root = &sessions[0];
        assert_eq!(root.title, "Short name");
        assert_eq!(root.updated, 1783093401);
        assert_eq!(root.cwd, PathBuf::from("/repo"));
        assert_eq!(root.key.store, source.root);
    }
    #[test]
    fn codex_sqlite_normalizes_milliseconds_and_prefers_preview_without_name() {
        let (dir, source) = codex_fixture();
        let connection = rusqlite::Connection::open(dir.path().join("state_5.sqlite")).unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, cwd, name, preview, title, first_user_message, updated_at, updated_at_ms, recency_at, archived, source, thread_source) VALUES ('ms', '/repo', NULL, 'preview title', '', '', 0, 1700000000123, 0, 0, 'cli', 'user')",
                [],
            )
            .unwrap();
        connection.close().unwrap();
        let sessions = codex_via_sqlite(&source).unwrap().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "preview title");
        assert_eq!(sessions[0].updated, 1700000000);
    }
    #[test]
    fn codex_missing_store_falls_back_but_broken_sqlite_is_an_error() {
        let source = Source {
            provider: "codex",
            root: "/data".into(),
        };
        // No sqlite files and no sessions directory: empty, never a spawn
        // error, so other providers stay usable.
        let mut files = HashMap::new();
        assert!(discover(&source, &mut files).unwrap().is_empty());
        assert!(codex_via_sqlite(&source).unwrap().is_none());

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("state_5.sqlite"), "not a database").unwrap();
        let source = Source {
            provider: "codex",
            root: dir.path().into(),
        };
        // Schema drift must surface as a provider error (stale cache plus a
        // sidebar notice), never as a silently empty list.
        assert!(codex_via_sqlite(&source).is_err());
    }
}
