//! Lists that say what each row is, and the cursor that moves among them.
//!
//! A row is a small block rather than a line: an optional category line above
//! the name, the name itself, and an optional detail under it. The name is what
//! the list is read by, so it is the line the focus bar covers, and the pieces
//! are placed the same way in every list that uses one: mark at the left edge,
//! the note at the right edge so a column of notes lines up.
//!
//! A row can start a group with a section heading. Rows may then differ in
//! height, so the cursor measures the list when it decides how many fit.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem},
};

use crate::tui::widgets::{palette, utils::truncate};

/// A section heading attached to the row that starts it.
pub struct Heading {
    pub title: String,
    pub note: Option<String>,
}

/// One row of a list.
pub struct Row {
    /// A section heading above this row, when it starts a group.
    pub heading: Option<Heading>,
    /// The line the list is read by.
    pub title: String,
    /// Said above the title: a category, a state.
    pub label: Option<Span<'static>>,
    /// Said under the title: what the row last had to say.
    pub detail: Option<String>,
    /// Said at the right edge of the first line.
    pub note: Option<String>,
    /// The mark in front of the first line, where the row has a status of its
    /// own. Already drawn by the caller, because a mark can be one that travels
    /// while it waits, and only the drawing knows where the frame is.
    pub mark: Vec<Span<'static>>,
    /// How the name reads when the focus is elsewhere. The focus bar overrides it.
    pub tone: Style,
    /// Whether a blank line follows the row, so fat rows can breathe.
    pub gap: bool,
}

impl Row {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            heading: None,
            label: None,
            detail: None,
            note: None,
            mark: Vec::new(),
            tone: palette::primary(),
            gap: false,
        }
    }

    pub fn heading(mut self, title: impl Into<String>, note: impl Into<String>) -> Self {
        self.heading = Some(Heading {
            title: title.into(),
            note: Some(note.into()),
        });
        self
    }

    pub fn label(mut self, label: Span<'static>) -> Self {
        self.label = Some(label);
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

    pub fn gap(mut self, gap: bool) -> Self {
        self.gap = gap;
        self
    }

    /// How many lines the row takes, before its gap.
    fn height(&self) -> u16 {
        u16::from(self.heading.is_some())
            + 1
            + u16::from(self.label.is_some())
            + u16::from(self.detail.is_some())
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
        // The mark sits at the left edge of the first line, whether or not the
        // row has a category line of its own. The lines under it align with the
        // name so a list of marked rows still reads as a column.
        if let Some(label) = &self.label {
            let text = format!("{mark}{}", label.content);
            let used = Line::from(text.as_str()).width() + note_width;
            let padding = " ".repeat(width.saturating_sub(used).max(1));
            if focused {
                lines.push(Line::from(Span::styled(
                    format!("{text}{padding}{note}"),
                    palette::focus(),
                )));
            } else {
                let mut spans = vec![Span::styled(text, self.tone)];
                spans.push(Span::raw(padding));
                spans.push(Span::styled(note.to_owned(), secondary));
                lines.push(Line::from(spans));
            }
        }
        let note_here = if self.label.is_none() { note } else { "" };
        let note_here_width = Line::from(note_here).width();
        let available = width
            .saturating_sub(indent)
            .saturating_sub(note_here_width)
            .max(1);
        let title = truncate(&self.title, available);
        let prefix = if self.label.is_none() {
            mark.clone()
        } else {
            " ".repeat(indent)
        };
        let used = indent + Line::from(title.as_str()).width() + note_here_width;
        let padding = " ".repeat(width.saturating_sub(used).max(1));
        if focused {
            // The whole row is one bar, not just the line with the name in it:
            // what is selected is the block, and every line of it belongs to it.
            lines.push(Line::from(Span::styled(
                format!("{prefix}{title}{padding}{note_here}"),
                palette::focus(),
            )));
        } else {
            // The name carries its tone itself rather than inheriting it from the
            // line, so what is behind a row cannot decide how the row reads.
            let mut spans = vec![Span::raw(prefix), Span::styled(title, self.tone)];
            spans.push(Span::raw(padding));
            if !note_here.is_empty() {
                spans.push(Span::styled(note_here.to_owned(), secondary));
            }
            lines.push(Line::from(spans));
        }
        if let Some(detail) = &self.detail {
            let available = width.saturating_sub(indent).max(1);
            let detail = truncate(detail, available);
            let used = indent + Line::from(detail.as_str()).width();
            let padding = " ".repeat(width.saturating_sub(used).max(1));
            if focused {
                // The fill is the row's, but the detail is not its name: the
                // quieter foreground keeps the line under the title rather than
                // turning the whole bar into one weight.
                lines.push(Line::from(Span::styled(
                    format!("{}{detail}{padding}", " ".repeat(indent)),
                    palette::focus_detail(),
                )));
            } else {
                lines.push(
                    Line::from(vec![
                        Span::raw(" ".repeat(indent)),
                        Span::styled(detail, secondary),
                    ])
                    .style(self.tone),
                );
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

    /// Move the view toward where it should be, easing out so a long jump does
    /// not crawl. Answers whether anything moved, which is what asks for a frame.
    pub fn glide(&mut self) -> bool {
        if self.offset == self.target {
            return false;
        }
        let gap = self.offset.abs_diff(self.target);
        let step = (gap / 2).max(1);
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

/// The height a uniform list's first row takes, gap included.
pub fn row_height(rows: &[Row]) -> u16 {
    rows.first()
        .map_or(1, |row| row.height() + u16::from(row.gap))
}

/// How many rows of a list with mixed heights fit in `height`.
pub fn visible(height: u16, rows: &[Row]) -> usize {
    let mut used = 0;
    let mut count = 0;
    for row in rows {
        let row_height = row.height() + u16::from(row.gap);
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
) {
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
    for row in rows.iter().skip(offset) {
        if top >= area.bottom() {
            break;
        }
        let height = row.height().min(area.bottom() - top);
        frame.render_widget(
            Clear,
            Rect {
                x: area.x,
                y: top,
                width: area.width,
                height,
            },
        );
        top = top.saturating_add(row.height() + u16::from(row.gap));
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
            if row.gap {
                lines.push(Line::default());
            }
            ListItem::new(lines)
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), list_area);
}

/// The full-width bar behind one row, gutter included.
fn bar(frame: &mut Frame, area: Rect, rows: &[Row], offset: usize, index: usize, style: Style) {
    if index < offset || index >= rows.len() {
        return;
    }
    let heading = u16::from(rows[index].heading.is_some());
    let content_height = rows[index]
        .height()
        .saturating_sub(heading)
        .saturating_sub(u16::from(rows[index].gap))
        .max(1);
    let top = area.y.saturating_add(
        rows[offset..index]
            .iter()
            .map(|row| row.height() + u16::from(row.gap))
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

/// A section heading: its name, and the count at the right edge.
fn heading_line(heading: &Heading, width: usize) -> Line<'static> {
    let note = heading.note.as_deref().unwrap_or_default();
    let note_width = Line::from(note).width();
    let title = truncate(&heading.title, width.saturating_sub(note_width).max(1));
    let used = Line::from(title.as_str()).width() + note_width;
    let padding = " ".repeat(width.saturating_sub(used).max(1));
    Line::from(vec![
        Span::styled(title, palette::heading()),
        Span::raw(padding),
        Span::styled(note.to_owned(), palette::muted()),
    ])
}
