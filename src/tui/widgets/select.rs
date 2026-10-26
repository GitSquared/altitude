//! Choosing one of a few known things: a value you press, and a list you step
//! through or type at.
//!
//! A page draws the value as a small box and gives this widget the keys while
//! the focus is on it: enter opens the list, j/k or the arrows move through it,
//! and enter again takes what is under the keys. The box is the same one the
//! prompt wears, because a border is how this interface says a thing can be
//! acted on rather than read. A list long enough to be worth
//! narrowing takes the first letters as a query instead, because stepping
//! through a dozen machines is not how anyone finds one. The list floats over
//! the page rather than taking a row of it, so opening it does not move what the
//! operator was reading.
//!
//! The offers are the page's; this widget only knows what each one is called and
//! whether it can be chosen, so one control serves a machine, an agent and an
//! environment alike. A page whose offers are rebuilt from the network keeps its
//! choice by value rather than by position, so a remote that dropped out of the
//! list cannot quietly become a different one.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph},
};

use crate::tui::widgets::{palette, utils::truncate};

/// How many offers a list needs before typing narrows it. Under that, j/k and
/// the arrows are worth more than a query: the list is already short enough to
/// step through, and a search box over three rows is a mode to remember.
const SEARCH_FROM: usize = 5;

/// The mark that says a pill opens.
const OPENS: char = '▼';

/// One thing that can be chosen.
pub struct Offer<T> {
    pub value: T,
    pub label: String,
    /// Whether it can be taken. One that cannot is still listed, because the
    /// reason it cannot is worth reading.
    pub ready: bool,
}

impl<T> Offer<T> {
    pub fn new(value: T, label: impl Into<String>) -> Self {
        Self {
            value,
            label: label.into(),
            ready: true,
        }
    }
}

/// A value chosen from its offers, and the open list when it is open.
pub struct Select<T> {
    /// What is being chosen, shown on the pill and over the list.
    name: &'static str,
    offers: Vec<Offer<T>>,
    chosen: usize,
    open: bool,
    /// What has been typed to narrow a list that is searched. A short list never
    /// has anything here: its keys are for moving.
    query: String,
    /// Which match the keys are on, as an index into the matches rather than
    /// into the offers.
    at: usize,
    /// How many offers it takes before typing narrows the list. A filter of
    /// seven known values wants j/k; a list of machines wants a query.
    search_from: usize,
}

impl<T> Select<T> {
    pub fn new(name: &'static str, offers: Vec<Offer<T>>, chosen: usize) -> Self {
        Self {
            name,
            chosen: chosen.min(offers.len().saturating_sub(1)),
            offers,
            open: false,
            query: String::new(),
            at: 0,
            search_from: SEARCH_FROM,
        }
    }

    /// Keep every key for stepping, even when the list is longer than the search
    /// threshold. For a short, known set of values, a query is a mode to
    /// remember for no gain.
    pub fn without_search(mut self) -> Self {
        self.search_from = usize::MAX;
        self
    }

    pub fn chosen_offer(&self) -> Option<&Offer<T>> {
        self.offers.get(self.chosen)
    }

    pub fn chosen_value(&self) -> Option<&T> {
        self.chosen_offer().map(|offer| &offer.value)
    }

    /// The chosen value's name, or nothing when there is none to choose.
    pub fn label(&self) -> &str {
        self.chosen_offer().map_or("", |offer| offer.label.as_str())
    }

    pub fn choose(&mut self, index: usize) {
        if index < self.offers.len() {
            self.chosen = index;
        }
    }

    /// Choose the first offer `test` accepts, answering whether there was one.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether this list is long enough that typing narrows it, which is what
    /// decides whether the keys are a query or a way through.
    pub fn is_searching(&self) -> bool {
        self.open && self.offers.len() >= self.search_from
    }

