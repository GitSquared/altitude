use std::{
    cmp::Reverse,
    sync::{Arc, OnceLock},
};

use image::{DynamicImage, imageops::crop_imm};
use mermaid_rs_renderer::{
    LayoutConfig, Theme, compute_layout, config::FlowchartLayoutConfig, parse_mermaid, render_svg,
};
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
    let svg = move_labels_clear_of_boxes(&straighten_edges(&render_svg(&layout, &theme, &config)));

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
        node_spacing: 44.0,
        rank_spacing: 36.0,
        node_padding_x: 16.0,
        node_padding_y: 10.0,
        label_line_height: 1.45,
        max_label_width_chars: 18,
        flowchart: FlowchartLayoutConfig {
            // Ports kept away from a node's corners, so two edges leaving one side
            // are given a point each instead of one shared point that the router has
            // to loop back around.
            port_pad_ratio: 0.35,
            ..FlowchartLayoutConfig::default()
        },
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

// The renderer gives each end of an edge its own attachment point, and two that miss
// each other by a few pixels are drawn as a rounded chamfer — a kink on a line meant
// to be straight, which on a short hop is the whole shape anybody sees.
fn straighten_edges(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut cursor = 0;
    while let Some(found) = svg[cursor..].find("class=\"edgePath\"") {
        let after_class = cursor + found + "class=\"edgePath\"".len();
        let Some(start) = svg[after_class..]
            .find(" d=\"")
            .map(|at| after_class + at + 4)
        else {
            break;
        };
        let Some(end) = svg[start..].find('"').map(|quote| start + quote) else {
            break;
        };
        out.push_str(&svg[cursor..start]);
        out.push_str(&straight_path(&svg[start..end]));
        cursor = end;
    }
    out.push_str(&svg[cursor..]);
    out
}

const JOG: f32 = 8.0;

fn straight_path(path: &str) -> String {
    let numbers: Vec<f32> = path
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter(|token| !token.is_empty())
        .filter_map(|token| token.parse().ok())
        .collect();
    if numbers.len() < 4 {
        return path.to_owned();
    }
    let (x0, y0) = (numbers[0], numbers[1]);
    let (x1, y1) = (numbers[numbers.len() - 2], numbers[numbers.len() - 1]);
    if (x1 - x0).abs() < JOG && (y1 - y0).abs() >= JOG {
        return format!("M {x1:.3},{y0:.3} L {x1:.3},{y1:.3}");
    }
    if (y1 - y0).abs() < JOG && (x1 - x0).abs() >= JOG {
        return format!("M {x0:.3},{y1:.3} L {x1:.3},{y1:.3}");
    }
    path.to_owned()
}

// The renderer places an edge label against its own route and hands back a picture
// whose nodes are painted over labels, so a label with no room in the gap it runs
// through is drawn under a box and lost. Nothing in its layout can be asked to leave
// room, so a label that lands on one is moved to the nearest space the diagram has.
fn move_labels_clear_of_boxes(svg: &str) -> String {
    let placed = placed_rects(svg);
    let Some(canvas) = canvas_size(svg) else {
        return svg.to_owned();
    };
    let boxes: Vec<Rect> = placed
        .iter()
        .filter(|rect| rect.edge.is_none())
        .map(|rect| rect.area)
        .collect();
    let labels: Vec<&PlacedRect> = placed.iter().filter(|rect| rect.edge.is_some()).collect();
    let mut edits: Vec<Edit> = Vec::new();
    for label in &labels {
        let others: Vec<Rect> = boxes
            .iter()
            .copied()
            .chain(
                labels
                    .iter()
                    .filter(|other| other.at != label.at)
                    .map(|other| other.area),
            )
            .collect();
        if !others.iter().any(|obstacle| label.area.meets(obstacle)) {
            continue;
        }
        let Some(group) = label_group(svg, label.edge.as_deref().unwrap_or_default()) else {
            continue;
        };
        let Some(centre) = nearest_clear_centre(label.area, &others, canvas) else {
            continue;
        };
        let (dx, dy) = (
            centre.0 - label.area.centre().0,
            centre.1 - label.area.centre().1,
        );
        shift_attributes(svg, (label.at, label.end), dx, dy, &mut edits);
        shift_attributes(svg, group, dx, dy, &mut edits);
    }
    apply_edits(svg, edits)
}

const LABEL_CLEARANCE: f32 = 4.0;
const SEARCH_STEP: f32 = 6.0;
const SEARCH_ARC: f32 = 10.0;
const SEARCH_LIMIT: f32 = 900.0;

