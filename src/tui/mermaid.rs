use std::sync::{Arc, OnceLock};

use image::DynamicImage;
use mermaid_rs_renderer::{LayoutConfig, Theme, compute_layout, parse_mermaid, render_svg};
use resvg::{
    tiny_skia,
    usvg::{self, fontdb::Database},
};

// Diagrams are drawn at twice their nominal size so a card's worth of columns
// still has pixels to spare when the picture is scaled down to cells.
const SCALE: f32 = 2.0;

pub fn picture(source: &str, is_dark: bool) -> Option<DynamicImage> {
    let parsed = parse_mermaid(source).ok()?;
    let theme = themed(is_dark);
    let config = LayoutConfig::default();
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
    let mut pixmap = tiny_skia::Pixmap::new(
        (size.width() * SCALE) as u32,
        (size.height() * SCALE) as u32,
    )?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(SCALE, SCALE),
        &mut pixmap.as_mut(),
    );
    // A round trip through PNG is what un-premultiplies tiny-skia's alpha, and it
    // is paid once per diagram rather than once per frame.
    image::load_from_memory(&pixmap.encode_png().ok()?).ok()
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
