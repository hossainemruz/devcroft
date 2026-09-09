use crate::review::{
    comments::Store,
    git::{ReviewScope, load_review, suggest_base_branch},
};
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq, clap::Args)]
pub(crate) struct ReviewArgs {
    #[arg(long, global = true)]
    checkout: Option<PathBuf>,
    #[arg(long, global = true)]
    base: Option<String>,
    #[arg(long, global = true, default_value = "origin")]
    remote: String,
    /// Use comments from the uncommitted scope (default: full diff).
    #[arg(long, global = true)]
    uncommitted: bool,
    #[command(subcommand)]
    command: ReviewCommand,
}

#[derive(Debug, PartialEq, Eq, clap::Subcommand)]
enum ReviewCommand {
    /// List comments as JSON, with fresh anchors and source excerpts.
    List {
        #[arg(long, conflicts_with = "resolved")]
        open: bool,
        #[arg(long)]
        resolved: bool,
    },
    /// Mark a comment resolved after addressing it.
    Resolve {
        id: String,
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Reopen a resolved comment.
    Reopen {
        id: String,
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Permanently delete a comment by its ID.
    Delete {
        id: String,
        #[arg(long)]
        revision: Option<u64>,
    },
}

pub(crate) fn run(args: ReviewArgs) -> Result<()> {
    let cwd = super::resolve_working_directory(args.checkout)?;
    let base = args.base.unwrap_or_else(|| {
        suggest_base_branch(&cwd, &args.remote).unwrap_or_else(|| "main".into())
    });
    let scope = if args.uncommitted {
        ReviewScope::UncommittedChanges
    } else {
        ReviewScope::full_diff(&base, &args.remote)
    };
    let diff = load_review(&cwd, &scope)?;
    let store = Store::open(&cwd, &base, &args.remote, &scope, &diff)?;
    match args.command {
        ReviewCommand::List { open, resolved } => {
            let comments = store.refresh(&cwd, &diff)?;
            let values: Vec<_> = comments
                .into_iter()
                .filter(|c| (!open || !c.resolved) && (!resolved || c.resolved))
                .map(|mut c| {
                    let excerpt = c
                        .anchor
                        .source
                        .lines()
                        .skip(c.anchor.start as usize - 1)
                        .take((c.anchor.end - c.anchor.start + 1) as usize)
                        .collect::<Vec<_>>()
                        .join("\n");
                    c.anchor.source.clear();
                    serde_json::json!({"comment": c, "excerpt": excerpt})
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&values)?);
        }
        ReviewCommand::Resolve { id, revision } => {
            store.change(&id, revision, None, Some(true), false)?
        }
        ReviewCommand::Reopen { id, revision } => {
            store.change(&id, revision, None, Some(false), false)?
        }
        ReviewCommand::Delete { id, revision } => store.change(&id, revision, None, None, true)?,
    }
    Ok(())
}
