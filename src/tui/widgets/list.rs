//! Lists that say what each row is, and the cursor that moves among them.
//!
//! A row is a small block rather than a line: an optional mark and note beside
//! the name, and an optional detail under it. The focus bar covers the whole
//! block, because what is selected is the row rather than its name. The pieces
//! are placed the same way in every list that uses one: mark at the left edge,
//! note at the right edge so a column of notes lines up.
//!
//! A row can start a group with a section heading. Rows may then differ in
//! height, so the cursor measures the list when it decides how many fit. Every
//! row is followed by a blank line, so blocks read apart rather than as one
//! paragraph.

use std::time::Duration;

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem},
};

use crate::tui::widgets::{palette, utils::truncate};

pub struct Heading {
    pub title: String,
    pub note: Option<String>,
    /// The mark before the title and the tone both it and the title take.
    /// A group can use one place for what every row in it shares.
    pub mark: Vec<Span<'static>>,
    pub tone: Style,
}

pub struct Row {
    pub heading: Option<Heading>,
    pub title: String,
    pub detail: Option<String>,
    pub note: Option<String>,
    /// The mark in front of the first line, where the row has a status of its
    /// own. Its spans keep their own styles; only their width is measured here.
    pub mark: Vec<Span<'static>>,
    pub tone: Style,
}

impl Row {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            heading: None,
            detail: None,
            note: None,
            mark: Vec::new(),
            tone: palette::primary(),
        }
    }

    pub fn heading(mut self, title: impl Into<String>, note: impl Into<String>) -> Self {
        self.heading = Some(Heading {
            title: title.into(),
            note: Some(note.into()),
            mark: Vec::new(),
            tone: palette::heading(),
        });
        self
    }

    pub fn heading_kind(mut self, mark: Vec<Span<'static>>, tone: Style) -> Self {
        if let Some(heading) = &mut self.heading {
            heading.mark = mark;
            heading.tone = tone;
        }
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
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

    fn height(&self) -> u16 {
        u16::from(self.heading.is_some()) + 1 + u16::from(self.detail.is_some())
    }

    fn lines(&self, width: usize, focused: bool, selected: bool) -> Vec<Line<'static>> {
        let mark = self
            .mark
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let indent = Line::from(mark.as_str()).width();
        let note = self.note.as_deref().unwrap_or_default();
        let note_width = Line::from(note).width();
        // On an inactive selection the muted shade and the bar behind it are the
        // same colour, so the secondary lines come up to the primary tone rather
        // than disappearing into it.
        let secondary = if selected {
            palette::selection_detail()
        } else {
            palette::muted()
        };
        let mut lines = Vec::with_capacity(3);
        // The mark sits at the left edge. The lines under it align with the name
        // so a list of marked rows still reads as a column.
        let available = width
            .saturating_sub(indent)
            .saturating_sub(note_width)
            .saturating_sub(usize::from(!note.is_empty()))
            .max(1);
        let title = truncate(&self.title, available);
        let used = indent + Line::from(title.as_str()).width() + note_width;
        let padding = " ".repeat(width.saturating_sub(used));
        if focused {
            // The whole row is one bar, not just the line with the name in it:
            // what is selected is the block, and every line of it belongs to it.
            // The mark keeps its own colour above the fill, so a travelling dot
            // does not disappear when the row is selected.
            let mut spans = self.mark.clone();
            spans.push(Span::styled(
                format!("{title}{padding}{note}"),
                palette::focus(),
            ));
            lines.push(Line::from(spans));
        } else {
            // The name carries its tone itself rather than inheriting it from the
            // line, so what is behind a row cannot decide how the row reads. The
            // padding does inherit the selection, because on an inactive bar it
            // is the bar that has to continue across the row.
            let mut spans = self.mark.clone();
            spans.push(Span::styled(
                title,
                if selected {
                    palette::inactive_selection()
                } else {
                    self.tone
                },
            ));
            spans.push(padding_span(&padding, selected));
            if !note.is_empty() {
                spans.push(Span::styled(note.to_owned(), secondary));
            }
            lines.push(Line::from(spans));
        }
        if let Some(detail) = &self.detail {
            let available = width.saturating_sub(indent).max(1);
            let detail = truncate(detail, available);
            let used = indent + Line::from(detail.as_str()).width();
            let padding = " ".repeat(width.saturating_sub(used));
            if focused {
                // The fill is the row's, but the detail is not its name: the
                // quieter foreground keeps the line under the title rather than
                // turning the whole bar into one weight.
                lines.push(Line::from(Span::styled(
                    format!("{}{detail}{padding}", " ".repeat(indent)),
                    palette::focus_detail(),
                )));
            } else {
                lines.push(Line::from(vec![
                    padding_span(&" ".repeat(indent), selected),
                    Span::styled(detail, secondary),
                    padding_span(&padding, selected),
                ]));
            }
        }
        lines
    }
}

/// Where the cursor is, and which row the list starts at.
///
/// The two are separate because the view moves on its own clock: the selection
/// changes on a key, and the rows follow it one frame at a time, so a jump reads
/// as the list moving rather than the content changing.
#[derive(Default)]
pub struct Cursor {
    selected: usize,
    offset: usize,
    target: usize,
}

impl Cursor {
    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Put the cursor on a row, keeping the view where it is until it is told to
    /// follow.
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    /// Start again at the top, with no scroll to carry over. Used when the list
    /// under the cursor is a different list.
    pub fn reset(&mut self) {
        self.selected = 0;
        self.offset = 0;
        self.target = 0;
    }

    pub fn move_by(&mut self, forward: bool, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if forward {
            (self.selected + 1) % len
        } else {
            (self.selected + len - 1) % len
        };
    }

