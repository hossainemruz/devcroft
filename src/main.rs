mod add_repository;
mod agent;
mod agent_activity;
mod agent_sessions;
mod agent_skill;
mod artifacts;
mod cli;
mod command_palette;
mod commands;
/// Data-directory foundation (`docs/data-directory-plan.md`): root
/// resolution, device store, first-run init, portable-only sync.
/// Startup consumes resolution/init/device-load; origin and sync gain
/// Settings consumers (manual run, schedule, remote) while workspace
/// loading gains UI consumers with Home/Tasks/Review, so the not-yet-wired
/// surface is allow-listed until then — tests cover it now.
#[allow(dead_code, unused_imports)]
mod data;
mod fonts;
mod git_status;
mod home;
mod keys;
mod metrics;
mod pane;
mod preview;
mod relative_time;
mod review;
mod session;
mod settings;
mod tasks;
mod workspace;
mod workspace_settings;

use anyhow::{Context as _, Result, anyhow};
use clap::Parser as _;
use gpui_kit::component::{ActiveTheme as _, Root, Theme, ThemeMode};
use gpui_kit::{
    AppContext as _, Focusable as _, KeyBinding, SharedString, Styled as _, WindowBounds,
    WindowOptions, px, size,
};

use crate::cli::{Cli, Command};
use crate::{
    command_palette::{
        GoToAgent, GoToEditor, GoToReview, GoToTasks, GoToTerminal, ToggleActionsPalette,
        ToggleProjectsPalette,
    },
    fonts::load_terminal_fonts,
    preview::PreviewView,
    workspace::Workspace,
};

fn main() -> Result<()> {
    // CLI dispatch happens before any GPUI init so later headless commands
    // stay fast (see `docs/cli-plan.md`). `app` boots the workspace;
    // `preview` validates its file first so a bad path fails
    // without ever opening a window.
    let cli = Cli::parse();
    match cli.command {
        Command::App(args) => run_app(args.checkout),
        Command::Preview(args) => run_preview(args.path),
        Command::GitStatus(args) => run_git_status(args.checkout, args.limit),
        Command::Repository(args) => commands::repository(args).context("devcroft repository"),
        Command::Task(args) => commands::task(args).context("devcroft task"),
        Command::Subtask(args) => commands::subtask(args).context("devcroft subtask"),
        Command::Artifact(args) => commands::artifact(args).context("devcroft artifact"),
        Command::Review(args) => cli::review::run(args).context("devcroft review"),
        Command::Skill(args) => agent_skill::run(args).context("devcroft skill"),
    }
}

/// Boot path for `devcroft app [--checkout <path>]`: the previous `main()`
/// behavior verbatim, rooted at `--checkout` (or cwd when absent).
fn run_app(checkout: Option<std::path::PathBuf>) -> Result<()> {
    // Resolve `--checkout` before touching the GUI so a bad path fails fast
    // with a runtime error instead of opening a window rooted elsewhere.
    let working_directory = crate::cli::resolve_working_directory(checkout)?;
    let (theme_mode, app_font_size) = resolve_appearance();
    // Live pane geometry reads this global at render time; Settings edits it
    // later through the same path.
    crate::metrics::set_app_font_size(app_font_size);

    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        // Global command-bar toggles: `cmd-k` opens the action commands,
        // `cmd-p` the project switcher (`cmd` is Super on Linux, so this
        // covers super+k/super+p there too; the `ctrl` variants are fallbacks
        // for environments without a platform modifier). `cmd-a`/`cmd-e` jump
        // straight to the Agent/Editor tabs, `cmd-/` to the Terminal tab, and
        // `cmd-r`/`cmd-t` to the Review/Tasks tabs — all deliberately without
        // `ctrl` fallbacks, so `ctrl-a`/`ctrl-e` (readline
        // beginning/end-of-line) and `ctrl-/` keep reaching
        // terminal applications. The terminal pane also forwards these
        // keystrokes explicitly (see `TerminalPane::on_key_down`), since its
        // raw key handler would otherwise swallow the event while focused.
        cx.bind_keys([
            KeyBinding::new("cmd-k", ToggleActionsPalette, None),
            KeyBinding::new("ctrl-k", ToggleActionsPalette, None),
            KeyBinding::new("cmd-p", ToggleProjectsPalette, None),
            KeyBinding::new("ctrl-p", ToggleProjectsPalette, None),
            KeyBinding::new("cmd-a", GoToAgent, None),
            KeyBinding::new("cmd-e", GoToEditor, None),
            KeyBinding::new("cmd-/", GoToTerminal, None),
            KeyBinding::new("cmd-r", GoToReview, None),
            KeyBinding::new("cmd-t", GoToTasks, None),
        ]);
        load_terminal_fonts(cx).expect("failed to load the bundled JetBrains Mono Nerd Font");
        Theme::change(theme_mode, None, cx);

        let options = WindowOptions {
            titlebar: Some(gpui_kit::TitlebarOptions {
                title: Some("Devcroft".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
            // Wayland app-id / X11 WM_CLASS. Omarchy's universal clipboard
            // shortcuts (`Super+C/V/X`) only send terminal keys
            // (`Shift+Insert`, …) to windows tagged `terminal`, matched on
            // class — without this the compositor sees an empty class,
            // treats Devcroft as a GUI app, and synthesizes `Ctrl+V`,
            // which the terminal pane deliberately passes through to the
            // pty instead of pasting.
            app_id: Some("devcroft".to_owned()),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(window, cx, &working_directory));
                cx.new(|cx| Root::new(workspace, window, cx).bg(cx.theme().background))
            })
            .map_err(|error| anyhow!("failed to open Devcroft window: {error}"))
            .expect("failed to open Devcroft window");
        })
        .detach();
    });
    Ok(())
}

