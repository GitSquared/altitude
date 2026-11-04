mod layout;
mod row;

pub use row::Row;

use std::{collections::HashSet, time::Duration};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{Frame, layout::Rect, text::Line, widgets::Paragraph};

use crate::tui::widgets::{format::truncate, palette};
use layout::{Layout, bar, indent_of, visible};
use row::heading_line;

// Where a row landed, and whether the pointer found a name in it that points
// somewhere. A name the terminal cannot make clickable is still a name a click
// can follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub row: usize,
    pub link: Option<String>,
}

// What a key did, so a screen can decide what it means: a list already at its top
// hands the focus back to whatever is above it, and a fold is the screen's to
// keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Moved,
    AtStart,
    AtEnd,
    Folded { title: String, folded: bool },
}

#[derive(Default)]
pub struct List {
    cursor: usize,
    offset: usize,
    target: usize,
    folded: HashSet<String>,
    layout: Layout,
}

impl List {
    pub fn selected(&self) -> usize {
        self.cursor
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn select(&mut self, index: usize) {
        self.cursor = index;
    }

    pub fn reset(&mut self) {
        self.cursor = 0;
        self.offset = 0;
        self.target = 0;
    }

    pub fn content_lines(&self) -> usize {
        self.layout.content_lines()
    }

    pub fn is_gliding(&self) -> bool {
        self.offset != self.target
    }

    pub fn glide(&mut self, elapsed: Duration) -> bool {
        if self.offset == self.target {
            return false;
        }
        let gap = self.offset.abs_diff(self.target);
        let fraction = 1.0 - 0.5_f64.powf(elapsed.as_secs_f64() / 0.08);
        let step = ((gap as f64 * fraction) as usize).max(1);
        self.offset = if self.offset < self.target {
            (self.offset + step).min(self.target)
        } else {
            self.offset.saturating_sub(step).max(self.target)
        };
        true
    }

    pub fn move_by(&mut self, forward: bool) -> bool {
        let rows: Vec<usize> = self.layout.rows().map(|(_, row)| row).collect();
        match rows.iter().position(|row| *row == self.cursor) {
            Some(at) => {
                let next = if forward {
                    rows.get(at + 1)
                } else {
                    at.checked_sub(1).and_then(|index| rows.get(index))
                };
                match next {
                    Some(row) => {
                        self.cursor = *row;
                        true
                    }
                    None => false,
                }
            }
            // The selection is inside a folded group: the first row still on
            // screen is where the operator's eye already is.
            None => match rows.first() {
                Some(row) => {
                    self.cursor = *row;
                    true
                }
                None => false,
            },
        }
    }

    pub fn page_by(&mut self, forward: bool) -> bool {
        let band = usize::from(self.layout.height).max(1);
        let Some(top) = self.layout.top_of(self.cursor) else {
            return false;
        };
        let rows: Vec<(usize, usize)> = self.layout.rows().collect();
        let target = if forward {
            rows.iter()
                .filter(|(at, _)| *at <= top.saturating_add(band))
                .map(|(_, row)| *row)
                .next_back()
        } else {
            rows.iter()
                .filter(|(at, _)| *at >= top.saturating_sub(band))
                .map(|(_, row)| *row)
                .next()
        };
        let Some(target) = target else {
            return false;
        };
        let moved = target != self.cursor;
        self.cursor = target;
        moved
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Event> {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => Some(if self.move_by(true) {
                Event::Moved
            } else {
                Event::AtEnd
            }),
            KeyCode::Up | KeyCode::Char('k') => Some(if self.move_by(false) {
                Event::Moved
            } else {
                Event::AtStart
            }),
            KeyCode::PageDown => Some(if self.page_by(true) {
                Event::Moved
            } else {
                Event::AtEnd
            }),
            KeyCode::PageUp => Some(if self.page_by(false) {
                Event::Moved
            } else {
                Event::AtStart
            }),
            KeyCode::Home => {
                if let Some((_, row)) = self.layout.rows().next() {
                    self.cursor = row;
                }
                Some(Event::Moved)
            }
            KeyCode::End => {
                if let Some((_, row)) = self.layout.rows().last() {
                    self.cursor = row;
                }
                Some(Event::Moved)
            }
            KeyCode::Char('h') | KeyCode::Left => self.fold_at_cursor(false),
            KeyCode::Char('l') | KeyCode::Right => self.fold_at_cursor(true),
            KeyCode::Char(' ') => self.toggle_at_cursor(),
            _ => None,
        }
    }

    // Folding is asked for in a direction — h closes the group the cursor is in, l
    // opens it — so the key says what it wants rather than what went before.
    fn fold_at_cursor(&mut self, open: bool) -> Option<Event> {
        let (title, folded) = self.layout.group_of(self.cursor)?;
        if open == !folded {
            return None;
        }
        self.set_fold(title.to_owned(), !open)
    }

    fn toggle_at_cursor(&mut self) -> Option<Event> {
        let (title, folded) = self.layout.group_of(self.cursor)?;
        self.set_fold(title.to_owned(), !folded)
    }

    fn set_fold(&mut self, title: String, folded: bool) -> Option<Event> {
        if folded {
            self.folded.insert(title.clone());
        } else {
            self.folded.remove(&title);
        }
        Some(Event::Folded { title, folded })
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        rows: &[Row],
        tick: Option<usize>,
        focused: bool,
        gutter: u16,
    ) -> Vec<(Rect, Hit)> {
        let gutter = gutter.min(area.width / 2);
        let list_area = Rect {
            x: area.x.saturating_add(gutter),
            width: area.width.saturating_sub(gutter * 2),
            ..area
        };
        let indent = indent_of(rows);
        let width = usize::from(list_area.width);
        self.layout = Layout::of(rows, &self.folded, width, list_area.height);
        if self.layout.top_of(self.cursor).is_none() {
            // The list has changed shape under the cursor: the nearest row still
            // on screen is where the operator's eye already is.
            self.cursor = self
                .layout
                .units
                .iter()
                .map(|unit| unit.index)
                .min_by_key(|row| row.abs_diff(self.cursor))
                .unwrap_or(0);
        }
        self.follow(tick.is_some());

        // The bar goes under the row it is on and nowhere else: every other row
        // leaves the cells between its words to the field.
        let mut hitboxes = Vec::new();
        for unit in &self.layout.units {
            let (start, height) = unit.own();
            let Some(rect) = visible(area, start, usize::from(height.max(1)), self.offset) else {
                continue;
            };
            hitboxes.push((
                rect,
                Hit {
                    row: unit.index,
                    link: None,
                },
            ));
            // Each name in the row is its own target, laid where the row's own
            // line put it: a click on ALT-6 opens the issue rather than selecting
            // the row it happens to sit in.
            let row = &rows[unit.index];
            for (rect, url) in self.reference_targets(list_area, row, unit) {
                hitboxes.push((
                    rect,
                    Hit {
                        row: unit.index,
                        link: Some(url),
                    },
                ));
            }
            if unit.index == self.cursor {
                bar(
                    frame,
                    rect,
                    if focused {
                        palette::focus()
                    } else {
                        palette::inactive_selection()
                    },
                );
            }
        }

        for unit in &self.layout.units {
            let row = &rows[unit.index];
            if let Some(heading) = &row.heading {
                self.put(
                    frame,
                    list_area,
                    unit.top,
                    heading_line(heading, unit.folded),
                );
                // A folded group has to say how much it is hiding; an open one is
                // counted by the rows under it.
                if unit.folded
                    && let Some(index) = self.layout.grid.column_of(layout::Cell::Note)
                    && let Some(y) = self.line_on_screen(list_area, unit.top)
                {
                    let grid = &self.layout.grid;
                    let start = grid.start(index).min(usize::from(list_area.width));
                    let room = grid.rooms.get(index).copied().unwrap_or(0);
                    let note = truncate(&heading.note, room);
                    let width = u16::try_from(row::width_of(&note)).unwrap_or(0);
                    frame.render_widget(
                        Paragraph::new(Line::styled(note, palette::muted())),
                        Rect {
                            x: list_area
                                .x
                                .saturating_add(u16::try_from(start).unwrap_or(0)),
                            y,
                            width,
                            height: 1,
                        },
                    );
                }
            }
            if unit.folded {
                continue;
            }
            let selected = unit.index == self.cursor;
            let first = unit.own().0;
            for (offset, line) in row
                .lines(
                    indent,
                    &self.layout.grid,
                    tick,
                    focused && selected,
                    selected,
                )
                .into_iter()
                .enumerate()
            {
                self.put(frame, list_area, first + offset, line);
            }
        }
        hitboxes
    }

    // A line of the list, painted where the layout put it and only as wide as it
    // has something to say: what is not written is the field showing through.
    fn put(&self, frame: &mut Frame, list_area: Rect, line_index: usize, line: Line<'static>) {
        let Some(y) = self.line_on_screen(list_area, line_index) else {
            return;
        };
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                x: list_area.x,
                y,
                width: list_area.width,
                height: 1,
            },
        );
    }

    pub fn title_room(&self) -> usize {
        self.layout.grid.title
    }

    // Each column's name, starting where its values start: the table is drawn
    // from the same grid, so the name is over its column by construction.
    pub fn headers(&self, frame: &mut Frame, area: Rect, gutter: u16, labels: &[&str]) {
        let gutter = gutter.min(area.width / 2);
        let list_area = Rect {
            x: area.x.saturating_add(gutter),
            width: area.width.saturating_sub(gutter * 2),
            ..area
        };
        let grid = &self.layout.grid;
        for (index, label) in labels.iter().enumerate() {
            let Some(room) = grid.rooms.get(index) else {
                break;
            };
            let label = truncate(label, *room);
            let width = row::width_of(&label);
            if width == 0 {
                continue;
            }
            let start = grid.start(index).min(usize::from(list_area.width));
            let x = if grid.cells.get(index).is_some_and(|cell| cell.is_right()) {
                start.saturating_add(room.saturating_sub(width))
            } else {
                start
            };
            frame.render_widget(
                Paragraph::new(Line::styled(label, palette::muted())),
                Rect {
                    x: list_area.x.saturating_add(u16::try_from(x).unwrap_or(0)),
                    y: area.y,
                    width: u16::try_from(width).unwrap_or(0),
                    height: 1,
                },
            );
        }
    }

    // Where the names in a row landed, for the pointer. The pointer is told the
    // cells the row drew its names in, so a click follows the name it is on.
    fn reference_targets(
        &self,
        list_area: Rect,
        row: &Row,
        unit: &layout::Unit,
    ) -> Vec<(Rect, String)> {
        let grid = &self.layout.grid;
        let Some(y) = self.line_on_screen(list_area, unit.own().0) else {
            return Vec::new();
        };
        let mut targets = Vec::new();
        for (index, cell) in grid.cells.iter().enumerate() {
            let layout::Cell::Reference(at) = cell else {
                continue;
            };
            let Some(reference) = row.references.get(*at) else {
                continue;
            };
            let Some(url) = &reference.url else {
                continue;
            };
            let Some(room) = grid.rooms.get(index) else {
                continue;
            };
            let text = row::width_of(&reference.text).min(*room);
            if text == 0 {
                continue;
            }
            let x = grid.start(index).min(usize::from(list_area.width));
            targets.push((
                Rect {
                    x: list_area
                        .x
                        .saturating_add(u16::try_from(x).unwrap_or(u16::MAX)),
                    y,
                    width: u16::try_from(text).unwrap_or(0),
                    height: 1,
                },
                url.clone(),
            ));
        }
        targets
    }

    fn line_on_screen(&self, list_area: Rect, line_index: usize) -> Option<u16> {
        let y = line_index.checked_sub(self.offset)?;
        (y < usize::from(list_area.height)).then(|| {
            list_area
                .y
                .saturating_add(u16::try_from(y).unwrap_or(u16::MAX))
        })
    }

    fn follow(&mut self, animate: bool) {
        let band = usize::from(self.layout.height).max(1);
        let last = self.layout.content_lines().saturating_sub(band);
        let Some(top) = self.layout.top_of(self.cursor) else {
            self.offset = 0;
            self.target = 0;
            return;
        };
        let content = usize::from(self.layout.height_of(self.cursor).max(1));
        self.target = if content >= band || top < self.offset {
            top
        } else if top + content > self.offset + band {
            (top + content).saturating_sub(band)
        } else {
            self.offset
        }
        .min(last);
        if !animate {
            self.offset = self.target;
        }
    }
}
