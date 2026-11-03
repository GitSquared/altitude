use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use crate::tui::widgets::{format::truncate, palette};

pub fn draw(frame: &mut Frame, area: Rect, name: &str, detail: Option<&str>, note: Option<&str>) {
    frame.render_widget(Clear, area);
    let note = note.unwrap_or_default();
    let reserved = name.chars().count() + 3 + note.chars().count() + 2;
    let available = usize::from(area.width).saturating_sub(reserved);
    let mut line = vec![Span::styled(name.to_owned(), palette::heading())];
    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        line.push(Span::styled(" · ", palette::muted()));
        line.push(Span::styled(truncate(detail, available), palette::muted()));
    }
    frame.render_widget(Paragraph::new(Line::from(line)), area);
    if !note.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(note.to_owned(), palette::muted())).right_aligned(),
            area,
        );
    }
}
