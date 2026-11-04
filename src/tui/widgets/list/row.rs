use std::mem;

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::layout::{Cell, Grid};
use crate::tui::widgets::{format::truncate, palette, shimmer::shimmer};

// A title that needs more than this is a title that should have been written
// shorter; the rest of it is the detail screen's.
pub(super) const TITLE_LINES: usize = 3;
// A group's mark gets a column of its own and the rows sit under the label, so
// the group reads as a block rather than as lines that happen to follow a
// heading.
pub(super) const MARK_COLUMN: usize = 2;
// A short fact that belongs in a column: an identifier the eye runs down rather
// than a sentence it reads across. When it names a place, the terminal is told so
// and the name becomes a way in rather than a label.
#[derive(Clone)]
pub struct Reference {
    pub text: String,
    pub url: Option<String>,
}

pub struct Heading {
    pub title: String,
    pub mark: char,
    pub tone: Style,
    pub note: String,
}

// Two lines a row: what it is, then what is true of it. The second line carries
// the facts a person came to the list to compare, so a row is worth its height
// wherever it is drawn.
pub struct Row {
    pub heading: Option<Heading>,
    pub title: String,
    pub detail: Option<String>,
    pub references: Vec<Reference>,
    pub age: Option<String>,
    pub note: Option<String>,
    pub mark: Vec<Span<'static>>,
    pub tone: Style,
    pub shimmer: bool,
}

impl Row {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            heading: None,
            title: title.into(),
            detail: None,
            references: Vec::new(),
            age: None,
            note: None,
            mark: Vec::new(),
            tone: palette::primary(),
            shimmer: false,
        }
    }

    // The mark and the count belong to the group, so they are drawn once here
    // rather than on every row under it.
    pub fn heading(
        mut self,
        title: impl Into<String>,
        note: impl Into<String>,
        mark: char,
        tone: Style,
    ) -> Self {
        self.heading = Some(Heading {
            title: title.into(),
            mark,
            tone,
            note: note.into(),
        });
        self
    }

    // What the row is about: the fact that would change what the operator does.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    // Where the work lives: the issue it answers, the pull request it opened, the
    // machine holding it. Names rather than words, so they are drawn in the accent
    // and given columns of their own.
    pub fn reference(mut self, text: impl Into<String>) -> Self {
        self.references.push(Reference {
            text: text.into(),
            url: None,
        });
        self
    }

    // A reference that is also somewhere to go.
    pub fn link(mut self, text: impl Into<String>, url: impl Into<String>) -> Self {
        self.references.push(Reference {
            text: text.into(),
            url: Some(url.into()),
        });
        self
    }

    // How long ago the row last said something new, as the list reckons it: the one
    // column that reads towards the border rather than away from it.
    pub fn age(mut self, age: impl Into<String>) -> Self {
        let age = age.into();
        if !age.is_empty() {
            self.age = Some(age);
        }
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn mark(mut self, mark: Vec<Span<'static>>) -> Self {
        self.mark = mark;
        self
    }

    pub fn tone(mut self, tone: Style) -> Self {
        self.tone = tone;
        self
    }

    pub fn shimmer(mut self) -> Self {
        self.shimmer = true;
        self
    }

    pub(super) fn mark_width(&self) -> usize {
        self.mark.iter().map(Span::width).sum()
    }

    fn leading(&self, indent: usize) -> usize {
        indent + self.mark_width()
    }

    // The title's column is the table's first cell, so the mark and the group's
    // indent are taken off its width rather than off the line's.
    pub(super) fn title_room(&self, indent: usize, grid: &Grid) -> usize {
        grid.title.saturating_sub(self.leading(indent)).max(1)
    }

    fn title_lines(&self, room: usize) -> Vec<String> {
        wrap(&self.title, room, room, TITLE_LINES)
    }

    // The sentence the row is about: the agent's question, what it is doing, why
    // it stopped. One line, cut where the room ends.
    fn sentence(&self, room: usize, focused: bool, selected: bool) -> Option<Line<'static>> {
        // A row the bar is under carries its own colours, so the line reads the
        // same on the bar as it does on the field.
        let quiet = if focused {
            palette::focus()
        } else if selected {
            palette::selection_detail()
        } else {
            palette::muted()
        };
        let detail = self.detail.as_deref()?;
        (!detail.is_empty()).then(|| Line::from(Span::styled(truncate(detail, room), quiet)))
    }

    pub(super) fn height(&self, indent: usize, grid: &Grid) -> u16 {
        let titles = self.title_lines(self.title_room(indent, grid)).len();
        let facts = usize::from(self.detail.is_some());
        u16::try_from((titles + facts).max(1)).unwrap_or(u16::MAX)
    }

    // Only what the row says: no padding, so the field shows between the words of
    // a row that is not the one being read.
    pub(super) fn lines(
        &self,
        indent: usize,
        grid: &Grid,
        tick: Option<usize>,
        focused: bool,
        selected: bool,
    ) -> Vec<Line<'static>> {
        let lead = " ".repeat(indent);
        let mark_fill = " ".repeat(self.mark_width());
        let room = self.title_room(indent, grid);
        let mut lines = Vec::with_capacity(2);
        for (index, title) in self.title_lines(room).into_iter().enumerate() {
            let mut spans = Vec::new();
            if index == 0 {
                spans.push(Span::raw(lead.clone()));
                spans.extend(self.mark.iter().cloned());
            } else {
                spans.push(Span::raw(format!("{lead}{mark_fill}")));
            }
            spans.extend(self.title_spans(&title, tick, focused, selected));
            // The title holds its column on every line, so the names beside it
            // land on the same place in every row of the list.
            spans.push(Span::raw(" ".repeat(room.saturating_sub(width_of(&title)))));
            if index == 0 {
                spans.extend(self.cells(grid, focused, selected));
            }
            lines.push(Line::from(spans));
        }
        let sentence = grid.width.saturating_sub(self.leading(indent)).max(1);
        if let Some(sentence) = self.sentence(sentence, focused, selected) {
            let mut spans = vec![Span::raw(format!("{lead}{mark_fill}"))];
            spans.extend(sentence.spans);
            lines.push(Line::from(spans));
        }
        lines
    }

    // The row's facts, in the cells the whole list agreed on: a column is a column
    // because every row puts its value in the same place, and a row without one
    // leaves the cell empty rather than shifting the rest along.
    fn cells(&self, grid: &Grid, focused: bool, selected: bool) -> Vec<Span<'static>> {
        let accent = if focused {
            palette::focus()
        } else if selected {
            palette::selection_detail()
        } else {
            palette::accent()
        };
        let quiet = note_style(focused, selected);
        let mut spans = Vec::with_capacity(grid.cells.len());
        let last = grid.cells.len().saturating_sub(1);
        for (index, (cell, room)) in grid.cells.iter().zip(&grid.rooms).enumerate() {
            let (text, style) = match cell {
                Cell::Reference(index) => (
                    self.references
                        .get(*index)
                        .map_or("", |reference| reference.text.as_str()),
                    accent,
                ),
                Cell::Age => (self.age.as_deref().unwrap_or(""), quiet),
                Cell::Note => (self.note.as_deref().unwrap_or(""), quiet),
            };
            let text = truncate(text, *room);
            let padding = " ".repeat(room.saturating_sub(width_of(&text)));
            // The column of air is the cell's as much as its text is: the next
            // cell starts after it, on a line drawn from the same grid.
            let air = if index == last { "" } else { " " };
            let line = if cell.is_right() {
                format!("{padding}{text}{air}")
            } else {
                format!("{text}{padding}{air}")
            };
            spans.push(Span::styled(line, style));
        }
        spans
    }

    fn title_spans(
        &self,
        title: &str,
        tick: Option<usize>,
        focused: bool,
        selected: bool,
    ) -> Vec<Span<'static>> {
        if self.shimmer && !focused && !selected {
            return shimmer(title, tick);
        }
        let style = if focused {
            palette::focus()
        } else if selected {
            palette::inactive_selection()
        } else {
            self.tone
        };
        vec![Span::styled(title.to_owned(), style)]
    }
}