    /// Point the view at the row it should start from so the selection is inside
    /// `visible` rows.
    ///
    /// With animations off there is no clock to glide on, so the view is put
    /// there at once.
    pub fn follow(&mut self, len: usize, visible: usize, animate: bool) {
        self.selected = self.selected.min(len.saturating_sub(1));
        let visible = visible.max(1);
        self.target = if self.selected < self.offset {
            self.selected
        } else if self.selected >= self.offset + visible {
            self.selected + 1 - visible
        } else {
            self.offset
        }
        .min(len.saturating_sub(visible));
        if !animate {
            self.offset = self.target;
        }
    }

    pub fn is_gliding(&self) -> bool {
        self.offset != self.target
    }

    /// Move the view toward where it should be, easing out so a long jump does
    /// not crawl. Answers whether anything moved, which is what asks for a frame.
    ///
    /// The step is measured in elapsed time, not in frames, so the faster loop
    /// eases toward the target instead of jumping there.
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

    /// How many rows of one uniform height fit in the area. Used by lists whose
    /// rows all have the same shape; see [`visible`] for mixed ones.
    pub fn visible(height: u16, row_height: u16) -> usize {
        usize::from((height / row_height.max(1)).max(1))
    }
}

pub fn row_height(rows: &[Row]) -> u16 {
    rows.first().map_or(2, |row| row.height() + 1)
}

pub fn visible(height: u16, rows: &[Row]) -> usize {
    let mut used = 0;
    let mut count = 0;
    for row in rows {
        let row_height = row.height() + 1;
        if used + row_height > height {
            break;
        }
        used += row_height;
        count += 1;
    }
    count.max(1)
}

/// Draw the rows from `offset` down.
///
/// `selected` is the row the detail follows, which keeps an inactive bar when
/// the focus is elsewhere; `focused` is the row the keys are on. `gutter` is the
/// horizontal padding inside the list: text is kept clear of it, but the bars run
/// through it, so a row reads as the whole block rather than a line floating in
/// the middle.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    rows: &[Row],
    offset: usize,
    selected: Option<usize>,
    focused: Option<usize>,
    gutter: u16,
) -> Vec<(Rect, usize)> {
    let gutter = gutter.min(area.width / 2);
    let list_area = Rect {
        x: area.x.saturating_add(gutter),
        width: area.width.saturating_sub(gutter * 2),
        ..area
    };
    // The field is under the list. Clear each row's block so the contours stay
    // out of the text; they remain visible in the gaps between rows and below
    // the list, which is where a backdrop belongs.
    let mut top = area.y;
    let mut hitboxes = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(offset) {
        if top >= area.bottom() {
            break;
        }
        let height = row.height().min(area.bottom() - top);
        let rect = Rect {
            x: area.x,
            y: top,
            width: area.width,
            height,
        };
        frame.render_widget(Clear, rect);
        hitboxes.push((rect, index));
        top = top.saturating_add(row.height() + 1);
    }
    if let Some(index) = selected.filter(|index| focused != Some(*index)) {
        bar(
            frame,
            area,
            rows,
            offset,
            index,
            palette::inactive_selection(),
        );
    }
    if let Some(index) = focused {
        bar(frame, area, rows, offset, index, palette::focus());
    }
    let width = usize::from(list_area.width);
    let items = rows
        .iter()
        .enumerate()
        .skip(offset)
        .map(|(index, row)| {
            let mut lines = Vec::new();
            if let Some(heading) = &row.heading {
                lines.push(heading_line(heading, width));
            }
            lines.extend(row.lines(width, focused == Some(index), selected == Some(index)));
            lines.push(Line::default());
            ListItem::new(lines)
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), list_area);
    hitboxes
}

fn bar(frame: &mut Frame, area: Rect, rows: &[Row], offset: usize, index: usize, style: Style) {
    if index < offset || index >= rows.len() {
        return;
    }
    let heading = u16::from(rows[index].heading.is_some());
    let content_height = rows[index].height().saturating_sub(heading).max(1);
    let top = area.y.saturating_add(
        rows[offset..index]
            .iter()
            .map(|row| row.height() + 1)
            .sum::<u16>(),
    ) + heading;
    let bottom = top.saturating_add(content_height).min(area.bottom());
    if top >= bottom {
        return;
    }
    let bar = Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: bottom - top,
    };
    frame.render_widget(Clear, bar);
    frame.render_widget(Block::default().style(style), bar);
}

fn heading_line(heading: &Heading, width: usize) -> Line<'static> {
    let note = heading.note.as_deref().unwrap_or_default();
    let note_width = Line::from(note).width();
    let mark_width = heading.mark.iter().map(|span| span.width()).sum::<usize>();
    let title = truncate(
        &heading.title,
        width
            .saturating_sub(mark_width)
            .saturating_sub(note_width)
            .saturating_sub(usize::from(!note.is_empty()))
            .max(1),
    );
    let used = mark_width + Line::from(title.as_str()).width() + note_width;
    let padding = " ".repeat(width.saturating_sub(used));
    let mut spans = heading.mark.clone();
    spans.push(Span::styled(title, heading.tone));
    spans.push(Span::raw(padding));
    spans.push(Span::styled(note.to_owned(), palette::muted()));
    Line::from(spans)
}

/// Padding inside an inactive selection keeps the bar's fill. Outside one there
/// is nothing to continue, so it is plain space.
fn padding_span(spaces: &str, selected: bool) -> Span<'static> {
    if selected {
        Span::styled(spaces.to_owned(), palette::inactive_selection())
    } else {
        Span::raw(spaces.to_owned())
    }
}
