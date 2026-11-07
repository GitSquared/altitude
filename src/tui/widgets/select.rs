use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Widget},
};

use crate::tui::widgets::{chrome, format::truncate, palette};

pub fn shown_pill(buffer: &mut Buffer, area: Rect, value: &str, focused: bool) {
    let room = usize::from(area.width).saturating_sub(3);
    let (value_style, quiet) = if focused {
        (palette::focus(), palette::focus())
    } else {
        (palette::primary(), palette::muted())
    };
    pill_in(
        buffer,
        area,
        &[
            Span::styled(" ", quiet),
            Span::styled(truncate(value, room), value_style),
            Span::styled(format!(" {OPENS} "), quiet),
        ],
        focused,
    );
}

fn pill_in(buffer: &mut Buffer, area: Rect, spans: &[Span<'static>], focused: bool) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // The fill covers the spaces as well as the words, because the pill is the
    // whole of what the fill is for.
    Paragraph::new(Line::from(spans.to_vec()))
        .style(if focused {
            palette::focus()
        } else {
            Style::default()
        })
        .render(area, buffer);
}

// A list that opens over the thing it changes. It owns every key while it is open,
// escape included, and that is one rule for every menu however it is drawn, so it is
// stated here rather than in each screen that draws one.
pub trait Menu {
    fn is_open(&self) -> bool;
    fn key(&mut self, key: KeyEvent);
}

impl<T> Menu for Select<T> {
    fn is_open(&self) -> bool {
        Select::is_open(self)
    }

    fn key(&mut self, key: KeyEvent) {
        Select::handle_key(self, key);
    }
}

// The menu that is open, if one is. A screen lists its menus in one place so that the
// order they are asked in is the order they are drawn in, and the first one open is
// the one in front.
pub fn opened<'a>(menus: impl IntoIterator<Item = &'a mut dyn Menu>) -> Option<&'a mut dyn Menu> {
    menus.into_iter().find(|menu| menu.is_open())
}

const SEARCH_FROM: usize = 5;

const OPENS: char = '▼';

pub struct Offer<T> {
    pub value: T,
    pub label: String,
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

pub struct Select<T> {
    name: &'static str,
    offers: Vec<Offer<T>>,
    chosen: usize,
    open: bool,
    filtering: bool,
    query: String,
    at: usize,
    search_from: usize,
}

impl<T> Select<T> {
    pub fn new(name: &'static str, offers: Vec<Offer<T>>, chosen: usize) -> Self {
        Self {
            name,
            chosen: chosen.min(offers.len().saturating_sub(1)),
            offers,
            open: false,
            filtering: false,
            query: String::new(),
            at: 0,
            search_from: SEARCH_FROM,
        }
    }

    pub fn chosen_offer(&self) -> Option<&Offer<T>> {
        self.offers.get(self.chosen)
    }

    pub fn chosen_value(&self) -> Option<&T> {
        self.chosen_offer().map(|offer| &offer.value)
    }

    pub fn label(&self) -> &str {
        self.chosen_offer().map_or("", |offer| offer.label.as_str())
    }

    pub fn choose(&mut self, index: usize) {
        if index < self.offers.len() {
            self.chosen = index;
        }
    }

    pub fn offer_choices(&mut self, offers: Vec<Offer<T>>) {
        self.offers = offers;
        self.chosen = self.chosen.min(self.offers.len().saturating_sub(1));
        self.at = 0;
        self.query.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.offers.is_empty()
    }

    pub fn mouse(&mut self, mouse: MouseEvent, rows: &[(Rect, usize)]) -> bool {
        if !self.open {
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll(true);
                true
            }
            MouseEventKind::ScrollDown => {
                self.scroll(false);
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((_, index)) = rows.iter().find(|(area, _)| {
                    mouse.column >= area.x
                        && mouse.column < area.right()
                        && mouse.row >= area.y
                        && mouse.row < area.bottom()
                }) else {
                    return false;
                };
                self.choose(*index);
                self.close();
                true
            }
            _ => false,
        }
    }

