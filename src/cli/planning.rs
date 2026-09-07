//! Parser-only planning arguments. Domain validation remains in shared stores.
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RepositoryArgs {
    /// Emit the versioned JSON response on stdout; diagnostics remain on stderr.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: RepositoryCommand,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum RepositoryCommand {
    /// List readable portable keys, including repositories without checkouts.
    List(LimitArgs),
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct TaskArgs {
    /// Emit the versioned JSON response on stdout; diagnostics remain on stderr.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: TaskCommand,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum TaskCommand {
    /// List tasks, optionally filtered by derived repository membership.
    List(TaskListArgs),
    /// Read the whole task, including archived tasks and all subtasks.
    Get(IdArgs),
    /// Persist an idea; repositories, descriptions, and artifacts are optional.
    Create(TaskCreateArgs),
    /// Patch fields; supplied lists replace existing lists, omitted fields stay.
    Update(TaskUpdateArgs),
    /// Hide a task from default lists without changing subtasks or artifacts.
    Archive(RevisionArgs),
    /// Restore a task to default lists.
    Unarchive(RevisionArgs),
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct SubtaskArgs {
    /// Emit the versioned JSON response on stdout; diagnostics remain on stderr.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: SubtaskCommand,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum SubtaskCommand {
    /// Append a todo subtask; return its generated ID and the updated task.
    Create(SubtaskCreateArgs),
    /// Patch one subtask using the containing task's revision token.
    Update(SubtaskUpdateArgs),
    /// Remove a subtask only after incoming dependencies have been removed.
    Remove(SubtaskIdentityArgs),
    /// Supply a complete permutation of stable subtask IDs (empty for no subtasks).
    Reorder(SubtaskReorderArgs),
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct ArtifactArgs {
    /// Emit the versioned JSON response on stdout; diagnostics remain on stderr.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: ArtifactCommand,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum ArtifactCommand {
    /// List standalone artifacts; archived records are excluded by default.
    List(ListArgs),
    /// Read current metadata and Markdown, including archived artifacts.
    Get(IdArgs),
    /// Create a standalone RFC, plan, or note from UTF-8 Markdown.
    Create(ArtifactCreateArgs),
    /// Patch metadata and/or Markdown with one revision-checked mutation.
    Update(ArtifactUpdateArgs),
    /// Hide an artifact from default lists without breaking existing links.
    Archive(RevisionArgs),
    /// Restore an artifact to default lists.
    Unarchive(RevisionArgs),
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct LimitArgs {
    /// Maximum returned records (1–1000); the response reports truncation.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=1000))]
    pub(crate) limit: u16,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct ListArgs {
    #[command(flatten)]
    pub(crate) bounds: LimitArgs,
    /// Include archived records as well as active ones.
    #[arg(long)]
    pub(crate) include_archived: bool,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct TaskListArgs {
    #[command(flatten)]
    pub(crate) list: ListArgs,
    /// Filter by a portable key in task or subtask repository membership.
    #[arg(long, value_name = "KEY")]
    pub(crate) repository: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct IdArgs {
    #[arg(value_name = "ID")]
    pub(crate) id: String,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RevisionArgs {
    #[arg(value_name = "ID")]
    pub(crate) id: String,
    /// Opaque token from the most recent read of this record.
    #[arg(long, value_name = "TOKEN")]
    pub(crate) revision: String,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct DescriptionInput {
    /// UTF-8 Markdown file, or - for stdin (maximum 4 MiB).
    #[arg(long, value_name = "PATH|-")]
    pub(crate) description_file: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct DescriptionPatch {
    #[command(flatten)]
    pub(crate) input: DescriptionInput,
    /// Explicitly replace the description with an empty string.
    #[arg(long, conflicts_with = "description_file")]
    pub(crate) clear_description: bool,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct ArtifactLinks {
    /// Artifact IDs; repeat the flag or use commas. Updates replace the full list.
    #[arg(long = "artifact", value_name = "ART_ID", value_delimiter = ',')]
    pub(crate) artifacts: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct ArtifactLinksPatch {
    #[command(flatten)]
    pub(crate) links: ArtifactLinks,
    /// Explicitly remove all artifact links from this task or subtask.
    #[arg(long, conflicts_with = "artifacts")]
    pub(crate) clear_artifacts: bool,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RepositoriesInput {
    /// Portable keys; repeat the flag or use commas. Updates replace the full list.
    #[arg(long = "repository", value_name = "KEY", value_delimiter = ',')]
    pub(crate) repositories: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct TaskCreateArgs {
    #[arg(long)]
    pub(crate) title: String,
    #[command(flatten)]
    pub(crate) description: DescriptionInput,
    #[command(flatten)]
    pub(crate) repositories: RepositoriesInput,
    #[command(flatten)]
    pub(crate) artifacts: ArtifactLinks,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct TaskUpdateArgs {
    #[command(flatten)]
    pub(crate) record: RevisionArgs,
    #[arg(long)]
    pub(crate) title: Option<String>,
    #[command(flatten)]
    pub(crate) description: DescriptionPatch,
    #[command(flatten)]
    pub(crate) repositories: RepositoriesInput,
    /// Explicitly remove all explicit task repository associations.
    #[arg(long, conflicts_with = "repositories")]
    pub(crate) clear_repositories: bool,
    #[command(flatten)]
    pub(crate) artifacts: ArtifactLinksPatch,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct DependenciesInput {
    /// Same-task dependencies; repeat the flag or use commas. Updates replace the list.
    #[arg(long = "depends-on", value_name = "SUBTASK_ID", value_delimiter = ',')]
    pub(crate) dependencies: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct TaskRevisionArgs {
    #[arg(value_name = "TASK_ID")]
    pub(crate) task_id: String,
    /// Opaque revision of the containing task, not an artifact or subtask token.
    #[arg(long, value_name = "TOKEN")]
    pub(crate) revision: String,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct SubtaskCreateArgs {
    #[command(flatten)]
    pub(crate) task: TaskRevisionArgs,
    #[arg(long)]
    pub(crate) title: String,
    #[arg(long, value_name = "KEY")]
    pub(crate) repository: String,
    #[command(flatten)]
    pub(crate) description: DescriptionInput,
    #[command(flatten)]
    pub(crate) dependencies: DependenciesInput,
    #[command(flatten)]
    pub(crate) artifacts: ArtifactLinks,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct SubtaskIdentityArgs {
    #[command(flatten)]
    pub(crate) task: TaskRevisionArgs,
    #[arg(value_name = "SUBTASK_ID")]
    pub(crate) subtask_id: String,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct SubtaskUpdateArgs {
    #[command(flatten)]
    pub(crate) record: SubtaskIdentityArgs,
    #[arg(long)]
    pub(crate) title: Option<String>,
    #[arg(long, value_name = "KEY")]
    pub(crate) repository: Option<String>,
    #[command(flatten)]
    pub(crate) description: DescriptionPatch,
    #[command(flatten)]
    pub(crate) dependencies: DependenciesInput,
    #[arg(long, conflicts_with = "dependencies")]
    pub(crate) clear_dependencies: bool,
    #[command(flatten)]
    pub(crate) artifacts: ArtifactLinksPatch,
    /// Explicit status; unfinished dependencies do not block a status change.
    #[arg(long, value_enum)]
    pub(crate) status: Option<StatusArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum StatusArg {
    Todo,
    Doing,
    Blocked,
    Done,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct SubtaskReorderArgs {
    #[command(flatten)]
    pub(crate) task: TaskRevisionArgs,
    /// Every existing subtask exactly once, in the desired order.
    #[arg(value_name = "SUBTASK_ID")]
    pub(crate) order: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum KindArg {
    Rfc,
    Plan,
    Note,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct ArtifactCreateArgs {
    #[arg(long)]
    pub(crate) title: String,
    #[arg(long, value_enum)]
    pub(crate) kind: KindArg,
    /// UTF-8 Markdown file, or - for stdin (maximum 4 MiB; empty input is valid).
    #[arg(long, value_name = "PATH|-")]
    pub(crate) content_file: PathBuf,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct ArtifactUpdateArgs {
    #[command(flatten)]
    pub(crate) record: RevisionArgs,
    #[arg(long)]
    pub(crate) title: Option<String>,
    #[arg(long, value_enum)]
    pub(crate) kind: Option<KindArg>,
    /// UTF-8 Markdown file, or - for stdin (maximum 4 MiB).
    #[arg(long, value_name = "PATH|-")]
    pub(crate) content_file: Option<PathBuf>,
    /// Explicitly replace Markdown content with an empty string.
    #[arg(long, conflicts_with = "content_file")]
    pub(crate) clear_content: bool,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    #[test]
    fn command_tree_is_consistent() {
        crate::cli::Cli::command().debug_assert();
    }
}
