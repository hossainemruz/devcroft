use anyhow::{Result, ensure};

use crate::cli::resources::{
    RelationshipCommand, RelationshipDescriptionArgs, RelationshipQueryArgs,
};
use crate::data::{
    DataRoot,
    relationships::{self, Mutation, Query},
};

pub(super) fn query(root: &DataRoot, args: RelationshipQueryArgs, json: bool) -> Result<()> {
    let graph = relationships::load(
        root,
        Query {
            repository: args.repository,
            depth: args.depth,
            group: args.group,
        },
    )?;
    let mut human = format!(
        "Provider → consumer (consumer depends on provider)\nRevision: {}\n",
        graph.revision
    );
    for node in &graph.nodes {
        human.push_str(&format!(
            "{}\t{}\t{}\n",
            node.key(),
            node.label(),
            node.repository.description.as_deref().unwrap_or("")
        ));
    }
    for edge in &graph.relationships {
        human.push_str(&format!(
            "{}\t{} → {}\t{}\n",
            edge.id, edge.from, edge.to, edge.description
        ));
    }
    if graph.query.repository.is_some() {
        human.push_str(&format!(
            "Dependencies: {}\nDependents: {}\n",
            graph.dependencies.join(", "),
            graph.dependents.join(", ")
        ));
    }
    if !graph.excluded_relationships.is_empty() {
        human.push_str(&format!(
            "{} cross-group connections excluded; query All to see their endpoints.\n",
            graph.excluded_relationships.len()
        ));
    }
    super::output::list_footer(&mut human, graph.nodes.len(), graph.truncated);
    super::output::emit(
        json,
        serde_json::to_value(&graph)?,
        &human,
        &graph.diagnostics,
        &[],
    )
}

fn description(args: RelationshipDescriptionArgs) -> Result<Option<String>> {
    let text = match args.description_file {
        Some(path) => Some(super::input::markdown(&path)?),
        None => args.description,
    };
    ensure!(
        text.as_ref().is_none_or(|s| s.len() <= 16_384),
        "description exceeds 16384 bytes"
    );
    Ok(text)
}

pub(super) fn mutate(root: &DataRoot, command: RelationshipCommand, json: bool) -> Result<()> {
    let (revision, mutation) = match command {
        RelationshipCommand::Create(args) => (
            args.revision,
            Mutation::Create {
                from: args.from,
                to: args.to,
                description: description(args.text)?.ok_or_else(|| {
                    anyhow::anyhow!("provide --description or --description-file")
                })?,
            },
        ),
        RelationshipCommand::Update(args) => (
            args.record.revision,
            Mutation::Update {
                id: args.record.id,
                from: args.from,
                to: args.to,
                description: description(args.text)?,
            },
        ),
        RelationshipCommand::Delete(args) => (args.revision, Mutation::Delete { id: args.id }),
    };
    let result = relationships::mutate(root, &revision, mutation)?;
    super::output::emit(
        json,
        serde_json::to_value(&result)?,
        &format!("{}\t{}\n", result.id, result.revision),
        &[],
        &[],
    )
}
