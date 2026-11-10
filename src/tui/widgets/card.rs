use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Rect, Size},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph, Widget},
};

use crate::tui::widgets::{markdown, palette};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tick {
    Sent,
    Delivered,
    Read,
}

impl Tick {
    fn glyphs(self) -> &'static str {
        match self {
            Self::Sent => "✓",
            Self::Delivered | Self::Read => "✓✓",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Sent | Self::Delivered => palette::muted(),
            Self::Read => palette::accent(),
        }
    }
}

pub struct Message<'a> {
    pub author: &'a str,
    pub when: &'a str,
    pub body: &'a markdown::Rendered,
    pub from_operator: bool,
    pub focused: bool,
    pub flashing: bool,
    pub tick: Option<Tick>,
    pub nested: Option<Nested<'a>>,
    // The cells each picture of the body covers, in the order it carries them. A body
    // whose pictures are not drawn carries none, and its words stand as written.
    pub sizes: &'a [Size],
}

// Where a picture goes, in the frame's own coordinates rather than the card's: a
// picture is drawn onto the terminal after the card has been copied there, not
// into the card's scratch buffer.
pub struct PictureSlot<'a> {
    pub source: &'a str,
    pub x: u16,
    pub width: u16,
    pub top: i32,
}

pub struct Nested<'a> {
    pub title: &'a str,
    pub lines: Vec<Line<'a>>,
}

pub fn height(body: &markdown::Rendered, nested: Option<&Nested<'_>>, pictures: &[Size]) -> u16 {
    let nested = nested.map_or(0, |nested| {
        u16::try_from(nested.lines.len())
            .unwrap_or(u16::MAX)
            // A blank line between the words and the block, and the block's own
            // two borders.
            .saturating_add(3)
    });
    // A picture takes the rows it covers between the lines it stands between, so the
    // body is as tall as its words and its pictures together.
    let rows = u16::try_from(body.lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(pictures_height(pictures));
    rows.saturating_add(2).saturating_add(nested)
}

fn pictures_height(pictures: &[Size]) -> u16 {
    pictures
        .iter()
        .fold(0, |total, picture| total.saturating_add(picture.height))
}

pub fn draw_clipped<'a>(
    frame: &mut Frame,
    visible: Rect,
    full_height: u16,
    top_offset: u16,
    message: &Message<'a>,
) -> Vec<PictureSlot<'a>> {
    if visible.width == 0 || visible.height == 0 || full_height == 0 {
        return Vec::new();
    }
    let mut buffer = Buffer::empty(Rect::new(0, 0, visible.width, full_height));
    // Accent is the selection and nothing else; the operator's own words are
    // simply not dimmed, which is enough to tell whose turn it is.
    let style = if message.focused || message.flashing {
        palette::accent().add_modifier(Modifier::BOLD)
    } else if message.from_operator {
        palette::primary()
    } else {
        palette::muted()
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            format!(" {} · {} ", message.author, message.when),
            style,
        ));
    let inner = block.inner(buffer.area);
    let nested_style = if message.focused {
        palette::accent()
    } else {
        palette::muted()
    };
    block.render(buffer.area, &mut buffer);
    if let Some(tick) = message.tick.filter(|_| message.from_operator) {
        stamp_tick(&mut buffer, tick);
    }
    // The rows a picture covers are left empty where the body names it; the picture
    // itself is drawn once this card has reached the frame.
    let mut laid = Vec::with_capacity(message.body.lines.len());
    let mut slots = Vec::with_capacity(message.sizes.len());
    let mut taken = 0;
    for (index, picture) in message.body.pictures.iter().enumerate() {
        let after = picture.after.min(message.body.lines.len());
        laid.extend_from_slice(&message.body.lines[taken..after]);
        taken = after;
        let Some(size) = message.sizes.get(index) else {
            continue;
        };
        let top = i32::from(visible.y) - i32::from(top_offset)
            + i32::from(inner.y)
            + i32::try_from(laid.len()).unwrap_or(i32::MAX);
        slots.push(PictureSlot {
            source: &picture.source,
            x: visible.x.saturating_add(inner.x),
            width: inner.width,
            top,
        });
        laid.resize(
            laid.len().saturating_add(usize::from(size.height)),
            Line::default(),
        );
    }
    laid.extend_from_slice(&message.body.lines[taken.min(message.body.lines.len())..]);
    let rows = u16::try_from(laid.len()).unwrap_or(u16::MAX);
    Paragraph::new(laid).render(inner, &mut buffer);
    let below = rows.saturating_add(1);
    if let Some(nested) = &message.nested {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(nested_style)
            .title(Line::styled(format!(" {} ", nested.title), nested_style));
        let area = Rect {
            x: inner.x,
            y: inner.y.saturating_add(below),
            width: inner.width,
            height: u16::try_from(nested.lines.len())
                .unwrap_or(u16::MAX)
                .saturating_add(2),
        };
        let inside = block.inner(area);
        block.render(area, &mut buffer);
        Paragraph::new(nested.lines.clone()).render(inside, &mut buffer);
    }

    let target = frame.buffer_mut();
    for row in 0..visible.height {
        for column in 0..visible.width {
            target[(visible.x + column, visible.y + row)] =
                buffer[(column, top_offset + row)].clone();
        }
    }
    slots
}

fn stamp_tick(buffer: &mut Buffer, tick: Tick) {
    let glyphs = tick.glyphs().chars().collect::<Vec<_>>();
    let row = buffer.area.height.saturating_sub(1);
    let end = buffer.area.width.saturating_sub(2);
    let start = end.saturating_sub(u16::try_from(glyphs.len()).unwrap_or(u16::MAX));
    for (index, glyph) in glyphs.into_iter().enumerate() {
        let column = start.saturating_add(u16::try_from(index).unwrap_or(0));
        if column < end {
            buffer[(column, row)]
                .set_symbol(&glyph.to_string())
                .set_style(tick.style());
        }
    }
}
