use std::{collections::HashMap, time::Duration};

use image::DynamicImage;
use ratatui::{
    Frame,
    layout::{Rect, Size},
};
use ratatui_image::{
    picker::{Capability, Picker, ProtocolType, cap_parser::QueryStdioOptions},
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

use super::mermaid;

// A diagram taller than this is a document, not a picture in a conversation.
const LONGEST: u16 = 60;
// Long enough for a terminal across an ssh link to answer, short enough that one
// that never answers does not hold the interface up.
const ANSWER_TIMEOUT: Duration = Duration::from_millis(800);
// Rasters are kept so a second reading of the same diagram costs nothing; only the
// ones a conversation is showing stay warm.
const WARM_RASTERS: usize = 12;

pub struct Pictures {
    picker: Picker,
    is_dark: bool,
    rasters: HashMap<u64, Option<Raster>>,
    drawn: HashMap<Key, Option<Picture>>,
}

struct Raster {
    picture: DynamicImage,
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

struct Picture {
    protocol: SlicedProtocol,
    size: Size,
}

impl Pictures {
    pub fn detect() -> Self {
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
            rasters: HashMap::new(),
            drawn: HashMap::new(),
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
        let Some(natural) = self.raster(source).map(|raster| raster.natural) else {
            return 1.0;
        };
        // Never floored: a diagram drawn larger than its column would be squashed
        // to fit, and a squashed diagram is worse than a small one.
        (f32::from(columns.max(1)) / f32::from(natural.width.max(1))).min(1.0)
    }

    pub fn size(&mut self, source: &str, columns: u16, scale: f32) -> Option<Size> {
        self.picture(source, columns, scale)
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
        let Some(picture) = self.picture(source, columns, scale) else {
            return;
        };
        let at = SignedPosition::from(at);
        frame.render_widget(SlicedImage::new(&picture.protocol, at), viewport);
    }

    fn raster(&mut self, source: &str) -> Option<&Raster> {
        let digest = digest(source);
        if !self.rasters.contains_key(&digest) {
            if self.rasters.len() >= WARM_RASTERS {
                self.rasters.clear();
                self.drawn.clear();
            }
            let raster = self.build_raster(source);
            self.rasters.insert(digest, raster);
        }
        self.rasters.get(&digest)?.as_ref()
    }

    fn build_raster(&self, source: &str) -> Option<Raster> {
        let font = self.picker.font_size();
        let picture = mermaid::picture(source, self.is_dark, font.height)?;
        // The raster is oversampled, so the cells it covers are counted in the
        // pixels the terminal will actually show, not the ones that were drawn.
        let natural = Size::new(
            u16::try_from(picture.width().div_ceil(u32::from(font.width).max(1) * 2)).ok()?,
            u16::try_from(picture.height().div_ceil(u32::from(font.height).max(1) * 2)).ok()?,
        );
        Some(Raster { picture, natural })
    }

    fn picture(&mut self, source: &str, columns: u16, scale: f32) -> Option<&Picture> {
        let key = Key {
            source: digest(source),
            columns,
            scale: (scale * 1000.0).round() as u16,
        };
        if !self.drawn.contains_key(&key) {
            let picture = self.build(source, columns, scale);
            self.drawn.insert(key, picture);
        }
        self.drawn.get(&key)?.as_ref()
    }

    fn build(&self, source: &str, columns: u16, scale: f32) -> Option<Picture> {
        let raster = self.rasters.get(&digest(source))?.as_ref()?;
        let box_size = fit(raster.natural, columns, scale);
        // The crate fits the image inside the size it is given, so the size it
        // reports back is the one to reserve: asking for a taller box than the
        // picture needs would leave the difference as blank rows under it.
        let protocol =
            SlicedProtocol::new(&self.picker, raster.picture.clone(), Some(box_size)).ok()?;
        let size = protocol.size();
        Some(Picture { protocol, size })
    }
}

// Only ever down from the scale the caller asked for, and never past the row budget:
// a diagram smaller than that keeps its own size rather than being blown up into a
// blur, and a tall one gets narrower instead of being squashed.
fn fit(natural: Size, columns: u16, scale: f32) -> Size {
    let scaled = |value: u16| {
        u16::try_from((f32::from(value) * scale).ceil().max(1.0) as u32)
            .unwrap_or(1)
            .max(1)
    };
    Size::new(
        scaled(natural.width).min(columns.max(1)),
        scaled(natural.height).min(LONGEST),
    )
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
