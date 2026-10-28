//! The containers a task tracker route can claim, as a list under its field.
//!
//! The same shape as the repository picker: the field opens the list, the list
//! takes every key while it is open, and choosing closes it with the value the
//! caller stores. The only difference is where the offers come from.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use crate::{
    tracker::ScopeSummary,
    tui::{
        keys::is_secondary,
        widgets::{
            palette,
            select::{Offer, Select},
            utils::shimmer,
        },
    },
};

pub struct Scopes {
    pub select: Select<ScopeSummary>,
    rows: Vec<(Rect, usize)>,
    error: Option<String>,
}

pub enum Key {
    Still,
    Closed,
    Save,
    Chosen(ScopeSummary),
}

impl Scopes {
    pub fn new() -> Self {
        let mut select = Select::new("team", Vec::new(), 0).without_search();
        select.open();
        Self {
            select,
            rows: Vec::new(),
            error: None,
        }
    }

    pub fn fill(&mut self, scopes: &[ScopeSummary]) {
        self.select.set_offers(offers(scopes));
        self.error = None;
    }

    pub fn failed(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn is_listing(&self) -> bool {
        self.error.is_none() && self.select.is_empty()
    }

    pub fn key(&mut self, key: KeyEvent) -> Key {
        if is_secondary(key) {
            return Key::Save;
        }
        self.select.handle_key(key);
        if self.select.is_open() {
            return Key::Still;
        }
        if key.code == KeyCode::Enter
            && let Some(scope) = self.select.chosen_value().cloned()
        {
            return Key::Chosen(scope);
        }
        Key::Closed
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, anchor: Rect, tick: Option<usize>) {
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
        let line = match &self.error {
            Some(error) => Line::styled(error.as_str(), palette::attention()),
            None => Line::from(shimmer("listing teams…", tick)),
        };
        frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), inner);
    }
}

fn offers(scopes: &[ScopeSummary]) -> Vec<Offer<ScopeSummary>> {
    let mut offers = vec![Offer::new(
        ScopeSummary {
            id: String::new(),
            key: String::new(),
            name: "whole account".to_owned(),
        },
        "Whole account",
    )];
    offers.extend(scopes.iter().cloned().map(|scope| {
        let label = if scope.key.is_empty() {
            scope.name.clone()
        } else {
            format!("{} ({})", scope.name, scope.key)
        };
        Offer::new(scope, label)
    }));
    offers
}
