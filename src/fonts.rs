//! Bundled terminal font loading and metrics.

use std::borrow::Cow;

use anyhow::Result;
use gpui_kit::{App, font, px};

use crate::metrics::{TERMINAL_FONT_SIZE, init_cell_width};

pub(crate) const TERMINAL_FONT_FAMILY: &str = "JetBrainsMonoNL NFM";

pub(crate) fn load_terminal_fonts(cx: &App) -> Result<()> {
    cx.text_system().add_fonts(vec![
        Cow::Borrowed(
            include_bytes!("../assets/JetBrainsMonoNLNerdFontMono-Regular.ttf").as_slice(),
        ),
        Cow::Borrowed(include_bytes!("../assets/JetBrainsMonoNLNerdFontMono-Bold.ttf").as_slice()),
        Cow::Borrowed(
            include_bytes!("../assets/JetBrainsMonoNLNerdFontMono-Italic.ttf").as_slice(),
        ),
        Cow::Borrowed(
            include_bytes!("../assets/JetBrainsMonoNLNerdFontMono-BoldItalic.ttf").as_slice(),
        ),
    ])?;
    let font_id = cx.text_system().resolve_font(&font(TERMINAL_FONT_FAMILY));
    let measured_width = cx
        .text_system()
        .advance(font_id, px(TERMINAL_FONT_SIZE), 'M')?
        .width
        .as_f32();
    init_cell_width(measured_width)
}
