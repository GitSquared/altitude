//! The repositories a version-control login can reach, as a list.
//!
//! A component rather than a page: a project's settings and a new installation's
//! first screen both ask the same question, and both want the same list, the same
//! keys, and the same "still listing" state. What differs is where the answer
//! goes, which is the caller's business.

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};
use std::mem;

use crate::{
    tui::{
        keys::is_secondary,
        widgets::{
            palette,
            select::{Offer, Select},
        },
    },
    vcs::Repository,
};

pub struct Repositories {
    pub select: Select<Repository>,
    /// Where each offer was drawn, so a click can find it.
    rows: Vec<(Rect, usize)>,
    /// Why there is nothing to choose, when it is not still on its way.
    error: Option<String>,
}

/// What a key did while the list was open.
pub enum Key {
    /// The list is still open.
    Still,
    /// The list closed without choosing: the caller keeps what it had.
    Closed,
    /// The form should save, as ctrl-enter does everywhere.
    Save,
    /// The list closed on this repository.
    Chosen(Repository),
}

impl Repositories {
    /// Open on the repository already chosen, when it is in the list.
    pub fn new(repositories: &[Repository], current: Option<&str>) -> Self {
        let chosen = current
            .and_then(|current| {
                repositories
                    .iter()
                    .position(|repository| repository.url == current)
            })
            .unwrap_or(0);
        let mut select = Select::new("repository", offers(repositories), chosen);
        select.open();
        Self {
            select,
            rows: Vec::new(),
            error: None,
        }
    }

    /// Take a fresh listing, keeping what was chosen where it is still there.
    pub fn fill(&mut self, repositories: &[Repository]) {
        let chosen = self
            .select
            .chosen_value()
            .map(|repository| repository.url.clone());
        self.select.set_offers(offers(repositories));
        if let Some(url) = chosen
            && let Some(index) = repositories
                .iter()
                .position(|repository| repository.url == url)
        {
            self.select.choose(index);
        }
        self.error = None;
    }

    pub fn failed(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn key(&mut self, key: KeyEvent) -> Key {
        if is_secondary(key) {
            return Key::Save;
        }
        self.select.handle_key(key);
        if self.select.is_open() {
            return Key::Still;
        }
        // Closed by enter rather than escape: take what was under the keys.
        if key.code == KeyCode::Enter
            && let Some(repository) = self.select.chosen_value().cloned()
        {
            return Key::Chosen(repository);
        }
        Key::Closed
    }

    /// Take a mouse event against the rows the last draw returned, answering the
    /// repository a line was clicked on.
    pub fn mouse(&mut self, mouse: MouseEvent) -> Option<Repository> {
        let rows = mem::take(&mut self.rows);
        if !self.select.mouse(mouse, &rows) {
            self.rows = rows;
            return None;
        }
        if self.select.is_open() {
            self.rows = rows;
            return None;
        }
        self.select.chosen_value().cloned()
    }

    /// Draw the list under the field it fills, or why it has not arrived.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, anchor: Rect) {
        if !self.select.is_empty() {
            self.rows = self.select.draw_list_below(frame, area, anchor);
            return;
        }
        self.rows.clear();
        let height = 4;
        let popup = Rect {
            y: anchor
                .bottom()
                .min(area.bottom().saturating_sub(height))
                .max(area.y),
            height,
            ..anchor
        };
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(palette::muted())
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let (text, style) = match &self.error {
            Some(error) => (error.as_str(), palette::attention()),
            None => ("listing repositories…", palette::muted()),
        };
        frame.render_widget(
            Paragraph::new(Line::styled(text, style)).wrap(Wrap { trim: true }),
            inner,
        );
    }
}

fn offers(repositories: &[Repository]) -> Vec<Offer<Repository>> {
    repositories
        .iter()
        .cloned()
        .map(|repository| {
            let label = if repository.is_private {
                format!("{} · private", repository.name)
            } else {
                repository.name.clone()
            };
            Offer::new(repository, label)
        })
        .collect()
}
