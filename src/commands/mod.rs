//! Headless resource handlers: translate CLI inputs, call shared stores, render.
mod artifacts;
mod input;
mod output;
mod relationships;

use anyhow::Result;
use serde_json::json;

use crate::cli::resources::{RepositoryArgs, RepositoryCommand};

pub(crate) use artifacts::artifact;

pub(crate) fn repository(args: RepositoryArgs) -> Result<()> {
    let root = crate::data::resolve_data_root()?;
    match args.command {
        RepositoryCommand::Relationships(query) => relationships::query(&root, query, args.json),
        RepositoryCommand::Relationship(mutation) => {
            relationships::mutate(&root, mutation.command, args.json)
        }
        RepositoryCommand::List(bounds) => {
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
        RepositoryCommand::Get(key) => {
            let metadata = crate::data::get_repository_metadata(&root, &key.key)?;
            let binding = crate::data::DeviceStore::new(&root)
                .load()
                .unwrap_or_default()
                .repositories
                .unwrap_or_default()
                .remove(&key.key);
            let human = format!(
                "{}\t{}\n",
                key.key,
                metadata.display_name.as_deref().unwrap_or(&key.key)
            );
            output::emit(
                args.json,
                json!({"formatVersion": 1, "key": key.key, "revision": metadata.revision, "metadata": metadata, "binding": binding}),
                &human,
                &[],
                &[],
            )
        }
        RepositoryCommand::Link(link) => {
            let linked = crate::data::link_repository(&root, &link.key, &link.checkout)?;
            let human = format!("{}\t{}\n", linked.key, linked.checkout_path.display());
            output::emit(
                args.json,
                json!({"formatVersion": 1, "key": linked.key, "checkoutPath": linked.checkout_path}),
                &human,
                &[],
                &[],
            )
        }
        RepositoryCommand::Unlink(key) => {
            let removed = crate::data::unlink_repository(&root, &key.key)?;
            let human = if removed {
                format!("unlinked {}\n", key.key)
            } else {
                format!("{} had no checkout binding\n", key.key)
            };
            output::emit(
                args.json,
                json!({"formatVersion": 1, "key": key.key, "unlinked": removed}),
                &human,
                &[],
                &[],
            )
        }
        RepositoryCommand::Update(update) => {
            // Patch semantics: absent flags keep stored values, an explicit
            // (even empty) flag overwrites — empty clears the field.
            let stored = crate::data::get_repository_metadata(&root, &update.key)?;
            let tags = match update.tags.as_deref() {
                None => stored.tags.unwrap_or_default(),
                Some(raw) => raw.split(',').map(str::to_owned).collect(),
            };
            let input = crate::data::NewRepositoryInput {
                display_name: update.display_name.or(stored.display_name),
                owner: update.owner.or(stored.owner),
                name: update.name.or(stored.name),
                description: update.description.or(stored.description),
                space: update.space.or(stored.space),
                tags,
                clone_url: update.clone_url.or(stored.clone_url),
                base_branch: update.base_branch.or(stored.base_branch),
            };
            let metadata = crate::data::update_repository_metadata(
                &root,
                &update.key,
                &input,
                &update.revision,
            )?;
            let human = format!(
                "{}\t{}\n",
                update.key,
                metadata.display_name.as_deref().unwrap_or(&update.key)
            );
            output::emit(
                args.json,
                json!({"formatVersion": 1, "key": update.key, "revision": metadata.revision, "metadata": metadata}),
                &human,
                &[],
                &[],
            )
        }
        RepositoryCommand::Remove(key) => {
            crate::data::remove_repository(&root, &key.key)?;
            let human = format!("removed {}\n", key.key);
            output::emit(
                args.json,
                json!({"formatVersion": 1, "key": key.key, "removed": true}),
                &human,
                &[],
                &[],
            )
        }
    }
}

pub(crate) fn sessions(args: RepositoryArgs) -> Result<()> {
    let root = crate::data::resolve_data_root()?;
    let catalog = crate::agent_sessions::Catalog::new(Some(&root));
    catalog.load_cache();
    catalog.refresh();
    let mut snapshot = catalog.snapshot();
    let RepositoryCommand::List(bounds) = args.command else {
        anyhow::bail!("unsupported session subcommand")
    };
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
