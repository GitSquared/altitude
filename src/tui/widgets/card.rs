//! One message of a conversation, in a card.
//!
//! The card is a rounded edge with the author and the time on it, and the body
//! inside. It holds no fill, so the backdrop stays visible around it, and the
//! border says which side of the conversation wrote it: the operator's cards are
//! accented, the agent's are quiet.

use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph, Wrap},
};

use crate::tui::widgets::palette;

pub struct Message<'a> {
    pub author: &'a str,
    pub when: &'a str,
    pub body: &'a str,
    pub from_operator: bool,
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

pub fn draw(frame: &mut Frame, area: Rect, message: &Message<'_>) {
    let style = if message.from_operator {
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
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(message.body)
            .style(palette::primary())
            .wrap(Wrap { trim: true }),
        inner,
    );
}
