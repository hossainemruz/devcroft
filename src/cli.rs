//! Command-line surface (`docs/cli-plan.md`, Phase 0 + preview).
//!
//! Day-one contract: `devcroft app [--checkout <path>]` boots the GUI,
//! bare `devcroft` prints help. `devcroft preview <path>` opens the file in
//! a standalone preview window (no workspace, no socket — the lightweight
//! MVP standing in for the Phase 2 `preview.open` socket method). Later
//! phases add headless store commands (`task`, `review`, `sync`) and the
//! remaining live-UI commands (`ref`, `focus`) behind the same parser, with
//! dispatch in `main()` before any GPUI initialization so headless commands
//! stay fast.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

/// First-class CLI: `devcroft app` starts the workspace; future subcommands
/// reuse this parser without changing the dispatch shape in `main()`.
#[derive(Debug, Parser)]
#[command(name = "devcroft", version, about = "Devcroft workspace and agent CLI")]
#[command(arg_required_else_help = true)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Phase 0 surface plus the preview MVP. New resources arrive as
/// additional variants; each maps to a handler under `src/commands/` in
/// later phases. `GitStatus` is the odd one out: a headless diagnostic in
/// the spirit of the planned `status | doctor` command that itemizes what
/// the header dot sees, for loader-vs-CLI disagreements.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub(crate) enum Command {
    /// Boot the GPUI workspace (the current app behavior, unchanged).
    App(AppArgs),
    /// Preview a file in a dialog (currently Markdown; e.g. from Neovim:
    /// `:!devcroft preview %`).
    Preview(PreviewArgs),
    /// Print what the header git dot sees for a checkout: branch, tracked
    /// dirtiness, and the first few itemized gix status entries, plus the
    /// `git status --porcelain` line count for comparison.
    GitStatus(GitStatusArgs),
}

/// Arguments for `devcroft app`.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub(crate) struct AppArgs {
    /// Checkout to open. Defaults to the current directory; terminal tabs,
    /// the project header, git status, and the Review projection all root
    /// here.
    #[arg(long, value_name = "PATH")]
    pub(crate) checkout: Option<PathBuf>,
}

/// Arguments for `devcroft preview <path>`.
///
/// The previewed file resolves against the process cwd, so a Neovim
/// `:!devcroft preview %` previews the current buffer wherever it lives.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub(crate) struct PreviewArgs {
    /// File to preview. Currently rendered as Markdown regardless of
    /// extension; must exist, be a regular file, be valid UTF-8, and stay
    /// under the 4 MiB preview limit.
    #[arg(value_name = "PATH")]
    pub(crate) path: PathBuf,
}

/// Arguments for `devcroft git-status`.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub(crate) struct GitStatusArgs {
    /// Checkout to inspect. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub(crate) checkout: Option<PathBuf>,
    /// Maximum itemized gix entries to print.
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub(crate) limit: usize,
}

/// Resolve the working directory for `app`: explicit `--checkout` wins,
/// else the process current directory.
pub(crate) fn resolve_working_directory(checkout: Option<PathBuf>) -> Result<PathBuf> {
    match checkout {
        Some(path) => {
            if !path.is_dir() {
                anyhow::bail!("checkout is not a directory: {}", path.display());
            }
            Ok(path)
        }
        None => std::env::current_dir().context("resolving current directory for app"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_with_no_checkout_parses() {
        let cli = Cli::try_parse_from(["devcroft", "app"]).unwrap();
        assert_eq!(cli.command, Command::App(AppArgs { checkout: None }));
    }

    #[test]
    fn app_checkout_parses() {
        let cli = Cli::try_parse_from(["devcroft", "app", "--checkout", "/tmp/work"]).unwrap();
        assert_eq!(
            cli.command,
            Command::App(AppArgs {
                checkout: Some(PathBuf::from("/tmp/work")),
            })
        );
    }

    #[test]
    fn preview_parses_with_positional_path() {
        let cli = Cli::try_parse_from(["devcroft", "preview", "README.md"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Preview(PreviewArgs {
                path: PathBuf::from("README.md"),
            })
        );
    }

    #[test]
    fn preview_requires_a_path() {
        let error = Cli::try_parse_from(["devcroft", "preview"]).unwrap_err();
        // Missing positional is a usage error (exit 2 per the CLI plan).
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn git_status_parses_with_defaults() {
        let cli = Cli::try_parse_from(["devcroft", "git-status"]).unwrap();
        assert_eq!(
            cli.command,
            Command::GitStatus(GitStatusArgs {
                checkout: None,
                limit: 30,
            })
        );
    }

    #[test]
    fn git_status_parses_checkout_and_limit() {
        let cli = Cli::try_parse_from([
            "devcroft",
            "git-status",
            "--checkout",
            "/tmp/work",
            "--limit",
            "5",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::GitStatus(GitStatusArgs {
                checkout: Some(PathBuf::from("/tmp/work")),
                limit: 5,
            })
        );
    }

    #[test]
    fn bare_invocation_prints_help() {
        let error = Cli::try_parse_from(["devcroft"]).unwrap_err();
        // `arg_required_else_help` surfaces missing-subcommand as help text.
        let rendered = error.to_string();
        assert!(
            rendered.contains("Usage"),
            "expected usage, got: {rendered}"
        );
    }

    #[test]
    fn checkout_must_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ok = resolve_working_directory(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(ok, dir.path());

        let missing = dir.path().join("no-such-dir");
        let error = resolve_working_directory(Some(missing.clone())).unwrap_err();
        assert!(
            error.to_string().contains(&missing.display().to_string()),
            "error should name the bad path, got: {error:#}"
        );
    }

    #[test]
    fn missing_checkout_defaults_to_current_dir() {
        let resolved = resolve_working_directory(None).unwrap();
        assert_eq!(resolved, std::env::current_dir().unwrap());
    }
}
