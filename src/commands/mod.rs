//! Headless planning handlers: translate CLI inputs, call shared stores, render.
mod artifacts;
mod input;
mod output;
mod tasks;

use anyhow::Result;
use serde_json::json;

use crate::cli::planning::{RepositoryArgs, RepositoryCommand};

pub(crate) use artifacts::artifact;
pub(crate) use tasks::{subtask, task};

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
