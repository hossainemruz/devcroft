//! Kitty protocol interpretation belongs to Ghostty; this module owns only
//! decoded GPU images and viewport placements. No file or network IO occurs here.
use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    sync::Arc,
};

use anyhow::{Result, anyhow};
use gpui_kit::{
    AnyElement, IntoElement, ObjectFit, ParentElement, RenderImage, Styled, StyledImage, div, img,
    px,
};
use libghostty_vt::{
    Terminal,
    alloc::{Allocator, Bytes},
    kitty::graphics::{self, DecodePng, DecodedImage, ImageFormat, PlacementIterator},
};

use crate::metrics::{TERMINAL_PADDING, cell_height, cell_width};

const IMAGE_BUDGET: u64 = 64 * 1024 * 1024;

struct PngDecoder;
impl DecodePng for PngDecoder {
    fn decode_png<'alloc>(
        &mut self,
        alloc: &'alloc Allocator<'_>,
        encoded: &[u8],
    ) -> Option<DecodedImage<'alloc>> {
        let mut reader =
            image::ImageReader::with_format(Cursor::new(encoded), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(IMAGE_BUDGET);
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        reader.limits(limits);
        let rgba = reader.decode().ok()?.into_rgba8();
        if rgba.len() as u64 > IMAGE_BUDGET {
            return None;
        }
        let mut data = Bytes::new_with_alloc(alloc, rgba.len()).ok()?;
        data.copy_from_slice(rgba.as_raw());
        Some(DecodedImage {
            width: rgba.width(),
            height: rgba.height(),
            data,
        })
    }
}

