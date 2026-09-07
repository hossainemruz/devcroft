use anyhow::{Context as _, Result};
use serde_json::json;

use crate::cli::planning::{ArtifactArgs, ArtifactCommand, KindArg};
use crate::data::artifacts::{self, ArtifactPatch, ArtifactStore, NewArtifact};

use super::{input, output};

fn kind(value: KindArg) -> artifacts::Kind {
    match value {
        KindArg::Rfc => artifacts::Kind::Rfc,
        KindArg::Plan => artifacts::Kind::Plan,
        KindArg::Note => artifacts::Kind::Note,
    }
}

pub(crate) fn artifact(args: ArtifactArgs) -> Result<()> {
    let store = ArtifactStore::new(&crate::data::resolve_data_root()?);
    let snapshot = match args.command {
        ArtifactCommand::List(options) => {
            let list = store
                .list(&artifacts::ListOptions {
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
        ArtifactCommand::Get(record) => store.get(&record.id).context("getting artifact")?,
        ArtifactCommand::Create(input_args) => store
            .create(NewArtifact {
                title: input_args.title,
                kind: kind(input_args.kind),
                content: input::markdown(&input_args.content_file)?,
            })
            .context("creating artifact")?,
        ArtifactCommand::Update(patch) => store
            .update(
                &patch.record.id,
                &patch.record.revision,
                ArtifactPatch {
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
    };
    output::artifact_snapshot(snapshot, args.json)
}
