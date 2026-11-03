use std::{collections::HashMap, time::Duration};

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

pub struct Pictures {
    picker: Picker,
    is_dark: bool,
    drawn: HashMap<Key, Option<Picture>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    source: u64,
    columns: u16,
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
            drawn: HashMap::new(),
        }
    }

    // Halfblocks are coloured squares: a diagram is mostly its labels, so the source
    // the operator can read beats a picture of it they cannot.
    pub fn has_pictures(&self) -> bool {
        self.picker.protocol_type() != ProtocolType::Halfblocks
    }

    pub fn size(&mut self, source: &str, columns: u16) -> Option<Size> {
        self.picture(source, columns).map(|picture| picture.size)
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        viewport: Rect,
        source: &str,
        columns: u16,
        top: i16,
    ) {
        let Some(picture) = self.picture(source, columns) else {
            return;
        };
        frame.render_widget(
            SlicedImage::new(&picture.protocol, SignedPosition::from((1, top))),
            viewport,
        );
    }

    fn picture(&mut self, source: &str, columns: u16) -> Option<&Picture> {
        let key = Key {
            source: digest(source),
            columns,
        };
        if !self.drawn.contains_key(&key) {
            let picture = self.build(source, columns);
            self.drawn.insert(key, picture);
        }
        self.drawn.get(&key)?.as_ref()
    }

    fn build(&self, source: &str, columns: u16) -> Option<Picture> {
        let picture = mermaid::picture(source, self.is_dark)?;
        let font = self.picker.font_size();
        let natural = Size::new(
            u16::try_from(picture.width().div_ceil(u32::from(font.width).max(1))).ok()?,
            u16::try_from(picture.height().div_ceil(u32::from(font.height).max(1))).ok()?,
        );
        let size = fit(natural, columns, LONGEST);
        let protocol = SlicedProtocol::new(&self.picker, picture, Some(size)).ok()?;
        Some(Picture { protocol, size })
    }
}

// Only ever down: a diagram smaller than the column it has been given keeps its
// own size rather than being blown up into a blur. Both limits scale it, so a tall
// diagram gets narrower instead of being squashed into its row budget.
fn fit(natural: Size, columns: u16, rows: u16) -> Size {
    let allowance = |limit: u16, value: u16| f32::from(limit.max(1)) / f32::from(value.max(1));
    let scale = allowance(columns, natural.width)
        .min(allowance(rows, natural.height))
        .min(1.0);
    let scaled = |value: u16| {
        u16::try_from((f32::from(value) * scale).ceil().max(1.0) as u32)
            .unwrap_or(1)
            .clamp(1, u16::MAX)
    };
    Size::new(
        scaled(natural.width).min(columns.max(1)),
        scaled(natural.height).min(rows.max(1)),
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