pub(crate) fn configure(terminal: &mut Terminal<'_, '_>) -> Result<()> {
    // Decoder callbacks are thread-local; terminal mutation is on the UI thread.
    graphics::set_png_decoder(Some(Box::new(PngDecoder)))?;
    terminal.set_kitty_image_storage_limit(IMAGE_BUDGET)?;
    // Inline transfers work for local and remote applications. Never let terminal
    // output read arbitrary host files or shared memory.
    terminal.set_kitty_image_from_file_allowed(false)?;
    terminal.set_kitty_image_from_shared_mem_allowed(false)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
struct Geometry {
    image_generation: u64,
    image_id: u32,
    placement_id: u32,
    z: i32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    source: [f32; 4],
}

#[derive(Clone)]
pub(crate) struct Placement {
    geometry: Geometry,
    image: Arc<RenderImage>,
    image_size: (u32, u32),
}

impl Placement {
    pub(crate) fn layer(&self) -> u8 {
        if self.geometry.z < i32::MIN / 2 {
            0
        } else if self.geometry.z < 0 {
            1
        } else {
            2
        }
    }

    pub(crate) fn element(&self, grid: (u16, u16)) -> AnyElement {
        let g = &self.geometry;
        let [x, y, width, height] = g.source;
        let sx = g.width / width.max(f32::EPSILON);
        let sy = g.height / height.max(f32::EPSILON);
        // Crop by positioning the full cached image inside a clipped source
        // rectangle. Multiple placements share the same uploaded pixels.
        let image = div()
            .absolute()
            .left(px(g.x))
            .top(px(g.y))
            .w(px(g.width))
            .h(px(g.height))
            .overflow_hidden()
            .child(
                img(self.image.clone())
                    .absolute()
                    .left(px(-x * sx))
                    .top(px(-y * sy))
                    .w(px(self.image_size.0 as f32 * sx))
                    .h(px(self.image_size.1 as f32 * sy))
                    .object_fit(ObjectFit::Fill),
            );
        div()
            .absolute()
            .left(px(TERMINAL_PADDING))
            .top(px(TERMINAL_PADDING))
            .w(px(grid.0 as f32 * cell_width()))
            .h(px(grid.1 as f32 * cell_height()))
            .overflow_hidden()
            .child(image)
            .into_any_element()
    }
}

#[derive(Default)]
pub(crate) struct Graphics {
    images: HashMap<u64, Arc<RenderImage>>,
    pub(crate) placements: Vec<Placement>,
}

impl Graphics {
    /// Recompute geometry on every presented frame (scroll/resize can move
    /// images without changing their generation). Upload pixels only once.
    pub(crate) fn update(&mut self, terminal: &Terminal<'_, '_>) -> Result<bool> {
        let storage = terminal.kitty_graphics()?;
        if storage.generation()? == 0 {
            let changed = !self.placements.is_empty();
            self.placements.clear();
            self.images.clear();
            return Ok(changed);
        }
        let mut iter = PlacementIterator::new()?;
        let mut iter = iter.update(&storage)?;
        let mut placements = Vec::new();
        let mut live = HashSet::new();
        let placeholders = placeholders(terminal)?;
        let mut used_placeholders = HashSet::new();
        while let Some(placement) = iter.next() {
            let id = placement.image_id()?;
            let Some(image) = storage.image(id) else {
                continue;
            };
            let info = placement.placement_render_info(&image, terminal)?;
            let virtual_placement = placement.is_virtual()?;
            if !virtual_placement
                && (!info.viewport_visible || info.source_width == 0 || info.source_height == 0)
            {
                continue;
            }
            let generation = image.generation()?;
            let size = (image.width()?, image.height()?);
            let rendered = if let Some(cached) = self.images.get(&generation) {
                cached.clone()
            } else {
                let Some(data) = image.data()? else {
                    continue;
                };
                let channels = match image.format()? {
                    ImageFormat::Rgb => 3,
                    ImageFormat::Rgba => 4,
                    _ => continue,
                };
                let pixels = to_bgra(data, channels)?;
                let buffer = image::RgbaImage::from_raw(size.0, size.1, pixels)
                    .ok_or_else(|| anyhow!("Invalid Kitty image size"))?;
                let rendered = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
                self.images.insert(generation, rendered.clone());
                rendered
            };
            live.insert(generation);
            let base = Geometry {
                image_generation: generation,
                image_id: id,
                placement_id: placement.placement_id()?,
                z: placement.z()?,
                x: info.viewport_col as f32 * cell_width()
                    + placement.x_offset()? as f32 * cell_width() / cell_width().round(),
                y: info.viewport_row as f32 * cell_height()
                    + placement.y_offset()? as f32 * cell_height() / cell_height().round(),
                width: info.pixel_width as f32 * cell_width() / cell_width().round(),
                height: info.pixel_height as f32 * cell_height() / cell_height().round(),
                source: [
                    info.source_x as f32,
                    info.source_y as f32,
                    info.source_width as f32,
                    info.source_height as f32,
                ],
            };
            if virtual_placement {
                let grid = placement.grid_size(&image, terminal)?;
                for (index, placeholder) in placeholders.iter().enumerate() {
                    if placeholder.image != id
                        || (placeholder.placement != 0
                            && placeholder.placement != base.placement_id)
                        || used_placeholders.contains(&index)
                    {
                        continue;
                    }
                    used_placeholders.insert(index);
                    if let Some(geometry) =
                        placeholder.geometry(&base, size, (grid.cols, grid.rows))
                    {
                        placements.push(Placement {
                            geometry,
                            image: rendered.clone(),
                            image_size: size,
                        });
                    }
                }
            } else {
                placements.push(Placement {
                    geometry: base,
                    image: rendered,
                    image_size: size,
                });
            }
        }
        self.images
            .retain(|generation, _| live.contains(generation));
        placements.sort_by_key(|p| (p.geometry.z, p.geometry.image_id, p.geometry.placement_id));
        let changed = !placements
            .iter()
            .map(|p| &p.geometry)
            .eq(self.placements.iter().map(|p| &p.geometry));
        self.placements = placements;
        Ok(changed)
    }
}

#[derive(Clone, Copy)]
struct Placeholder {
    image: u32,
    placement: u32,
    column: u32,
    row: u32,
    x: u16,
    y: u16,
}
impl Placeholder {
    fn geometry(&self, base: &Geometry, size: (u32, u32), grid: (u32, u32)) -> Option<Geometry> {
        if self.column >= grid.0 || self.row >= grid.1 || size.0 == 0 || size.1 == 0 {
            return None;
        }
        let width = grid.0 as f32 * cell_width();
        let height = grid.1 as f32 * cell_height();
        let scale = (width / size.0 as f32).min(height / size.1 as f32);
        let offset_x = (width - size.0 as f32 * scale) / 2.;
        let offset_y = (height - size.1 as f32 * scale) / 2.;
        let cell_x = self.column as f32 * cell_width();
        let cell_y = self.row as f32 * cell_height();
        let x = cell_x.max(offset_x);
        let y = cell_y.max(offset_y);
        let right = (cell_x + cell_width()).min(width - offset_x);
        let bottom = (cell_y + cell_height()).min(height - offset_y);
        if right <= x || bottom <= y {
            return None;
        }
        Some(Geometry {
            x: self.x as f32 * cell_width() + x - cell_x,
            y: self.y as f32 * cell_height() + y - cell_y,
            width: right - x,
            height: bottom - y,
            source: [
                (x - offset_x) / scale,
                (y - offset_y) / scale,
                (right - x) / scale,
                (bottom - y) / scale,
            ],
            ..base.clone()
        })
    }
}

fn color_id(color: libghostty_vt::style::StyleColor) -> u32 {
    use libghostty_vt::style::StyleColor;
    match color {
        StyleColor::None => 0,
        StyleColor::Palette(index) => index.0 as u32,
        StyleColor::Rgb(color) => {
            ((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32
        }
    }
}

/// The C binding exposes virtual placement metadata but not Ghostty's Unicode
/// iterator. Decode the protocol's placeholder cells, including omitted column
/// and high-ID diacritics on contiguous cells. Never paint placeholder glyphs.
fn placeholders(terminal: &Terminal<'_, '_>) -> Result<Vec<Placeholder>> {
    use libghostty_vt::terminal::{Point, PointCoordinate};
    let mut result = Vec::new();
    for y in 0..terminal.rows()? {
        let row = terminal.grid_ref(Point::Viewport(PointCoordinate { x: 0, y: y as u32 }))?;
        if !row.row()?.has_kitty_virtual_placeholder()? {
            continue;
        }
        let mut previous: Option<Placeholder> = None;
        for x in 0..terminal.cols()? {
            let cell = terminal.grid_ref(Point::Viewport(PointCoordinate { x, y: y as u32 }))?;
            if cell.cell()?.codepoint()? != 0x10EEEE {
                previous = None;
                continue;
            }
            let style = cell.style()?;
            let low = color_id(style.fg_color);
            let placement = color_id(style.underline_color);
            let mut codepoints = ['\0'; 32];
            let count = cell.graphemes(&mut codepoints)?;
            let index = |n| {
                if n < count {
                    DIACRITICS
                        .binary_search(&(codepoints[n] as u32))
                        .ok()
                        .map(|n| n as u32)
                } else {
                    None
                }
            };
            let row = index(1);
            let column = index(2);
            let high = index(3).filter(|n| *n <= 255);
            let prior = previous.filter(|p| {
                (p.image & 0xFFFFFF) == low
                    && p.placement == placement
                    && row.is_none_or(|row| row == p.row)
                    && column.is_none_or(|column| column == p.column + 1)
                    && high.is_none_or(|high| high == p.image >> 24)
            });
            let placeholder = Placeholder {
                image: low | (high.unwrap_or_else(|| prior.map_or(0, |p| p.image >> 24)) << 24),
                placement,
                row: row.unwrap_or_else(|| prior.map_or(0, |p| p.row)),
                column: column.unwrap_or_else(|| prior.map_or(0, |p| p.column + 1)),
                x,
                y,
            };
            previous = Some(placeholder);
            result.push(placeholder);
        }
    }
    Ok(result)
}

// Protocol codepoint table: https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders

const DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

fn to_bgra(data: &[u8], channels: usize) -> Result<Vec<u8>> {
    if !data.len().is_multiple_of(channels) || data.len() / channels > IMAGE_BUDGET as usize / 4 {
        return Err(anyhow!("Kitty image exceeds pixel budget"));
    }
    let mut pixels = Vec::with_capacity(data.len() / channels * 4);
    for p in data.chunks_exact(channels) {
        pixels.extend_from_slice(&[p[2], p[1], p[0], if channels == 4 { p[3] } else { 255 }]);
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> Terminal<'static, 'static> {
        let mut terminal = Terminal::new(20, 10).unwrap();
        terminal
            .resize(
                20,
                10,
                cell_width().round() as u32,
                cell_height().round() as u32,
            )
            .unwrap();
        configure(&mut terminal).unwrap();
        terminal
    }

    #[test]
    fn virtual_placeholders_render_and_infer_adjacent_columns() {
        let mut terminal = terminal();
        terminal.vt_write(b"\x1b_Ga=T,f=24,s=2,v=1,i=7,U=1,c=2,r=1;/wAAAP8A\x1b\\");
        terminal.vt_write(
            "\x1b[H\x1b[38;2;0;0;7m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\x1b[0m".as_bytes(),
        );
        let cells = placeholders(&terminal).unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].column, 0);
        assert_eq!(cells[1].column, 1);
        let mut renderer = Graphics::default();
        renderer.update(&terminal).unwrap();
        assert_eq!(renderer.placements.len(), 2);
        assert!(Arc::ptr_eq(
            &renderer.placements[0].image,
            &renderer.placements[1].image
        ));
        assert!(renderer.placements[1].geometry.x > renderer.placements[0].geometry.x);
        terminal.vt_write(b"\x1b[2J");
        renderer.update(&terminal).unwrap();
        assert!(renderer.placements.is_empty());
    }

    #[test]
    fn inline_images_cache_crop_scroll_delete_and_switch_screens() {
        let mut terminal = terminal();
        let mut renderer = Graphics::default();
        // Two RGB pixels, red and green; explicit grid dimensions.
        terminal.vt_write(b"\x1b_Ga=T,f=24,s=2,v=1,i=7,c=4,r=2;/wAAAP8A\x1b\\");
        assert!(renderer.update(&terminal).unwrap());
        assert_eq!(renderer.placements.len(), 1);
        let uploaded = renderer.placements[0].image.clone();
        assert_eq!(renderer.placements[0].geometry.source, [0., 0., 2., 1.]);
        assert!(!renderer.update(&terminal).unwrap());
        assert!(Arc::ptr_eq(&uploaded, &renderer.placements[0].image));
        terminal.vt_write(b"\x1b_Ga=p,i=7,p=2,x=1,y=0,w=1,h=1,c=2,r=1,z=-1;\x1b\\");
        assert!(renderer.update(&terminal).unwrap());
        assert_eq!(renderer.placements.len(), 2);
        assert_eq!(renderer.placements[0].geometry.source, [1., 0., 1., 1.]);
        assert_eq!(renderer.placements[0].layer(), 1);
        assert!(Arc::ptr_eq(
            &renderer.placements[0].image,
            &renderer.placements[1].image
        ));
        terminal.vt_write(b"\x1b[?1049h");
        assert!(renderer.update(&terminal).unwrap());
        assert!(renderer.placements.is_empty());
        terminal.vt_write(b"\x1b[?1049l");
        renderer.update(&terminal).unwrap();
        assert_eq!(renderer.placements.len(), 2);
        terminal.vt_write(b"\x1b_Ga=d,d=I,i=7;\x1b\\");
        assert!(renderer.update(&terminal).unwrap());
        assert!(renderer.placements.is_empty());
        assert!(renderer.images.is_empty());
    }

    #[test]
    fn png_decodes_and_color_conversion_preserves_alpha() {
        let mut terminal = terminal();
        terminal.vt_write(b"\x1b_Ga=T,f=100,i=1;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\");
        let mut renderer = Graphics::default();
        renderer.update(&terminal).unwrap();
        assert_eq!(renderer.placements.len(), 1);
        assert_eq!(renderer.placements[0].image_size, (1, 1));
        assert_eq!(to_bgra(&[255, 0, 0, 128], 4).unwrap(), vec![0, 0, 255, 128]);
        assert_eq!(to_bgra(&[255, 0, 0], 3).unwrap(), vec![0, 0, 255, 255]);
        assert!(to_bgra(&[1, 2], 3).is_err());
    }
}
