//! Full-color editor brand icons for the Settings Editor section.
//!
//! The PNGs under `assets/icons/` (`neovim.png`, `zed.png`, `vscode.png`)
//! are the respective editors' brand marks, used as full-color logos next to
//! the card titles (see `assets/icons/ATTRIBUTION.md`). They belong to their
//! respective owners and are redistributed here for product identification
//! only.
//!
//! PNGs are decoded once with the `image` crate and downscaled to 3x the
//! display size for retina (the same idea as [`crate::agent_icons`], which
//! rasterizes SVGs through the app's SVG renderer). Rows render the cached
//! tiles through [`img`]; when a tile is missing (unparseable bytes), rows
//! fall back to a neutral gpui-kit glyph so they never render an empty slot.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::{Icon, IconName};
use gpui_kit::{AnyElement, IntoElement, Styled as _, px};

/// An editor brand mark with a vendored PNG logo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EditorIcon {
    Neovim,
    Zed,
    VsCode,
}

impl EditorIcon {
    pub(crate) const ALL: [Self; 3] = [Self::Neovim, Self::Zed, Self::VsCode];

    /// Raw PNG bytes for the brand mark.
    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            Self::Neovim => include_bytes!("../assets/icons/neovim.png"),
            Self::Zed => include_bytes!("../assets/icons/zed.png"),
            Self::VsCode => include_bytes!("../assets/icons/vscode.png"),
        }
    }

    /// Neutral glyph used when the tile failed to decode.
    pub(crate) fn fallback(self) -> IconName {
        match self {
            Self::Neovim => IconName::SquareTerminal,
            Self::Zed | Self::VsCode => IconName::ExternalLink,
        }
    }
}

/// Display size of editor logos; tiles decode at 3x for retina.
pub(crate) const ICON_PX: f32 = 14.0;
const TILE_PX: u32 = 42;

pub(crate) type EditorIconTiles = HashMap<EditorIcon, Arc<gpui_kit::RenderImage>>;

/// Decode PNGs and cache tiles, skipping keys already cached and icons
/// with unparseable bytes. Call once when a view is created — each PNG
/// decodes and downscales once per view lifetime, never per frame.
/// Unparseable PNGs are skipped; rows fall back to the neutral glyph.
pub(crate) fn ensure_tiles(
    icons: impl IntoIterator<Item = EditorIcon>,
    tiles: &mut EditorIconTiles,
) {
    for icon in icons {
        if tiles.contains_key(&icon) {
            continue;
        }
        if let Some(tile) = decode_tile(icon.bytes()) {
            tiles.insert(icon, tile);
        }
    }
}

fn decode_tile(bytes: &[u8]) -> Option<Arc<gpui_kit::RenderImage>> {
    let decoded = image::load_from_memory(bytes).ok()?;
    let resized = decoded.resize_to_fill(TILE_PX, TILE_PX, image::imageops::FilterType::Triangle);
    let buffer = resized.to_rgba8();
    Some(Arc::new(gpui_kit::RenderImage::new(vec![
        image::Frame::new(buffer),
    ])))
}

/// Full-color logo for an editor, or its neutral glyph when the PNG failed
/// to decode. Always returns an element so rows never branch.
pub(crate) fn editor_icon(icon: EditorIcon, tiles: &EditorIconTiles, size_px: f32) -> AnyElement {
    match tiles.get(&icon) {
        Some(tile) => gpui_kit::img(tile.clone())
            .size(px(size_px))
            .rounded_sm()
            .into_any_element(),
        None => Icon::new(icon.fallback())
            .size(px(size_px))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_logos_decode() {
        for icon in EditorIcon::ALL {
            let bytes = icon.bytes();
            assert!(!bytes.is_empty());
            assert!(decode_tile(bytes).is_some(), "{icon:?} PNG should decode");
        }
    }
}
