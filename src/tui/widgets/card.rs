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
    /// Whether the message has the keyboard's focus in the conversation.
    pub focused: bool,
    /// What the operator's message says about its journey. Absent for the
    /// agent's side, which is not waiting on anyone.
    pub tick: Option<Tick>,
}

/// How many rows the card takes at this width, border included.
pub fn height(width: u16, body: &str) -> u16 {
    // The border takes two columns and the padding takes two more.
    let text_width = width.saturating_sub(4).max(1);
    let rows = Paragraph::new(body)
        .wrap(Wrap { trim: true })
        .line_count(text_width);
    u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(2)
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
    block.render(buffer.area, &mut buffer);
    if let Some(tick) = message.tick.filter(|_| message.from_operator) {
        stamp_tick(&mut buffer, tick);
    }
    Paragraph::new(message.body)
        .style(palette::primary())
        .wrap(Wrap { trim: true })
        .render(inner, &mut buffer);

    let target = frame.buffer_mut();
    for row in 0..visible.height {
        for column in 0..visible.width {
            target[(visible.x + column, visible.y + row)] =
                buffer[(column, top_offset + row)].clone();
        }
    }
}

/// Put the tick on the bottom border, right-aligned and clear of the corner.
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
