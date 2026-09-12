//! Headless planning handlers: translate CLI inputs, call shared stores, render.
mod artifacts;
mod input;
mod output;

use anyhow::Result;
use serde_json::json;

use crate::cli::planning::{RepositoryArgs, RepositoryCommand};

pub(crate) use artifacts::artifact;

pub(crate) fn repository(args: RepositoryArgs) -> Result<()> {
    let root = crate::data::resolve_data_root()?;
    let RepositoryCommand::List(bounds) = args.command;
    let list = crate::data::list_repositories(&root, bounds.limit.into())?;
    let mut human = String::new();
    let mut warnings = Vec::new();
    for record in &list.repositories {
        human.push_str(&format!(
            "{}\t{}\n",
            record.key,
            record.display_name.as_deref().unwrap_or(&record.key)
        ));
        warnings.extend(record.warnings.iter().cloned());
    }
    output::list_footer(&mut human, list.repositories.len(), list.truncated);
    output::emit(
        args.json,
        json!({"formatVersion": 1, "repositories": list.repositories, "errors": list.errors, "truncated": list.truncated}),
        &human,
        &warnings,
        &list.errors,
    )
}

pub(crate) fn sessions(args: RepositoryArgs) -> Result<()> {
    let root = crate::data::resolve_data_root()?;
    let catalog = crate::agent_sessions::Catalog::new(Some(&root));
    catalog.load_cache();
    catalog.refresh();
    let mut snapshot = catalog.snapshot();
    let RepositoryCommand::List(bounds) = args.command;
    let truncated = snapshot.sessions.len() > bounds.limit as usize;
    snapshot.sessions.truncate(bounds.limit as usize);
    output::emit(
        args.json,
        json!({"formatVersion": 1, "sessions": snapshot.sessions, "errors": snapshot.errors, "truncated": truncated}),
        &serde_json::to_string_pretty(&snapshot.sessions)?,
        &[],
        &snapshot.errors,
    )
}