pub(super) fn note_style(focused: bool, selected: bool) -> Style {
    if focused {
        palette::focus()
    } else if selected {
        palette::selection_detail()
    } else {
        palette::muted()
    }
}

pub(super) fn heading_line(heading: &Heading, folded: bool) -> Line<'static> {
    let caret = if folded { " ▸" } else { " ▾" };
    Line::from(vec![
        Span::styled(format!("{} ", heading.mark), heading.tone),
        // The fold indicator follows the label rather than leading it: a caret
        // in the left column would take the row's first column on every screen,
        // and the narrow ones cannot spare it.
        Span::styled(
            heading.title.clone(),
            heading.tone.add_modifier(Modifier::BOLD),
        ),
        Span::styled(caret, palette::muted()),
    ])
}

pub(super) fn width_of(text: &str) -> usize {
    Line::from(text).width()
}

// Greedy wrapping: `first` is the room on the line carrying the note, `rest` the
// room on the lines under it, `max` the lines the text may take before it is cut
// with an ellipsis.
fn wrap(text: &str, first: usize, rest: usize, max: usize) -> Vec<String> {
    let first = first.max(1);
    let rest = rest.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut room = first;
    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let gap = usize::from(!line.is_empty());
            if width_of(&line) + gap + width_of(word) <= room {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                break;
            }
            if line.is_empty() {
                let split = word
                    .char_indices()
                    .nth(room)
                    .map_or(word.len(), |(index, _)| index);
                let (head, tail) = word.split_at(split);
                lines.push(head.to_owned());
                room = rest;
                word = tail;
                if word.is_empty() {
                    break;
                }
                continue;
            }
            lines.push(mem::take(&mut line));
            room = rest;
        }
    }
    lines.push(line);
    if lines.len() > max {
        lines.truncate(max);
        let room = if max == 1 { first } else { rest };
        if let Some(last) = lines.last_mut() {
            *last = truncate(last, room);
        }
    }
    lines
}
