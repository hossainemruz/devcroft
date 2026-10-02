use anyhow::{Context as _, Result};
use serde_json::json;

use crate::cli::resources::{ArtifactArgs, ArtifactCommand, KindArg};
use crate::data::artifacts::{self, ArtifactPatch, ArtifactStore, NewArtifact};

use super::{input, output};

fn kind(value: KindArg) -> artifacts::Kind {
    match value {
        KindArg::Rfc => artifacts::Kind::Rfc,
        KindArg::Plan => artifacts::Kind::Plan,
        KindArg::Note => artifacts::Kind::Note,
        KindArg::Review => artifacts::Kind::Review,
        KindArg::Tutorial => artifacts::Kind::Tutorial,
    }
}

pub(crate) fn artifact(args: ArtifactArgs) -> Result<()> {
    let store = ArtifactStore::new(&crate::data::resolve_data_root()?);
    let snapshot = match args.command {
        ArtifactCommand::List(options) => {
            let list = store
                .list(&artifacts::ListOptions {
                    repository: options.repository,
                    space: options.space,
                    include_archived: options.include_archived,
                    limit: Some(options.bounds.limit.into()),
                })
                .context("listing artifacts")?;
            let mut human = String::new();
            for snapshot in &list.artifacts {
                human.push_str(&output::artifact_summary(snapshot));
            }
            output::list_footer(&mut human, list.artifacts.len(), list.truncated);
            return output::emit(
                args.json,
                json!({"formatVersion": 1, "artifacts": list.artifacts, "errors": list.errors, "truncated": list.truncated}),
                &human,
                &[],
                &list.errors,
            );
        }
        ArtifactCommand::Comment(options) => {
            use crate::cli::resources::CommentCommand::*;
            use artifacts::CommentChange as Change;
            let (record, change) = match options.command {
                List(options) => {
                    let record = options.record;
                    let snapshot = store.get(&record.id)?;
                    let comments = snapshot
                        .artifact
                        .comments
                        .into_iter()
                        .filter(|comment| {
                            (!options.open || !comment.resolved)
                                && (!options.resolved || comment.resolved)
                        })
                        .collect::<Vec<_>>();
                    return output::emit(
                        args.json,
                        json!({"formatVersion": 1, "artifactId": record.id, "revision": snapshot.revision, "comments": comments}),
                        &serde_json::to_string_pretty(&comments)?,
                        &[],
                        &[],
                    );
                }
                Create(input) => (input.record, Change::Create(input.body)),
                Edit(input) => (
                    input.comment.record,
                    Change::Edit(input.comment.comment_id, input.body),
                ),
                Resolve(input) => (input.record, Change::Resolve(input.comment_id, true)),
                Reopen(input) => (input.record, Change::Resolve(input.comment_id, false)),
                Delete(input) => (input.record, Change::Delete(input.comment_id)),
            };
            store.comment(&record.id, &record.revision, change)?
        }
        ArtifactCommand::Get(record) => store.get(&record.id).context("getting artifact")?,
        ArtifactCommand::Create(input_args) => store
            .create(NewArtifact {
                repository: Some(input_args.repository),
                sessions: sessions(input_args.sessions_file)?.unwrap_or_default(),
                title: input_args.title,
                kind: kind(input_args.kind),
                content: input::content(&input_args.content_file)?,
            })
            .context("creating artifact")?,
        ArtifactCommand::Update(patch) => store
            .update(
                &patch.record.id,
                &patch.record.revision,
                ArtifactPatch {
                    repository: patch.repository,
                    sessions: sessions(patch.sessions_file)?,
                    title: patch.title,
                    kind: patch.kind.map(kind),
                    content: input::text_patch(patch.content_file.as_deref(), patch.clear_content)?,
                },
            )
            .context("updating artifact")?,
        ArtifactCommand::Archive(record) => store
            .set_archived(&record.id, &record.revision, true)
            .context("archiving artifact")?,
        ArtifactCommand::Unarchive(record) => store
            .set_archived(&record.id, &record.revision, false)
            .context("unarchiving artifact")?,
        ArtifactCommand::Delete(record) => {
            store
                .delete(&record.id, &record.revision)
                .context("deleting artifact")?;
            return output::emit(
                args.json,
                json!({"formatVersion": 1, "artifactId": record.id, "deleted": true}),
                &format!("Deleted {}\n", record.id),
                &[],
                &[],
            );
        }
    };
    output::artifact_snapshot(snapshot, args.json)
}

fn sessions(path: Option<std::path::PathBuf>) -> Result<Option<Vec<artifacts::OriginSession>>> {
    path.map(|path| Ok(serde_json::from_str(&input::content(&path)?)?))
        .transpose()
}
