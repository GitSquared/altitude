//! One message of a conversation, in a card.
//!
//! The card is a rounded edge with the author and the time on it, and the body
//! inside. It holds no fill, so the backdrop stays visible around it, and the
//! border says which side of the conversation wrote it: the operator's cards are
//! accented, the agent's are quiet. A card can be taller than the screen, so it
//! is drawn into its own buffer and only the visible rows are copied out; that
//! is what lets a conversation scroll by the row instead of by the message.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph, Widget, Wrap},
};

use crate::tui::widgets::palette;

/// How far the operator's message got.
///
/// One mark means Altitude recorded it; two mean the agent has it; two in the
/// accent mean the agent has answered on the thread.
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
    pub body: &'a str,
    pub from_operator: bool,
    pub focused: bool,
    /// What the operator's message says about its journey. Absent for the
    /// agent's side, which is not waiting on anyone.
    pub tick: Option<Tick>,
    /// Something the message carries besides its words — a change the agent is
    /// asking for — drawn inside the same card so it reads as part of what was
    /// said rather than as a second message.
    pub nested: Option<Nested<'a>>,
}

/// A block inside a card: the machine-readable part of a message.
///
/// The lines are already fitted to the width by whoever built them, because the
/// card lays itself out by counting rows rather than by wrapping what it is
/// given.
pub struct Nested<'a> {
    pub title: &'a str,
    pub lines: Vec<Line<'a>>,
}

pub fn height(width: u16, body: &str, nested: Option<&Nested<'_>>) -> u16 {
    // The border takes two columns and the padding takes two more.
    let text_width = width.saturating_sub(4).max(1);
    let rows = Paragraph::new(body)
        .wrap(Wrap { trim: true })
        .line_count(text_width);
    let nested = nested.map_or(0, |nested| {
        u16::try_from(nested.lines.len())
            .unwrap_or(u16::MAX)
            // A blank line between the words and the block, and the block's own
            // two borders.
            .saturating_add(3)
    });
    u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .saturating_add(nested)
}

/// Draw a card into its own buffer, copying only the rows that meet `visible`.
///
/// The conversation scrolls by the row, so a card may start above the viewport
/// or end below it. `top_offset` is how many of the card's rows are hidden above
/// `visible`; the whole card is still laid out, so its border, title, and
/// wrapped body stay intact while it is only partly on screen.
pub fn draw_clipped(
    frame: &mut Frame,
    visible: Rect,
    full_height: u16,
    top_offset: u16,
    message: &Message<'_>,
) {
    if visible.width == 0 || visible.height == 0 || full_height == 0 {
        return;
    }
    let mut buffer = Buffer::empty(Rect::new(0, 0, visible.width, full_height));
    let style = if message.focused {
        palette::accent().add_modifier(Modifier::BOLD)
    } else if message.from_operator {
        palette::accent()
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
    let body = Paragraph::new(message.body)
        .style(palette::primary())
        .wrap(Wrap { trim: true });
    let rows = u16::try_from(
        Paragraph::new(message.body)
            .wrap(Wrap { trim: true })
            .line_count(inner.width.max(1)),
    )
    .unwrap_or(u16::MAX);
    body.render(inner, &mut buffer);
    if let Some(nested) = &message.nested {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(nested_style)
            .title(Line::styled(format!(" {} ", nested.title), nested_style));
        let area = Rect {
            x: inner.x,
            y: inner.y.saturating_add(rows).saturating_add(1),
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
