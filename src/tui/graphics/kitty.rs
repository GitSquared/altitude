use std::{
    collections::{HashMap, HashSet},
    mem,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use ratatui::layout::Rect;

use super::mermaid::Raster;

// One piece of a transmission carries at most 4096 characters of base64, and every
// piece but the last is followed by another: 3072 raw bytes encode to exactly that.
const PIECE: usize = 3072;

// Pictures placed by naming their own cells. The other thing the protocol offers is a
// virtual placement with unicode placeholders, where the terminal works the cell
// extent out from the image's pixel size and the cell size it believes it has — two
// guesses that disagree on any terminal that reports its font in logical pixels and
// lays images out in device pixels. Naming `c=` and `r=` leaves nothing to work out.
//
// The placements are written to the terminal after the frame rather than into the
// cells of it: a graphics command is positioned where the cursor is, so it belongs at
// a cursor we moved on purpose, not wherever a cell happened to be written.
pub struct Cells {
    viewport: Rect,
    next_id: u32,
    ids: HashMap<u64, u32>,
    sent: HashSet<u32>,
    placed: HashSet<u32>,
    pending: Vec<Emission>,
}

pub struct Placement<'a> {
    pub key: u64,
    pub raster: &'a Raster,
    pub columns: u16,
    pub rows: u16,
    // Where the picture starts, in cells relative to the viewport it is drawn in.
    pub at: (i16, i16),
}

struct Emission {
    id: u32,
    columns: u16,
    rows: u16,
    crop: Option<(u32, u32)>,
    at: (u16, u16),
    // The pixels, when the terminal does not have them yet.
    transmit: Option<String>,
}

impl Default for Cells {
    fn default() -> Self {
        Self {
            viewport: Rect::default(),
            // Zero is a valid image id but a poor sentinel, so ids start at one.
            next_id: 1,
            ids: HashMap::new(),
            sent: HashSet::new(),
            placed: HashSet::new(),
            pending: Vec::new(),
        }
    }
}

impl Cells {
    pub fn plan(&mut self, viewport: Rect, placement: Placement<'_>) {
        // The terminal forgets every image when its screen is rebuilt, so what has
        // been sent is only trustworthy for the viewport it was sent in.
        if viewport != self.viewport {
            self.viewport = viewport;
            self.sent.clear();
            self.placed.clear();
        }
        let Placement {
            key,
            raster,
            columns,
            rows,
            at,
        } = placement;
        if rows == 0 || columns == 0 {
            return;
        }
        let view_top = i32::from(viewport.y);
        let view_bottom = i32::from(viewport.bottom());
        let left = i32::from(viewport.x) + i32::from(at.0);
        let picture_top = view_top + i32::from(at.1);
        let visible_top = picture_top.max(view_top);
        let visible_bottom = (picture_top + i32::from(rows)).min(view_bottom);
        if visible_bottom <= visible_top
            || left >= i32::from(viewport.right())
            || left < i32::from(viewport.x)
        {
            return;
        }

        // Only the rows that fit are asked for, as a crop of the picture rather than a
        // shorter one, so what is scrolled off stays off.
        let hidden = u16::try_from(visible_top - picture_top).unwrap_or(0);
        let shown = u16::try_from(visible_bottom - visible_top).unwrap_or(rows);
        let id = match self.ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.ids.insert(key, id);
                id
            }
        };
        let transmit = if self.sent.insert(id) {
            Some(transmit(id, &raster.picture))
        } else {
            None
        };
        self.pending.push(Emission {
            id,
            columns,
            rows: shown,
            crop: crop_of(raster, rows, hidden, shown),
            at: (
                u16::try_from(left).unwrap_or(0),
                u16::try_from(visible_top).unwrap_or(0),
            ),
            transmit,
        });
    }

    // What the terminal has to be told once the frame is on screen: where each picture
    // goes, and the pixels of any it has not been given yet.
    pub fn settle(&mut self) -> String {
        let mut out = String::new();
        let wanted: HashSet<u32> = self.pending.iter().map(|emission| emission.id).collect();
        // Every placement this frame made last time is dropped before this frame's
        // are made: `d=i` deletes the placements of an image and keeps its pixels, so
        // a picture that moved cannot leave a copy behind at the old position.
        for id in &self.placed {
            out.push_str(&format!("\u{1b}_Ga=d,d=i,i={id},q=2;\u{1b}\\"));
        }
        for emission in mem::take(&mut self.pending) {
            if let Some(transmit) = &emission.transmit {
                out.push_str(transmit);
            }
            out.push_str(&format!(
                "\u{1b}[{};{}H",
                emission.at.1.saturating_add(1),
                emission.at.0.saturating_add(1)
            ));
            out.push_str(&place(
                emission.id,
                emission.columns,
                emission.rows,
                emission.crop,
            ));
        }
        self.placed = wanted;
        out
    }
}

// The pixels the picture is shown from, so a partly scrolled picture shows the part
// that is left rather than a squashed whole.
fn crop_of(raster: &Raster, rows: u16, hidden: u16, shown: u16) -> Option<(u32, u32)> {
    if hidden == 0 && shown >= rows {
        return None;
    }
    let height = raster.picture.height();
    let cells = u32::from(rows).max(1);
    let y = height.saturating_mul(u32::from(hidden)) / cells;
    let h = height.saturating_mul(u32::from(shown)) / cells;
    Some((y, h.max(1)))
}

// Pixels, not a PNG: the format key that carries image files is the one place
// terminals disagree, and a raw RGBA payload is what the reference client sends.
fn transmit(id: u32, picture: &image::DynamicImage) -> String {
    let rgba = picture.to_rgba8();
    let raw = rgba.as_raw();
    let pieces = raw.len().div_ceil(PIECE).max(1);
    let mut out = String::with_capacity(raw.len() / 3 * 4 + 64);
    for (index, piece) in raw.chunks(PIECE).enumerate() {
        let more = u8::from(index + 1 < pieces);
        let head = if index == 0 {
            format!(
                "a=t,f=32,t=d,q=2,i={id},s={},v={},m={more};",
                rgba.width(),
                rgba.height()
            )
        } else {
            format!("m={more};")
        };
        out.push_str("\u{1b}_G");
        out.push_str(&head);
        out.push_str(&STANDARD.encode(piece));
        out.push_str("\u{1b}\\");
    }
    out
}

fn place(id: u32, columns: u16, rows: u16, crop: Option<(u32, u32)>) -> String {
    let crop = crop.map_or_else(String::new, |(y, h)| format!(",y={y},h={h}"));
    format!("\u{1b}_Ga=p,q=2,i={id},c={columns},r={rows}{crop};\u{1b}\\")
}