    pub fn hints(&self) -> Vec<chrome::Hint> {
        if !self.open {
            return Vec::new();
        }
        if self.filtering {
            return vec![
                chrome::Hint::new("type", "narrow"),
                chrome::Hint::new("↑/↓", "choose"),
                chrome::Hint::new("enter", "take it"),
                chrome::Hint::new("esc", "clear"),
            ];
        }
        let mut hints = vec![chrome::Hint::new("j/k · ↑/↓", "choose")];
        if self.is_searchable() {
            hints.push(chrome::Hint::new("/", "filter"));
        }
        hints.push(chrome::Hint::new("enter", "take it"));
        hints.push(chrome::Hint::new("esc", "close"));
        hints
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn is_searchable(&self) -> bool {
        self.offers.len() >= self.search_from
    }

    pub fn is_filtering(&self) -> bool {
        self.open && self.filtering
    }

    pub fn open(&mut self) {
        self.open = true;
        self.filtering = false;
        self.query.clear();
        self.at = self.chosen;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.filtering = false;
        self.query.clear();
        self.at = 0;
    }

    fn leave_filter(&mut self) {
        self.filtering = false;
        self.query.clear();
        self.at = self.chosen.min(self.offers.len().saturating_sub(1));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if !self.open {
            return false;
        }
        let matches = self.matches();
        match key.code {
            // Escape leaves the filter before it leaves the list, so one press
            // puts the whole list back and a second closes it.
            KeyCode::Esc if self.filtering => self.leave_filter(),
            KeyCode::Esc => self.close(),
            // A list that is not being typed at leaves the way everything else
            // does.
            KeyCode::Char('q') if !self.filtering => self.close(),
            KeyCode::Char('/') if self.is_searchable() && !self.filtering => {
                self.filtering = true;
                self.at = self.at.min(matches.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(index) = matches.get(self.at)
                    && self.offers[*index].ready
                {
                    self.chosen = *index;
                    self.close();
                }
            }
            // Movement is movement: the vim keys travel until `/` says the list
            // is being typed at.
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab if !self.filtering => {
                self.step(true, matches.len());
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab if !self.filtering => {
                self.step(false, matches.len());
            }
            // The arrows still travel while typing, because a filter that has to
            // be left before its matches can be read is not a filter.
            KeyCode::Down | KeyCode::Tab => self.step(true, matches.len()),
            KeyCode::Up | KeyCode::BackTab => self.step(false, matches.len()),
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.step(true, matches.len());
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.step(false, matches.len());
            }
            KeyCode::Backspace if self.filtering => {
                self.query.pop();
                self.at = 0;
            }
            // A chord is not text, and a query is one line, so left, right and
            // the delete key do nothing rather than move a caret this list does
            // not have.
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
                // Typing narrows the list without being announced first: a picker
                // whose list is long is one somebody types into, and asking them to
                // find `/` for that is asking them to read the hints to use a list.
                // The movement keys are taken above, so this is text.
                if !self.filtering && self.is_searchable() {
                    self.filtering = true;
                    self.query.clear();
                    self.at = 0;
                }
                if self.filtering {
                    self.query.push(character);
                    self.at = 0;
                }
            }
            _ => {}
        }
        true
    }

    pub fn scroll(&mut self, forward: bool) {
        if self.open {
            let matches = self.matches();
            self.step(forward, matches.len());
        }
    }

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

    fn matches(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.offers
            .iter()
            .enumerate()
            .filter(|(_, offer)| query.is_empty() || offer.label.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    pub fn pill_width(&self, with_name: bool) -> u16 {
        (self.name_width(with_name) + self.label().chars().count() + 3) as u16
    }

    fn name_width(&self, with_name: bool) -> usize {
        if with_name {
            self.name.chars().count() + 2
        } else {
            1
        }
    }

    pub fn draw_pill(&self, frame: &mut Frame, area: Rect, focused: bool, with_name: bool) {
        self.draw_pill_in(frame.buffer_mut(), area, focused, with_name);
    }

    pub fn draw_pill_in(&self, buffer: &mut Buffer, area: Rect, focused: bool, with_name: bool) {
        pill_in(
            buffer,
            area,
            &self.pill_spans(focused, with_name, usize::from(area.width)),
            focused,
        );
    }

    pub fn pill_spans(&self, focused: bool, with_name: bool, room: usize) -> Vec<Span<'static>> {
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
            room.saturating_sub(self.name_width(with_name) + 3),
        );
        let mut spans = Vec::new();
        if with_name {
            spans.push(Span::styled(format!(" {} ", self.name), quiet));
        } else {
            spans.push(Span::styled(" ", quiet));
        }
        spans.push(Span::styled(value, value_style));
        spans.push(Span::styled(format!(" {OPENS} "), quiet));
        spans
    }

    pub fn paste(&mut self, text: &str) {
        if self.filtering {
            self.query.push_str(&text.replace(['\r', '\n'], " "));
            self.at = 0;
        }
    }

    pub fn draw_list(&self, frame: &mut Frame, area: Rect, anchor: Rect) -> Vec<(Rect, usize)> {
        self.draw_list_in(frame, area, anchor, true)
    }

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
        let filtering = self.filtering;
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
            .max(if filtering { 20 } else { 0 })
            .min(usize::from(area.width)) as u16;
        if area.height < 3 {
            return Vec::new();
        }
        let height = (matches.len() as u16 + 2 + u16::from(filtering))
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
            Layout::vertical([Constraint::Length(u16::from(filtering)), Constraint::Min(0)])
                .areas(inner);
        if filtering {
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