    /// Open the list on the chosen offer, with nothing typed.
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.at = self.chosen;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.at = 0;
    }

    /// Take a key while the list is open, answering whether it was the list's.
    ///
    /// Every key is the list's while it is open, escape included: it closes the
    /// list rather than the page, which is what a dropdown does everywhere else.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if !self.open {
            return false;
        }
        let searching = self.is_searching();
        let matches = self.matches();
        match key.code {
            KeyCode::Esc => self.close(),
            // A list that is not being typed at leaves the way everything else
            // does.
            KeyCode::Char('q') if !searching => self.close(),
            KeyCode::Enter => {
                if let Some(index) = matches.get(self.at)
                    && self.offers[*index].ready
                {
                    self.chosen = *index;
                    self.close();
                }
            }
            KeyCode::Down | KeyCode::Char('j') if !searching => self.step(true, matches.len()),
            KeyCode::Up | KeyCode::Char('k') if !searching => self.step(false, matches.len()),
            // Tab is still the way down a list that is being typed at, because
            // there is no other way to move without leaving a letter behind.
            KeyCode::Down | KeyCode::Tab => self.step(true, matches.len()),
            KeyCode::Up | KeyCode::BackTab => self.step(false, matches.len()),
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.step(true, matches.len());
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.step(false, matches.len());
            }
            KeyCode::Backspace if searching => {
                self.query.pop();
                self.at = 0;
            }
            // A chord is not text, and a query is one line, so left, right and
            // the delete key do nothing rather than move a caret this list does
            // not have.
            KeyCode::Char(character)
                if searching
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
                self.query.push(character);
                self.at = 0;
            }
            _ => {}
        }
        true
    }

    /// Move the keys one match, for a wheel notch over the open list.
    pub fn scroll(&mut self, forward: bool) {
        if self.open {
            let matches = self.matches();
            self.step(forward, matches.len());
        }
    }

    /// Move the keys one match, and round again at either end: a list of three
    /// is quicker to run round than to hit its last row and stop.
    fn step(&mut self, forward: bool, matches: usize) {
        if matches == 0 {
            self.at = 0;
            return;
        }
        let at = self.at.min(matches - 1);
        self.at = if forward {
            (at + 1) % matches
        } else {
            (at + matches - 1) % matches
        };
    }

    /// The offers the typed query still matches, in the order they were given.
    fn matches(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.offers
            .iter()
            .enumerate()
            .filter(|(_, offer)| query.is_empty() || offer.label.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    /// How wide this pill wants to be: its name when there is room for one, its
    /// value, and the mark that it opens.
    pub fn pill_width(&self, with_name: bool) -> u16 {
        (self.name_width(with_name) + self.label().chars().count() + 3) as u16
    }

    /// How much of a pill its name takes: the word and the space around it, or
    /// the space before the value when there is no room for the name.
    fn name_width(&self, with_name: bool) -> usize {
        if with_name {
            self.name.chars().count() + 2
        } else {
            1
        }
    }

    /// Draws the chosen value as a pill: its name, what it is set to, and a mark
    /// that it opens.
    ///
    /// The accent fill is the same one a button or a list row uses when it holds
    /// the keys, so "the keys are here" is one thing to recognise across the
    /// interface. Inside, the name is quiet and the value is not, which is the
    /// whole difference between the two.
    pub fn draw_pill(&self, frame: &mut Frame, area: Rect, focused: bool, with_name: bool) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let chosen = self.chosen_offer();
        let (value_style, quiet) = if focused {
            (palette::focus(), palette::focus())
        } else if chosen.is_some_and(|offer| !offer.ready) {
            (palette::attention(), palette::muted())
        } else {
            (palette::primary(), palette::muted())
        };
        let value = truncate(
            self.label(),
            usize::from(area.width).saturating_sub(self.name_width(with_name) + 3),
        );
        let mut spans = Vec::new();
        if with_name {
            spans.push(Span::styled(format!(" {} ", self.name), quiet));
        } else {
            spans.push(Span::styled(" ", quiet));
        }
        spans.push(Span::styled(value, value_style));
        spans.push(Span::styled(format!(" {OPENS} "), quiet));
        // The fill covers the spaces as well as the words, because the pill is
        // the whole of what the fill is for.
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(if focused {
                palette::focus()
            } else {
                Style::default()
            }),
            area,
        );
    }

    /// Add pasted text to the query, as one line: a list is narrowed by words,
    /// not by whatever line breaks came with them.
    pub fn paste(&mut self, text: &str) {
        if self.is_searching() {
            self.query.push_str(&text.replace(['\r', '\n'], " "));
            self.at = 0;
        }
    }

    /// Draws the open list over `area`, opening upwards from `anchor`, and
    /// answers with where each offer was drawn so a click can find it.
    pub fn draw_list(&self, frame: &mut Frame, area: Rect, anchor: Rect) -> Vec<(Rect, usize)> {
        self.draw_list_in(frame, area, anchor, true)
    }

    /// Draws the open list downwards from `anchor`, for a pill above the content
    /// it filters. It is as tall as its matches and no taller, and as wide as its
    /// widest row so it does not paper over the page it is drawn on.
    pub fn draw_list_below(
        &self,
        frame: &mut Frame,
        area: Rect,
        anchor: Rect,
    ) -> Vec<(Rect, usize)> {
        self.draw_list_in(frame, area, anchor, false)
    }

    fn draw_list_in(
        &self,
        frame: &mut Frame,
        area: Rect,
        anchor: Rect,
        opens_up: bool,
    ) -> Vec<(Rect, usize)> {
        if !self.open || area.height == 0 || area.width == 0 {
            return Vec::new();
        }
        let searching = self.is_searching();
        let matches = self.matches();
        let widest = matches
            .iter()
            .filter_map(|index| self.offers.get(*index))
            .map(|offer| offer.label.chars().count())
            .max()
            .unwrap_or(0);
        // The list is as wide as the widest of: its rows (two borders, two
        // columns of padding and the mark before the label), its own title, and
        // the pill it belongs to.
        let width = (widest + 6)
            .max(self.name.chars().count() + 4)
            .max(usize::from(anchor.width))
            .max(if searching { 20 } else { 0 })
            .min(usize::from(area.width)) as u16;
        if area.height < 3 {
            return Vec::new();
        }
        let height = (matches.len() as u16 + 2 + u16::from(searching))
            .max(3)
            .min(area.height);
        let y = if opens_up {
            anchor.y.saturating_sub(height).max(area.y)
        } else {
            anchor
                .bottom()
                .min(area.bottom().saturating_sub(height))
                .max(area.y)
        };
        let popup = Rect {
            x: anchor.x.min(area.right().saturating_sub(width)),
            y,
            width,
            height,
        };
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(palette::accent())
            .padding(Padding::horizontal(1))
            .title(Line::styled(format!(" {} ", self.name), palette::heading()));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        if inner.height == 0 {
            return Vec::new();
        }
        let [query_row, list_area] =
            Layout::vertical([Constraint::Length(u16::from(searching)), Constraint::Min(0)])
                .areas(inner);
        if searching {
            // The typed query is marked out from the matches under it, so the
            // first row is not read as what has been typed.
            let typed = format!("› {}", self.query);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    truncate(&typed, usize::from(query_row.width)),
                    palette::primary(),
                )),
                query_row,
            );
            frame.set_cursor_position((
                query_row.x
                    + (Line::from(typed).width() as u16).min(query_row.width.saturating_sub(1)),
                query_row.y,
            ));
        }
        if list_area.height == 0 {
            return Vec::new();
        }
        if matches.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled("nothing matches", palette::muted())),
                list_area,
            );
            return Vec::new();
        }
        // Drawn row by row rather than through a list widget, because the same
        // rectangles have to be handed to the mouse: a widget that scrolls knows
        // where its rows went and does not say.
        let height = usize::from(list_area.height);
        let offset = (self.at + 1).saturating_sub(height);
        matches
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .filter_map(|(row, index)| {
                let offer = self.offers.get(*index)?;
                let area = Rect {
                    x: list_area.x,
                    y: list_area.y + (row - offset) as u16,
                    width: list_area.width,
                    height: 1,
                };
                let chosen = *index == self.chosen;
                let focused = row == self.at;
                frame.render_widget(
                    Paragraph::new(offer_line(offer, chosen, focused, usize::from(area.width)))
                        .style(if focused {
                            palette::focus()
                        } else {
                            Style::default()
                        }),
                    area,
                );
                Some((area, *index))
            })
            .collect()
    }
}

/// One offer: the mark that it is the one chosen, and what it is called.
fn offer_line<T>(offer: &Offer<T>, chosen: bool, focused: bool, width: usize) -> Line<'static> {
    let mark = if chosen { "✓" } else { " " };
    if focused {
        return Line::styled(
            truncate(&format!("{mark} {}", offer.label), width),
            palette::focus(),
        );
    }
    let style: Style = if offer.ready {
        palette::primary()
    } else {
        palette::muted()
    };
    Line::from(vec![
        Span::styled(
            format!("{mark} "),
            if chosen {
                palette::accent()
            } else {
                palette::muted()
            },
        ),
        Span::styled(offer.label.clone(), style),
    ])
}
