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
    /// Capture/reopen a GitHub PR independently of the checkout.
    #[arg(long,global=true,conflicts_with_all=["base","uncommitted"])]
    pr: Option<String>,
    /// Fetch and capture the latest remote revision instead of resuming offline.
    #[arg(long, global = true, requires = "pr")]
    refresh: bool,
    #[command(subcommand)]
    command: ReviewCommand,
}

#[derive(Debug, PartialEq, Eq, clap::Subcommand)]
enum ReviewCommand {
    /// Open the visual review workspace for an immutable local snapshot.
    Open {
        /// Import an authored manifest/chapter directory after validation.
        #[arg(long)]
        bundle: Option<PathBuf>,
    },
    /// Print the immutable authoring context as JSON (does not run an agent).
    Capture,
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
    if let Some(url) = &args.pr {
        let identity = crate::review::session::pr::Identity::parse(url)?;
        let capture = if !args.refresh
            && let Some(review) = identity.store()?.load()?
        {
            review.active().capture.clone()
        } else {
            crate::review::session::pr::acquire(url)?
        };
        return match &args.command {
            ReviewCommand::Open { bundle } => crate::run_pr_review(capture, bundle.clone()),
            ReviewCommand::Capture => {
                println!("{}", serde_json::to_string(&capture)?);
                Ok(())
            }
            _ => anyhow::bail!(
                "--pr supports open and capture; findings are managed in the visual workspace"
            ),
        };
    }
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
    if let ReviewCommand::Open { bundle } = &args.command {
        return crate::run_visual_review(cwd, diff, base, scope, bundle.clone());
    }
    if matches!(args.command, ReviewCommand::Capture) {
        let capture = crate::review::session::Capture::stable_local(
            &cwd,
            &scope,
            &format!("Local working-tree snapshot · {base}"),
        )?;
        println!("{}", serde_json::to_string(&capture)?);
        return Ok(());
    }
    let store = Store::open(&cwd, &base, &args.remote, &scope, &diff)?;
    match args.command {
        ReviewCommand::Open { .. } | ReviewCommand::Capture => unreachable!(),
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