/// Persisted appearance (theme mode plus app font size) shared by every GUI
/// entry point, so the preview window matches the workspace. Failures stay
/// non-fatal with dark/default fallbacks — the terminal tabs and the
/// preview remain usable without persisted state.
fn resolve_appearance() -> (ThemeMode, f32) {
    // App-owned data root: resolve, `mkdir -p`, and first-run init
    // `portable/` (see `docs/data-directory-plan.md`).
    let root = match data::ensure_ready(None) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("devcroft: data directory unavailable: {error:#}");
            return (ThemeMode::Dark, crate::metrics::DEFAULT_APP_FONT_SIZE);
        }
    };
    match data::DeviceStore::new(&root).load() {
        Ok(state) => (
            state
                .theme
                .as_deref()
                .map_or(ThemeMode::Dark, theme_mode_from_name),
            state.app_font_size_or_default(),
        ),
        Err(error) => {
            eprintln!("devcroft: device state unavailable: {error:#}");
            (ThemeMode::Dark, crate::metrics::DEFAULT_APP_FONT_SIZE)
        }
    }
}

/// Boot path for `devcroft preview <path>`: validates the file (failing
/// fast with a runtime error, before any window exists), then opens a
/// standalone preview window — no workspace, no terminal panes, no socket.
/// From Neovim: `:!devcroft preview %`.
fn run_preview(path: std::path::PathBuf) -> Result<()> {
    let (title, content) = match crate::preview::read_markdown_file(&path) {
        Ok(preview) => preview,
        Err(error) => {
            eprintln!("devcroft: preview: {}: {error:#}", path.display());
            std::process::exit(1);
        }
    };
    let (theme_mode, _) = resolve_appearance();
    let content: SharedString = content.into();
    let window_title = format!("Preview — {title}");

    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        load_terminal_fonts(cx).expect("failed to load the bundled JetBrains Mono Nerd Font");
        Theme::change(theme_mode, None, cx);

        let options = WindowOptions {
            titlebar: Some(gpui_kit::TitlebarOptions {
                title: Some(window_title.into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::centered(size(px(900.), px(700.)), cx)),
            // Same identity as the workspace window (see `run_app`): the
            // compositor, taskbars, and window rules key off this.
            app_id: Some("devcroft".to_owned()),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| PreviewView::new(content.clone(), cx));
                view.read(cx).focus_handle(cx).focus(window, cx);
                view
            })
            .map_err(|error| anyhow!("failed to open preview window: {error}"))
            .expect("failed to open preview window");
        })
        .detach();
    });
    Ok(())
}

/// Stored theme name to [`ThemeMode`]. Unknown or absent values keep the
/// default dark theme; the tolerance matches `device.json`'s read defaults.
fn theme_mode_from_name(name: &str) -> ThemeMode {
    match name.trim().to_ascii_lowercase().as_str() {
        "light" => ThemeMode::Light,
        _ => ThemeMode::Dark,
    }
}

