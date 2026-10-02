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
    /// Query provider → consumer relationships, including unlinked repositories.
    Relationships(RelationshipQueryArgs),
    /// Create, edit, or delete a relationship with a fresh graph revision.
    Relationship(RelationshipArgs),
    /// List readable portable keys, including repositories without checkouts.
    List(LimitArgs),
    /// Show one portable record plus this device's checkout binding.
    Get(RepositoryKeyArgs),
    /// Bind an existing portable record to a local git checkout.
    Link(RepositoryLinkArgs),
    /// Remove this device's checkout binding, keeping the portable record.
    Unlink(RepositoryKeyArgs),
    /// Patch portable metadata; absent flags keep their stored values,
    /// an empty string clears the field.
    Update(RepositoryUpdateArgs),
    /// Delete the portable record and this device's binding. The local
    /// checkout itself is left on disk.
    Remove(RepositoryKeyArgs),
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RepositoryKeyArgs {
    /// Portable repository key.
    #[arg(value_name = "KEY")]
    pub(crate) key: String,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RepositoryLinkArgs {
    /// Portable repository key.
    #[arg(long)]
    pub(crate) key: String,
    /// Local git checkout to bind.
    #[arg(long, value_name = "PATH")]
    pub(crate) checkout: PathBuf,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RepositoryUpdateArgs {
    /// Portable repository key.
    #[arg(value_name = "KEY")]
    pub(crate) key: String,
    /// Repository metadata revision from get or relationships.
    #[arg(long)]
    pub(crate) revision: String,
    #[arg(long)]
    pub(crate) display_name: Option<String>,
    #[arg(long)]
    pub(crate) owner: Option<String>,
    #[arg(long)]
    pub(crate) name: Option<String>,
    #[arg(long)]
    pub(crate) description: Option<String>,
    /// Isolation profile; an unknown name is added to the portable catalog.
    #[arg(long)]
    pub(crate) space: Option<String>,
    /// Comma-separated tags; empty string clears. Absent keeps stored tags.
    #[arg(long)]
    pub(crate) tags: Option<String>,
    #[arg(long)]
    pub(crate) clone_url: Option<String>,
    #[arg(long)]
    pub(crate) base_branch: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RelationshipQueryArgs {
    pub(crate) repository: Option<String>,
    #[arg(long, requires = "repository", value_parser = clap::value_parser!(u8).range(1..=8))]
    pub(crate) depth: Option<u8>,
    /// Isolation profile filter; absent shows every space.
    #[arg(long)]
    pub(crate) space: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RelationshipArgs {
    #[command(subcommand)]
    pub(crate) command: RelationshipCommand,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum RelationshipCommand {
    Create(RelationshipCreateArgs),
    Update(RelationshipUpdateArgs),
    Delete(RevisionArgs),
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RelationshipDescriptionArgs {
    #[arg(long, conflicts_with = "description_file")]
    pub(crate) description: Option<String>,
    /// UTF-8 multiline description, or - for stdin (maximum 16384 bytes).
    #[arg(long)]
    pub(crate) description_file: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RelationshipCreateArgs {
    /// Provider repository key.
    #[arg(long)]
    pub(crate) from: String,
    /// Consumer repository key.
    #[arg(long)]
    pub(crate) to: String,
    #[arg(long)]
    pub(crate) revision: String,
    #[command(flatten)]
    pub(crate) text: RelationshipDescriptionArgs,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct RelationshipUpdateArgs {
    #[command(flatten)]
    pub(crate) record: RevisionArgs,
    #[arg(long)]
    pub(crate) from: Option<String>,
    #[arg(long)]
    pub(crate) to: Option<String>,
    #[command(flatten)]
    pub(crate) text: RelationshipDescriptionArgs,
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
    /// Create a standalone RFC, plan, note, or review from UTF-8 Markdown.
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
    /// Isolation profile filter; absent lists every space.
    #[arg(long)]
    pub(crate) space: Option<String>,
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
    Review,
    Tutorial,
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
    /// UTF-8 Markdown for rfc/plan/note/review, or a self-contained HTML
    /// document for tutorial; - for stdin (maximum 4 MiB; empty Markdown is valid).
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
    /// UTF-8 Markdown for rfc/plan/note/review, or a self-contained HTML
    /// document for tutorial; - for stdin (maximum 4 MiB).
    #[arg(long, value_name = "PATH|-")]
    pub(crate) content_file: Option<PathBuf>,
    /// Explicitly replace content with an empty string (Markdown kinds only).
    #[arg(long, conflicts_with = "content_file")]
    pub(crate) clear_content: bool,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::{ArtifactCommand, KindArg};

    #[test]
    fn command_tree_is_consistent() {
        crate::cli::Cli::command().debug_assert();
    }

    #[test]
    fn tutorial_kind_parses_for_artifact_create() {
        use clap::Parser as _;
        let cli = crate::cli::Cli::try_parse_from([
            "devcroft",
            "artifact",
            "create",
            "--repository",
            "repo",
            "--title",
            "Change tutorial",
            "--kind",
            "tutorial",
            "--content-file",
            "/tmp/tutorial.html",
        ])
        .unwrap();
        let crate::cli::Command::Artifact(args) = cli.command else {
            panic!("expected artifact command");
        };
        let ArtifactCommand::Create(create) = args.command else {
            panic!("expected artifact create");
        };
        assert_eq!(create.kind, KindArg::Tutorial);
        assert_eq!(
            create.content_file,
            std::path::PathBuf::from("/tmp/tutorial.html")
        );
    }
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct CommentArgs {
    #[command(subcommand)]
    pub command: CommentCommand,
}
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum CommentCommand {
    List(CommentList),
    Create(CommentCreate),
    Edit(CommentEdit),
    Resolve(CommentIdentity),
    Reopen(CommentIdentity),
    Delete(CommentIdentity),
}
#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct CommentList {
    #[command(flatten)]
    pub record: IdArgs,
    #[arg(long, conflicts_with = "resolved")]
    pub open: bool,
    #[arg(long, conflicts_with = "open")]
    pub resolved: bool,
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
