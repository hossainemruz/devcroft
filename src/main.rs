/// Data-directory foundation (`docs/data-directory-plan.md`): root
/// resolution, device store, first-run init, portable-only sync.
/// Startup consumes resolution/init/device-load today; sync, origin, and
/// workspace loading gain UI consumers with Home/Tasks/Review, so the
/// not-yet-wired surface is allow-listed until then — tests cover it now.
#[allow(dead_code, unused_imports)]
mod data;
mod command_palette;
mod fonts;
mod keys;
mod metrics;
mod pane;
mod review;
mod session;
mod workspace;

use anyhow::{Result, anyhow};
use gpui_kit::component::{ActiveTheme as _, Root, Theme, ThemeMode};
use gpui_kit::{AppContext as _, KeyBinding, Styled as _, WindowBounds, WindowOptions, px, size};

use crate::{command_palette::ToggleCommandPalette, fonts::load_terminal_fonts, workspace::Workspace};

fn main() -> Result<()> {
    // App-owned data root: resolve, `mkdir -p`, and first-run init
    // `portable/` (see `docs/data-directory-plan.md`). Failures stay
    // non-fatal — the terminal tabs remain usable without persisted state.
    let device = match data::ensure_ready(None) {
        Ok(root) => match data::DeviceStore::new(&root).load() {
            Ok(state) => Some(state),
            Err(error) => {
                eprintln!("devcroft: device state unavailable: {error:#}");
                None
            }
        },
        Err(error) => {
            eprintln!("devcroft: data directory unavailable: {error:#}");
            None
        }
    };
    let theme_mode = device
        .as_ref()
        .and_then(|state| state.theme.as_deref())
        .map_or(ThemeMode::Dark, theme_mode_from_name);

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
                let workspace = cx.new(|cx| Workspace::new(window, cx));
                cx.new(|cx| Root::new(workspace, window, cx).bg(cx.theme().background))
            })
            .map_err(|error| anyhow!("failed to open Devcroft window: {error}"))
            .expect("failed to open Devcroft window");
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
