mod kitty;
mod mermaid;

use std::{collections::HashMap, fs, path::PathBuf, time::Duration};

use ratatui::{
    Frame,
    layout::{Rect, Size},
};
use ratatui_image::{
    picker::{Capability, Picker, ProtocolType, cap_parser::QueryStdioOptions},
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

use self::{
    kitty::{Cells, Placement},
    mermaid::Raster,
};
use crate::task::PICTURE;

// A diagram taller than this is a document, not a picture in a conversation.
const LONGEST: u16 = 40;
// A diagram at its own scale has labels exactly one cell tall, which is how mermaid
// would draw it at 14px next to 14px body text. A conversation is read, not studied,
// so a picture is kept a notch under that: still legible, and a few rows shorter.
const COMPACT: f32 = 0.85;
// Long enough for a terminal across an ssh link to answer, short enough that one
// that never answers does not hold the interface up.
const ANSWER_TIMEOUT: Duration = Duration::from_millis(800);
// Rasters are kept so a second reading of the same diagram costs nothing; only the
// ones a conversation is showing stay warm.
const WARM_RASTERS: usize = 12;

pub struct Pictures {
    picker: Picker,
    is_dark: bool,
    // Where the pictures a signal carries are kept, by the digest that names them.
    attachments: PathBuf,
    rasters: HashMap<u64, Option<Warm>>,
    protocols: HashMap<Key, Option<Picture>>,
    cells: Cells,
}

struct Warm {
    raster: Raster,
    // The cells the diagram covers at its own scale, which is where every other
    // size is measured from.
    natural: Size,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    source: u64,
    columns: u16,
    scale: u16,
}

// The crate's own picture, for the protocols that are not kitty.
struct Picture {
    protocol: SlicedProtocol,
    size: Size,
}

impl Pictures {
    pub fn detect(attachments: PathBuf) -> Self {
        let options = QueryStdioOptions {
            timeout: ANSWER_TIMEOUT,
            terminal_background_color_osc: true,
            ..QueryStdioOptions::default()
        };
        let picker =
            Picker::from_query_stdio_with_options(options).unwrap_or_else(|_| Picker::halfblocks());
        // A terminal that does not say which background it has is assumed dark,
        // which is the theme most of them are set to.
        let is_dark = picker
            .capabilities()
            .iter()
            .find_map(|capability| match capability {
                Capability::Background(red, green, blue) => {
                    Some(is_dark_colour(*red, *green, *blue))
                }
                _ => None,
            })
            .unwrap_or(true);
        Self {
            picker,
            is_dark,
            attachments,
            rasters: HashMap::new(),
            protocols: HashMap::new(),
            cells: Cells::default(),
        }
    }

    pub fn describe(&self) -> String {
        let font = self.picker.font_size();
        format!(
            "{:?} · font {}x{} · {}",
            self.picker.protocol_type(),
            font.width,
            font.height,
            if self.is_dark { "dark" } else { "light" }
        )
    }

    // Halfblocks are coloured squares: a diagram is mostly its labels, so the source
    // the operator can read beats a picture of it they cannot.
    pub fn has_pictures(&self) -> bool {
        self.picker.protocol_type() != ProtocolType::Halfblocks
    }

    // At its own scale a diagram's labels match the words around it, so that is 1.0
    // and the only question is whether it is wider than the column it has.
    pub fn allowance(&mut self, source: &str, columns: u16) -> f32 {
        let Some(natural) = self.warm(source).map(|warm| warm.natural) else {
            return 1.0;
        };
        // Never floored: a diagram drawn larger than its column would be squashed
        // to fit, and a squashed diagram is worse than a small one.
        (f32::from(columns.max(1)) / f32::from(natural.width.max(1))).min(1.0)
    }

    pub fn size(&mut self, source: &str, columns: u16, scale: f32) -> Option<Size> {
        let natural = self.warm(source)?.natural;
        let box_size = fit(natural, columns, scale);
        // A picture placed by cells covers exactly the cells it names, so the box is
        // the size; the crate reports what it will draw for everything else.
        if self.is_placing_by_cells() {
            return Some(box_size);
        }
        self.protocol(source, columns, scale)
            .map(|picture| picture.size)
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        viewport: Rect,
        source: &str,
        columns: u16,
        scale: f32,
        at: (i16, i16),
    ) {
        let Some(natural) = self.warm(source).map(|warm| warm.natural) else {
            return;
        };
        let box_size = fit(natural, columns, scale);
        if self.is_placing_by_cells() {
            let digest = digest(source);
            let Some(warm) = self.rasters.get(&digest).and_then(Option::as_ref) else {
                return;
            };
            let _ = frame;
            self.cells.plan(
                viewport,
                Placement {
                    key: digest,
                    raster: &warm.raster,
                    columns: box_size.width,
                    rows: box_size.height,
                    at,
                },
            );
            return;
        }
        // Every other protocol is laid out by the crate, which knows how to slice a
        // sixel and how much of an iTerm2 image a terminal will take.
        if let Some(picture) = self.protocol(source, columns, scale) {
            let at = SignedPosition::from(at);
            frame.render_widget(SlicedImage::new(&picture.protocol, at), viewport);
        }
    }

    // What the terminal has to be told after this frame: pictures are positioned by
    // the cursor, so they are written once the frame is on screen.
    pub fn settle(&mut self) -> String {
        self.cells.settle()
    }

    fn is_placing_by_cells(&self) -> bool {
        self.picker.protocol_type() == ProtocolType::Kitty
    }

    fn warm(&mut self, source: &str) -> Option<&Warm> {
        let digest = digest(source);
        if !self.rasters.contains_key(&digest) {
            if self.rasters.len() >= WARM_RASTERS {
                self.rasters.clear();
                self.protocols.clear();
            }
            let warm = self.build(source);
            self.rasters.insert(digest, warm);
        }
        self.rasters.get(&digest)?.as_ref()
    }

    fn build(&self, source: &str) -> Option<Warm> {
        let font = self.picker.font_size();
        let raster = match source.strip_prefix(PICTURE) {
            // A picture the host has already read out of a sandbox: its digest names
            // the bytes, and nothing about it needs drawing.
            Some(digest) => Raster {
                picture: image::load_from_memory(&fs::read(self.attachments.join(digest)).ok()?)
                    .ok()?,
            },
            None => mermaid::raster(source, self.is_dark, font.height)?,
        };
        // The raster is oversampled, so the cells it covers are counted in the
        // pixels the terminal will actually show, not the ones that were drawn.
        let natural = Size::new(
            u16::try_from(
                raster
                    .picture
                    .width()
                    .div_ceil(u32::from(font.width).max(1) * 2),
            )
            .ok()?,
            u16::try_from(
                raster
                    .picture
                    .height()
                    .div_ceil(u32::from(font.height).max(1) * 2),
            )
            .ok()?,
        );
        Some(Warm { raster, natural })
    }

    fn protocol(&mut self, source: &str, columns: u16, scale: f32) -> Option<&Picture> {
        let key = Key {
            source: digest(source),
            columns,
            scale: (scale * 1000.0).round() as u16,
        };
        if !self.protocols.contains_key(&key) {
            let picture = self.build_protocol(source, columns, scale);
            self.protocols.insert(key, picture);
        }
        self.protocols.get(&key)?.as_ref()
    }

    fn build_protocol(&self, source: &str, columns: u16, scale: f32) -> Option<Picture> {
        let warm = self.rasters.get(&digest(source))?.as_ref()?;
        let box_size = fit(warm.natural, columns, scale);
        let protocol =
            SlicedProtocol::new(&self.picker, warm.raster.picture.clone(), Some(box_size)).ok()?;
        let size = protocol.size();
        Some(Picture { protocol, size })
    }
}

// Only ever down from the scale the caller asked for, and never past the row budget:
// a diagram smaller than that keeps its own size rather than being blown up into a
// blur, and a tall one gets narrower instead of being squashed.
fn fit(natural: Size, columns: u16, scale: f32) -> Size {
    let wide = f32::from(natural.width.max(1));
    let tall = f32::from(natural.height.max(1));
    // Room for the picture at all, measured on both axes and applied to both: a
    // picture fitted by width and then cut by a row budget would be squashed, and
    // the terminal stretches whatever box it is given to fit.
    let room = (f32::from(columns.max(1)) / wide).min(f32::from(LONGEST) / tall);
    let scale = (scale * COMPACT).min(room).max(f32::MIN_POSITIVE);
    let mut width = (wide * scale).round().max(1.0) as u16;
    let mut height = (f32::from(width) * tall / wide).round().max(1.0) as u16;
    // Rounding can push a side one cell past its budget; giving back that cell on
    // the other axis is what keeps the proportions rather than shaving one side.
    if height > LONGEST {
        height = LONGEST;
        width = (f32::from(height) * wide / tall).round().max(1.0) as u16;
    }
    if width > columns.max(1) {
        width = columns.max(1);
        height = (f32::from(width) * tall / wide).round().max(1.0) as u16;
    }
    Size::new(width.max(1), height.clamp(1, LONGEST))
}

fn is_dark_colour(red: u8, green: u8, blue: u8) -> bool {
    let luma = 0.299 * f32::from(red) + 0.587 * f32::from(green) + 0.114 * f32::from(blue);
    luma < 128.0
}

fn digest(source: &str) -> u64 {
    let hash = blake3::hash(source.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(bytes)
}
