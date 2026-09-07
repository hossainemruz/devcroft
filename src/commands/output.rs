use std::io::{self, Write as _};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::data::{artifacts, tasks};

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

pub(super) fn task_value(snapshot: &tasks::Snapshot) -> Value {
    json!({
        "task": snapshot.task,
        "revision": snapshot.revision,
        "progress": snapshot.task.progress(),
        "involvedRepositories": snapshot.task.involved_repositories(),
        "warnings": snapshot.warnings,
    })
}

pub(super) fn task_snapshot(
    snapshot: tasks::Snapshot,
    json_output: bool,
    subtask_id: Option<String>,
) -> Result<()> {
    let mut value = task_value(&snapshot);
    value["formatVersion"] = json!(1);
    let mut human = task_detail(&snapshot);
    if let Some(id) = subtask_id {
        human.push_str(&format!("Created subtask: {id}\n"));
        value["subtaskId"] = json!(id);
    }
    emit(json_output, value, &human, &snapshot.warnings, &[])
}

pub(super) fn progress(task: &tasks::Task) -> String {
    let progress = task.progress();
    if progress.is_planned() {
        format!("{}/{} done", progress.completed, progress.total)
    } else {
        "Not planned".into()
    }
}

pub(super) fn task_summary(snapshot: &tasks::Snapshot) -> String {
    let task = &snapshot.task;
    format!(
        "{}\t{}\t{}{}\tRepositories: {}\n  Revision: {}\n",
        task.id,
        task.title,
        progress(task),
        if task.archived { " (archived)" } else { "" },
        task.involved_repositories()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", "),
        snapshot.revision
    )
}

fn task_detail(snapshot: &tasks::Snapshot) -> String {
    let task = &snapshot.task;
    let mut text = task_summary(snapshot);
    text.push_str(&format!(
        "Created: {}\nUpdated: {}\nExplicit repositories: {}\nArtifacts: {}\n\n{}\n",
        task.created_at,
        task.updated_at,
        task.repositories.join(", "),
        task.artifacts.join(", "),
        task.description
    ));
    for subtask in &task.subtasks {
        text.push_str(&format!(
            "\n{} [{}] {}\n  Repository: {}\n  Dependencies: {}\n  Artifacts: {}\n{}\n",
            subtask.id,
            status(subtask.status),
            subtask.title,
            subtask.repository,
            subtask.dependencies.join(", "),
            subtask.artifacts.join(", "),
            subtask.description
        ));
    }
    text
}

fn status(value: tasks::Status) -> &'static str {
    match value {
        tasks::Status::Todo => "todo",
        tasks::Status::Doing => "doing",
        tasks::Status::Blocked => "blocked",
        tasks::Status::Done => "done",
    }
}

pub(super) fn kind(value: artifacts::Kind) -> &'static str {
    match value {
        artifacts::Kind::Rfc => "rfc",
        artifacts::Kind::Plan => "plan",
        artifacts::Kind::Note => "note",
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
