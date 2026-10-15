use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use crate::tui::widgets::chrome;

pub enum SetupKind {
    Remote,
    RemoteTracker,
    Linear,
}

pub struct Setup {
    pub kind: SetupKind,
    lines: Vec<Line<'static>>,
    scroll: u16,
}

impl Setup {
    pub fn new(kind: SetupKind, lines: Vec<Line<'static>>) -> Self {
        Self {
            kind,
            lines,
            scroll: 0,
        }
    }

    pub fn scroll(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = u16::MAX,
            _ => {}
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let popup = chrome::centered(area, 85, area.height.saturating_sub(2));
        let block = Block::bordered()
            .title(match self.kind {
                SetupKind::Remote => " Set up a remote ",
                SetupKind::RemoteTracker => " Set up the remote's shared Linear app ",
                SetupKind::Linear => " Sign in to Linear ",
            })
            .title_bottom(" ↑/↓ · j/k · PgUp/PgDn scroll · Esc/q close ")
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        let paragraph = Paragraph::new(self.lines.clone()).wrap(Wrap { trim: false });
        let maximum = paragraph
            .line_count(inner.width)
            .saturating_sub(usize::from(inner.height));
        self.scroll = self.scroll.min(maximum.min(usize::from(u16::MAX)) as u16);
        frame.render_widget(paragraph.scroll((self.scroll, 0)), inner);
    }
}
