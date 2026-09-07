use anyhow::{Context as _, Result};
use serde_json::json;

use crate::cli::planning::{StatusArg, SubtaskArgs, SubtaskCommand, TaskArgs, TaskCommand};
use crate::data::tasks::{self, NewSubtask, NewTask, SubtaskPatch, TaskPatch, TaskStore};

use super::{input, output};

pub(crate) fn task(args: TaskArgs) -> Result<()> {
    let store = TaskStore::new(&crate::data::resolve_data_root()?);
    let snapshot = match args.command {
        TaskCommand::List(args_list) => {
            let list = store
                .list(&tasks::ListOptions {
                    repository: args_list.repository,
                    include_archived: args_list.list.include_archived,
                    limit: Some(args_list.list.bounds.limit.into()),
                })
                .context("listing tasks")?;
            let mut human = String::new();
            let mut warnings = Vec::new();
            for snapshot in &list.tasks {
                human.push_str(&output::task_summary(snapshot));
                warnings.extend(snapshot.warnings.iter().cloned());
            }
            output::list_footer(&mut human, list.tasks.len(), list.truncated);
            return output::emit(
                args.json,
                json!({"formatVersion": 1, "tasks": list.tasks.iter().map(output::task_value).collect::<Vec<_>>(), "errors": list.errors, "truncated": list.truncated}),
                &human,
                &warnings,
                &list.errors,
            );
        }
        TaskCommand::Get(record) => store.get(&record.id).context("getting task")?,
        TaskCommand::Create(input_args) => store
            .create(NewTask {
                title: input_args.title,
                description: input::description(input_args.description)?,
                repositories: input_args.repositories.repositories,
                artifacts: input_args.artifacts.artifacts,
            })
            .context("creating task")?,
        TaskCommand::Update(patch) => store
            .update(
                &patch.record.id,
                &patch.record.revision,
                TaskPatch {
                    title: patch.title,
                    description: input::description_patch(patch.description)?,
                    repositories: input::list_patch(
                        patch.repositories.repositories,
                        patch.clear_repositories,
                    ),
                    artifacts: input::list_patch(
                        patch.artifacts.links.artifacts,
                        patch.artifacts.clear_artifacts,
                    ),
                },
            )
            .context("updating task")?,
        TaskCommand::Archive(record) => store
            .set_archived(&record.id, &record.revision, true)
            .context("archiving task")?,
        TaskCommand::Unarchive(record) => store
            .set_archived(&record.id, &record.revision, false)
            .context("unarchiving task")?,
    };
    output::task_snapshot(snapshot, args.json, None)
}

pub(crate) fn subtask(args: SubtaskArgs) -> Result<()> {
    let store = TaskStore::new(&crate::data::resolve_data_root()?);
    let mut created_id = None;
    let snapshot = match args.command {
        SubtaskCommand::Create(input_args) => {
            let snapshot = store
                .create_subtask(
                    &input_args.task.task_id,
                    &input_args.task.revision,
                    NewSubtask {
                        title: input_args.title,
                        description: input::description(input_args.description)?,
                        repository: input_args.repository,
                        dependencies: input_args.dependencies.dependencies,
                        artifacts: input_args.artifacts.artifacts,
                    },
                )
                .context("creating subtask")?;
            // The shared API appends exactly one subtask and returns that same
            // committed snapshot; do not reread or perform a second mutation.
            created_id = snapshot.task.subtasks.last().map(|s| s.id.clone());
            snapshot
        }
        SubtaskCommand::Update(patch) => store
            .update_subtask(
                &patch.record.task.task_id,
                &patch.record.task.revision,
                &patch.record.subtask_id,
                SubtaskPatch {
                    title: patch.title,
                    description: input::description_patch(patch.description)?,
                    repository: patch.repository,
                    dependencies: input::list_patch(
                        patch.dependencies.dependencies,
                        patch.clear_dependencies,
                    ),
                    artifacts: input::list_patch(
                        patch.artifacts.links.artifacts,
                        patch.artifacts.clear_artifacts,
                    ),
                    status: patch.status.map(|value| match value {
                        StatusArg::Todo => tasks::Status::Todo,
                        StatusArg::Doing => tasks::Status::Doing,
                        StatusArg::Blocked => tasks::Status::Blocked,
                        StatusArg::Done => tasks::Status::Done,
                    }),
                },
            )
            .context("updating subtask")?,
        SubtaskCommand::Remove(record) => store
            .remove_subtask(
                &record.task.task_id,
                &record.task.revision,
                &record.subtask_id,
            )
            .context("removing subtask")?,
        SubtaskCommand::Reorder(order) => store
            .reorder_subtasks(&order.task.task_id, &order.task.revision, &order.order)
            .context("reordering subtasks")?,
    };
    output::task_snapshot(snapshot, args.json, created_id)
}
