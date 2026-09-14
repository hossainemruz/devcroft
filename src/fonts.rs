//! Bundled application fonts and terminal metrics.

use std::borrow::Cow;

use anyhow::Result;
use gpui_kit::{App, font, px};

use crate::metrics::{BASE_FONT_SIZE, init_cell_width};

pub(crate) const TERMINAL_FONT_FAMILY: &str = "JetBrainsMonoNL NFM";

pub(crate) const MARKDOWN_FONT_FAMILY: &str = "Google Sans Flex";

pub(crate) fn load_bundled_fonts(cx: &App) -> Result<()> {
    cx.text_system().add_fonts(vec![
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/google-sans-flex/GoogleSansFlex-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/google-sans-flex/GoogleSansFlex-SemiBold.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/google-sans-flex/GoogleSansFlex-Bold.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/google-sans-flex/GoogleSansFlex-Italic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/google-sans-flex/GoogleSansFlex-SemiBoldItalic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/google-sans-flex/GoogleSansFlex-BoldItalic.ttf"
        )),
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
    // Measure once at the reference size; `cell_width()` scales linearly
    // from here for the live app-wide size.
    let measured_width = cx
        .text_system()
        .advance(font_id, px(BASE_FONT_SIZE), 'M')?
        .width
        .as_f32();
    init_cell_width(measured_width)
}