/// Headless git-status inspector for loader-vs-CLI disagreements: prints
/// what the header dot sees for a checkout (branch, tracked dirtiness, and
/// the first itemized gix entries) next to the `git status --porcelain`
/// line count, so a stray dirty dot can be attributed to a concrete path.
/// Runs before any GPUI init, like the other headless commands.
fn run_git_status(checkout: Option<std::path::PathBuf>, limit: usize) -> Result<()> {
    use std::fmt::Write as _;

    let working_directory = crate::cli::resolve_working_directory(checkout)?;
    let canonical =
        std::fs::canonicalize(&working_directory).unwrap_or_else(|_| working_directory.clone());
    let summary = crate::git_status::load_git_status(&working_directory);
    let tracked = crate::git_status::has_tracked_changes(&working_directory);
    let items = crate::git_status::status_items(&working_directory, limit.max(1));

    let mut report = String::new();
    let _ = writeln!(report, "workdir: {}", canonical.display());
    let _ = writeln!(
        report,
        "branch: {}",
        summary.branch.as_deref().unwrap_or("(none)")
    );
    let _ = writeln!(report, "detached: {}", summary.detached);
    let _ = writeln!(report, "dirty: {}", summary.dirty);
    let _ = writeln!(report, "tracked_changes: {tracked}");
    let _ = writeln!(
        report,
        "has_upstream: {} ahead: {} behind: {}",
        summary.has_upstream, summary.ahead, summary.behind
    );
    let _ = writeln!(report, "gix_items (showing up to {limit}): {}", items.len());
    for item in &items {
        let _ = writeln!(report, "  {}  {}", item.kind, item.path);
    }
    match cli_porcelain_line_count(&working_directory) {
        Ok(count) => {
            let _ = writeln!(report, "cli_porcelain_lines: {count}");
        }
        Err(error) => {
            let _ = writeln!(report, "cli_porcelain_lines: unavailable ({error:#})");
        }
    }
    // Attribution: which CLI ignore rule (if any) covers each gix item,
    // and what sits directly inside itemized directories. A gix-Untracked
    // item that the CLI ignores (or that holds only ignored children) is
    // the precise shape of a loader-vs-CLI divergence.
    for line in attribute_items(&working_directory, &items) {
        let _ = writeln!(report, "{line}");
    }
    print!("{report}");
    Ok(())
}

/// `git status --porcelain=v1` line count with the ambient environment —/// the same view the user gets in a shell. Kept separate from the test
/// oracle (which isolates config) on purpose: a disagreement between this
/// number and `gix_items` above is exactly the diagnostic signal.
fn cli_porcelain_line_count(workdir: &std::path::Path) -> Result<usize> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["status", "--porcelain=v1"])
        .output()
        .with_context(|| "spawning git status for comparison")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git status failed: {}", stderr.trim());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().filter(|line| !line.trim().is_empty()).count())
}

/// Attribute each gix item from the CLI's side: the `check-ignore` ruling
/// rule (or `not-ignored`), plus a capped listing of direct children when
/// the item is a directory on disk. Best-effort diagnostic output — never
/// fails, degrades to `unavailable` lines when git is missing.
fn attribute_items(
    workdir: &std::path::Path,
    items: &[crate::git_status::StatusItem],
) -> Vec<String> {
    use std::collections::HashMap;

    const CHILD_CAP: usize = 20;

    let mut out = vec!["attribution:".to_owned()];
    if items.is_empty() {
        out.push("  (no gix items to attribute)".to_owned());
        return out;
    }
    // One `check-ignore` call for all item paths: exit status 1 with
    // partial output is the normal "some match, some don't" case.
    let mut rulings: HashMap<String, String> = HashMap::new();
    let check = std::process::Command::new("git")
        .arg("-C")
        .arg(workdir)
        .arg("check-ignore")
        .arg("-v")
        .arg("--")
        .args(items.iter().map(|item| &item.path))
        .output();
    match check {
        Ok(output) => {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                // `source:lineno:pattern<TAB>path`.
                if let Some((rule, path)) = line.split_once('\t') {
                    rulings.insert(path.to_owned(), rule.to_owned());
                }
            }
            if rulings.is_empty() {
                out.push("  check-ignore: not-ignored (no rule covers these paths)".to_owned());
            }
        }
        Err(error) => {
            out.push(format!("  check-ignore: unavailable ({error:#})"));
        }
    }
    for item in items {
        match rulings.get(&item.path) {
            Some(rule) => out.push(format!("  {}: ignored by {rule}", item.path)),
            None => out.push(format!("  {}: not-ignored", item.path)),
        }
        let disk_path = workdir.join(&item.path);
        match std::fs::read_dir(&disk_path) {
            Ok(entries) => {
                let mut children: Vec<String> = entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                            format!("{name}/")
                        } else {
                            name
                        }
                    })
                    .collect();
                children.sort();
                let shown = children.len().min(CHILD_CAP);
                out.push(format!(
                    "    children ({}): {}",
                    children.len(),
                    children[..shown].join(" ")
                ));
            }
            Err(_) => out.push("    (not a directory on disk)".to_owned()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git CLI must be available for fixture setup");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    #[test]
    fn attribute_items_reports_ruling_and_children() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join(".gitignore"), "ignored-dir/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("plain-dir")).unwrap();
        std::fs::write(dir.path().join("plain-dir/child.txt"), "x\n").unwrap();
        let items = vec![crate::git_status::StatusItem {
            path: "plain-dir".to_owned(),
            kind: "walk:Untracked".to_owned(),
        }];
        let lines = attribute_items(dir.path(), &items);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("plain-dir: not-ignored")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("child.txt")),
            "{lines:?}"
        );
    }

    #[test]
    fn attribute_items_empty_without_items() {
        let dir = tempfile::tempdir().unwrap();
        let lines = attribute_items(dir.path(), &[]);
        assert!(
            lines.iter().any(|line| line.contains("no gix items")),
            "{lines:?}"
        );
    }
}
