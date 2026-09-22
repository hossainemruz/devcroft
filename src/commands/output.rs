use std::io::{self, Write as _};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::data::artifacts;

pub(super) fn emit(
    json_output: bool,
    value: Value,
    human: &str,
    warnings: &[String],
    errors: &[String],
) -> Result<()> {
    let text = if json_output {
        format!("{}\n", serde_json::to_string(&value)?)
    } else {
        human.to_owned()
    };
    io::stdout().lock().write_all(text.as_bytes())?;
    for warning in warnings {
        eprintln!("devcroft: warning: {warning}");
    }
    for error in errors {
        eprintln!("devcroft: {error}");
    }
    if !errors.is_empty() {
        bail!(
            "{} record(s) could not be listed; valid records were returned on stdout",
            errors.len()
        );
    }
    Ok(())
}

pub(super) fn kind(value: artifacts::Kind) -> &'static str {
    match value {
        artifacts::Kind::Rfc => "rfc",
        artifacts::Kind::Plan => "plan",
        artifacts::Kind::Note => "note",
        artifacts::Kind::Review => "review",
    }
}

pub(super) fn artifact_summary(snapshot: &artifacts::Snapshot) -> String {
    let artifact = &snapshot.artifact;
    format!(
        "{}\t[{}] {}{}\n  Revision: {}\n",
        artifact.id,
        kind(artifact.kind),
        artifact.title,
        if artifact.archived { " (archived)" } else { "" },
        snapshot.revision
    )
}

pub(super) fn artifact_snapshot(snapshot: artifacts::Snapshot, json_output: bool) -> Result<()> {
    let mut human = artifact_summary(&snapshot);
    human.push_str(&format!(
        "Created: {}\nUpdated: {}\n\n{}\n",
        snapshot.artifact.created_at, snapshot.artifact.updated_at, snapshot.artifact.content
    ));
    emit(
        json_output,
        json!({"formatVersion": 1, "artifact": snapshot.artifact, "revision": snapshot.revision}),
        &human,
        &[],
        &[],
    )
}

pub(super) fn list_footer(human: &mut String, count: usize, truncated: bool) {
    human.push_str(&format!(
        "{count} record(s){}\n",
        if truncated {
            " (truncated; increase --limit, maximum 1000)"
        } else {
            ""
        }
    ));
}
