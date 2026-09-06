mod cli;
mod command_palette;
/// Data-directory foundation (`docs/data-directory-plan.md`): root
/// resolution, device store, first-run init, portable-only sync.
/// Startup consumes resolution/init/device-load today; sync, origin, and
/// workspace loading gain UI consumers with Home/Tasks/Review, so the
/// not-yet-wired surface is allow-listed until then — tests cover it now.
#[allow(dead_code, unused_imports)]
mod data;
mod fonts;
mod git_status;
mod keys;
mod metrics;
mod pane;
mod preview;
mod review;
mod session;
mod settings;
mod workspace;

use anyhow::{Result, anyhow};
use clap::Parser as _;
use gpui_kit::component::{ActiveTheme as _, Root, Theme, ThemeMode};
use gpui_kit::{
    AppContext as _, Focusable as _, KeyBinding, SharedString, Styled as _, WindowBounds,
    WindowOptions, px, size,
};

use crate::cli::{Cli, Command};
use crate::{
    command_palette::ToggleCommandPalette, fonts::load_terminal_fonts, preview::PreviewView,
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
        // Global command-bar toggle. The terminal pane also forwards this
        // keystroke explicitly (see `TerminalPane::on_key_down`), since its
        // raw key handler would otherwise swallow the event while focused.
        cx.bind_keys([
            KeyBinding::new("cmd-k", ToggleCommandPalette, None),
            KeyBinding::new("ctrl-k", ToggleCommandPalette, None),
        ]);
        load_terminal_fonts(cx).expect("failed to load the bundled JetBrains Mono Nerd Font");
        Theme::change(theme_mode, None, cx);

        let options = WindowOptions {
            titlebar: Some(gpui_kit::TitlebarOptions {
                title: Some("Devcroft".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
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
