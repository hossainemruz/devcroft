//! Parser-only resource arguments. Domain validation remains in shared stores.
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
    /// Permanently delete an artifact and its comments.
    Delete(RevisionArgs),
    /// Read and manage artifact feedback.
    Comment(CommentArgs),
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct LimitArgs {
    /// Maximum returned records (1–1000); the response reports truncation.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=1000))]
    pub(crate) limit: u16,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct ListArgs {
    #[arg(long)]
    pub(crate) repository: Option<String>,
    #[command(flatten)]
    pub(crate) bounds: LimitArgs,
    /// Include archived records as well as active ones.
    #[arg(long)]
    pub(crate) include_archived: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum KindArg {
    Rfc,
    Plan,
    Note,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct ArtifactCreateArgs {
    /// Portable repository key.
    #[arg(long)]
    pub(crate) repository: String,
    /// JSON array of originating sessions (repository, key, title).
    #[arg(long)]
    pub(crate) sessions_file: Option<PathBuf>,
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
    #[arg(long)]
    pub(crate) repository: Option<String>,
    /// Replace originating sessions with a JSON array; [] clears the list.
    #[arg(long)]
    pub(crate) sessions_file: Option<PathBuf>,
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

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct CommentArgs {
    #[command(subcommand)]
    pub command: CommentCommand,
}
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum CommentCommand {
    List(IdArgs),
    Create(CommentCreate),
    Edit(CommentEdit),
    Resolve(CommentIdentity),
    Reopen(CommentIdentity),
    Delete(CommentIdentity),
}
#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct CommentCreate {
    #[command(flatten)]
    pub record: RevisionArgs,
    #[arg(long)]
    pub body: String,
}
#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct CommentIdentity {
    #[command(flatten)]
    pub record: RevisionArgs,
    pub comment_id: String,
}
#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct CommentEdit {
    #[command(flatten)]
    pub comment: CommentIdentity,
    #[arg(long)]
    pub body: String,
}
