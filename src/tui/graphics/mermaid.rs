use std::sync::{Arc, OnceLock};

use image::{DynamicImage, imageops::crop_imm};
use mermaid_rs_renderer::{LayoutConfig, Theme, compute_layout, parse_mermaid, render_svg};
use ratatui::layout::Size;
use resvg::{
    tiny_skia,
    usvg::{self, fontdb::Database},
};

use super::{Drawn, drawn_scale};

// Twice the pixels the terminal will show, so a picture stays sharp on a display
// that is finer than the cells the font reports.
const OVERSAMPLE: f32 = 2.0;

// A diagram is rasterized for the column it is drawn in, so what it is drawn at is
// counted from the column rather than from a size it is only ever shrunk from.
pub fn raster(source: &str, is_dark: bool, cell: Size, columns: u16) -> Option<Drawn> {
    let parsed = parse_mermaid(source).ok()?;
    let theme = themed(is_dark);
    let config = layout();
    let layout = compute_layout(&parsed.graph, &theme, &config);
    let svg = render_svg(&layout, &theme, &config);

    let options = usvg::Options {
        fontdb: fonts(),
        ..usvg::Options::default()
    };
    // Without a font database every label is dropped without an error, leaving a
    // diagram of empty boxes.
    let tree = usvg::Tree::from_str(&svg, &options).ok()?;
    let size = tree.size();
    // A label one cell row tall is the diagram at its own scale, whatever the theme's
    // type size happens to be. Its pixels per point in the raster, and the cells it
    // covers, are both counted from there.
    let points = f32::from(cell.height.max(1)) / theme.font_size;
    let natural = Size::new(
        as_cells(size.width() * points / f32::from(cell.width.max(1))),
        as_cells(size.height() / theme.font_size),
    );

    let scale = OVERSAMPLE * points * drawn_scale(natural, columns);
    let mut pixmap = tiny_skia::Pixmap::new(
        (size.width() * scale) as u32,
        (size.height() * scale) as u32,
    )?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia draws premultiplied alpha and a terminal wants it straight, which is
    // one pass over the pixels rather than a PNG round trip.
    let (width, height) = (pixmap.width(), pixmap.height());
    let picture = image::RgbaImage::from_raw(width, height, pixmap.take_demultiplied())?;
    Some(Drawn {
        picture: tightened(&picture.into()),
        natural,
        columns,
    })
}

fn as_cells(extent: f32) -> u16 {
    u16::try_from(extent.round().max(1.0) as u32).unwrap_or(u16::MAX)
}

// The canvas a diagram is drawn on is not its content: cropping to the ink is what
// keeps a card from carrying the difference as blank space.
fn tightened(picture: &DynamicImage) -> DynamicImage {
    let pixels = picture.to_rgba8();
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for (x, y, pixel) in pixels.enumerate_pixels() {
        if pixel[3] <= 8 {
            continue;
        }
        bounds = Some(match bounds {
            None => (x, y, x, y),
            Some((left, top, right, bottom)) => {
                (left.min(x), top.min(y), right.max(x), bottom.max(y))
            }
        });
    }
    let Some((left, top, right, bottom)) = bounds else {
        return picture.clone();
    };
    crop_imm(&pixels, left, top, right - left + 1, bottom - top + 1)
        .to_image()
        .into()
}

// Mermaid lays a diagram out for a page: roomy boxes, wide gaps between ranks, and
// labels left on one line. A conversation column is narrow and tall, and every pixel
// of width a diagram takes is a pixel off the size its words are drawn at, so the
// layout is made for the column it has to fit: boxes close to their text, ranks
// close together, and labels wrapped early so a node grows downward instead of the
// whole picture growing sideways.
fn layout() -> LayoutConfig {
    LayoutConfig {
        node_spacing: 24.0,
        rank_spacing: 24.0,
        node_padding_x: 12.0,
        node_padding_y: 8.0,
        label_line_height: 1.3,
        max_label_width_chars: 16,
        ..LayoutConfig::default()
    }
}

fn themed(is_dark: bool) -> Theme {
    let mut theme = if is_dark {
        Theme::dark()
    } else {
        Theme::modern()
    };
    // No background of its own: the diagram sits on whatever the card is drawn
    // on, so it belongs to the conversation instead of pasted onto a slab.
    theme.background = "none".to_owned();
    theme.font_family = "ui-sans-serif, system-ui, sans-serif".to_owned();
    theme
}

fn fonts() -> Arc<Database> {
    static FONTS: OnceLock<Arc<Database>> = OnceLock::new();
    Arc::clone(FONTS.get_or_init(|| {
        let mut database = Database::new();
        database.load_system_fonts();
        Arc::new(database)
    }))
}