#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Rect {
    fn centre(&self) -> (f32, f32) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    fn meets(&self, other: &Rect) -> bool {
        self.x < other.x + other.width + LABEL_CLEARANCE
            && other.x < self.x + self.width + LABEL_CLEARANCE
            && self.y < other.y + other.height + LABEL_CLEARANCE
            && other.y < self.y + self.height + LABEL_CLEARANCE
    }

    fn is_inside(&self, canvas: (f32, f32)) -> bool {
        self.x >= LABEL_CLEARANCE
            && self.y >= LABEL_CLEARANCE
            && self.x + self.width <= canvas.0 - LABEL_CLEARANCE
            && self.y + self.height <= canvas.1 - LABEL_CLEARANCE
    }
}

struct PlacedRect {
    at: usize,
    end: usize,
    area: Rect,
    edge: Option<String>,
}

fn placed_rects(svg: &str) -> Vec<PlacedRect> {
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = svg[cursor..].find("<rect ") {
        let at = cursor + offset;
        let Some(close) = svg[at..].find("/>") else {
            break;
        };
        let end = at + close + 2;
        let attributes = &svg[at..end];
        cursor = end;
        // The canvas rect carries no ink and is the frame every picture is measured
        // against, not something a label can land on.
        if attributes.contains(" fill=\"none\"") {
            continue;
        }
        let (Some(x), Some(y), Some(width), Some(height)) = (
            attribute(attributes, "x"),
            attribute(attributes, "y"),
            attribute(attributes, "width"),
            attribute(attributes, "height"),
        ) else {
            continue;
        };
        found.push(PlacedRect {
            at,
            end,
            area: Rect {
                x,
                y,
                width,
                height,
            },
            edge: attributes
                .contains("data-label-kind")
                .then(|| text_attribute(attributes, "data-edge-id"))
                .flatten(),
        });
    }
    found
}

fn canvas_size(svg: &str) -> Option<(f32, f32)> {
    let start = svg.find("<svg ")?;
    let end = start + svg[start..].find('>')?;
    let tag = &svg[start..end];
    Some((attribute(tag, "width")?, attribute(tag, "height")?))
}

fn attribute(text: &str, name: &str) -> Option<f32> {
    text_attribute(text, name)?.parse().ok()
}

fn text_attribute(text: &str, name: &str) -> Option<String> {
    let marker = format!(" {name}=\"");
    let start = text.find(&marker)? + marker.len();
    let end = start + text[start..].find('"')?;
    Some(text[start..end].to_owned())
}

fn label_group(svg: &str, edge: &str) -> Option<(usize, usize)> {
    let marker = format!("<g class=\"edgeLabel\" data-edge-id=\"{edge}\"");
    let start = svg.find(&marker)?;
    let end = start + svg[start..].find("</g>")?;
    Some((start, end))
}

fn nearest_clear_centre(area: Rect, obstacles: &[Rect], canvas: (f32, f32)) -> Option<(f32, f32)> {
    let (centre_x, centre_y) = area.centre();
    let mut radius = 0.0;
    while radius <= SEARCH_LIMIT {
        let mut angle: f32 = 0.0;
        while angle < 360.0 {
            let (sine, cosine) = angle.to_radians().sin_cos();
            let (x, y) = (centre_x + radius * cosine, centre_y + radius * sine);
            let candidate = Rect {
                x: x - area.width / 2.0,
                y: y - area.height / 2.0,
                ..area
            };
            if candidate.is_inside(canvas)
                && !obstacles.iter().any(|obstacle| candidate.meets(obstacle))
            {
                return Some((x, y));
            }
            angle += SEARCH_ARC;
        }
        radius += SEARCH_STEP;
    }
    None
}

struct Edit {
    at: usize,
    len: usize,
    text: String,
}

fn shift_attributes(svg: &str, range: (usize, usize), dx: f32, dy: f32, edits: &mut Vec<Edit>) {
    for (marker, delta) in [(" x=\"", dx), (" y=\"", dy)] {
        let mut cursor = range.0;
        while let Some(found) = svg[cursor..range.1].find(marker) {
            let start = cursor + found + marker.len();
            let Some(end) = svg[start..range.1].find('"').map(|quote| start + quote) else {
                break;
            };
            if let Ok(value) = svg[start..end].parse::<f32>() {
                edits.push(Edit {
                    at: start,
                    len: end - start,
                    text: format!("{:.3}", value + delta),
                });
            }
            cursor = end;
        }
    }
}

fn apply_edits(svg: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|edit| Reverse(edit.at));
    let mut out = svg.to_owned();
    for edit in edits {
        out.replace_range(edit.at..edit.at + edit.len, &edit.text);
    }
    out
}
