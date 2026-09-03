mod fonts;
mod keys;
mod metrics;
mod pane;
mod session;
mod workspace;

use anyhow::{Result, anyhow};
use gpui_kit::component::{ActiveTheme as _, Root, Theme, ThemeMode};
use gpui_kit::{AppContext as _, Styled as _, WindowBounds, WindowOptions, px, size};

use crate::{fonts::load_terminal_fonts, workspace::Workspace};

fn main() -> Result<()> {
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        load_terminal_fonts(cx).expect("failed to load the bundled JetBrains Mono Nerd Font");
        Theme::change(ThemeMode::Dark, None, cx);

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
