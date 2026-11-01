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
    pub mark: Vec<Span<'static>>,
    pub tone: Style,
}

pub struct Row {
    pub heading: Option<Heading>,
    pub title: String,
    pub detail: Option<String>,
    pub note: Option<String>,
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

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

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
            (self.selected + 1).min(len - 1)
        } else {
            self.selected.saturating_sub(1)
        };
    }

    pub fn page_by(&mut self, forward: bool, layout: &Layout) {
        let len = layout.heights.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let band = usize::from(layout.height).max(1);
        let mut used = 0;
        let mut index = self.selected.min(len - 1);
        while used < band {
            if forward {
                if index + 1 >= len {
                    break;
                }
                index += 1;
            } else {
                if index == 0 {
                    break;
                }
                index -= 1;
            }
            used += usize::from(layout.heights[index]) + 1;
        }
        self.selected = index;
    }

    pub fn follow(&mut self, rows: &[Row], height: u16, animate: bool) {
        let band = usize::from(height).max(1);
        if rows.is_empty() {
            self.selected = 0;
            self.target = 0;
            self.offset = 0;
            return;
        }
        self.selected = self.selected.min(rows.len() - 1);
        let top = start_line(rows, self.selected);
        let content = usize::from(rows[self.selected].height()).max(1);
        let last = content_lines(rows).saturating_sub(band);
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
}

fn block(row: &Row) -> usize {
    usize::from(row.height()) + 1
}

fn start_line(rows: &[Row], index: usize) -> usize {
    rows[..index.min(rows.len())].iter().map(block).sum()
}

pub fn content_lines(rows: &[Row]) -> usize {
    rows.iter().map(block).sum()
}

#[derive(Clone, Default)]
pub struct Layout {
    heights: Vec<u16>,
    height: u16,
}

impl Layout {
    pub fn of(rows: &[Row], height: u16) -> Self {
        Self {
            heights: rows.iter().map(Row::height).collect(),
            height,
        }
    }
}

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
    let mut hitboxes = Vec::new();
    for (index, _) in rows.iter().enumerate() {
        let Some((top, visible)) = on_screen(rows, index, offset, area) else {
            if start_line(rows, index) >= offset.saturating_add(usize::from(area.height)) {
                break;
            }
            continue;
        };
        let rect = Rect {
            x: area.x,
            y: top,
            width: area.width,
            height: visible,
        };
        frame.render_widget(Clear, rect);
        hitboxes.push((rect, index));
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
    // The view can start part-way through a row, so the first one drawn gives up
    // the lines that are already above it.
    let mut skipped = false;
    let items = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let start = start_line(rows, index);
            if start + block(row) <= offset {
                return None;
            }
            if start >= offset.saturating_add(usize::from(area.height)) {
                return None;
            }
            let mut lines = Vec::new();
            if let Some(heading) = &row.heading {
                lines.push(heading_line(heading, width));
            }
            lines.extend(row.lines(width, focused == Some(index), selected == Some(index)));
            lines.push(Line::default());
            if !skipped {
                skipped = true;
                lines.drain(..offset.saturating_sub(start).min(lines.len()));
            }
            Some(ListItem::new(lines))
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), list_area);
    hitboxes
}

fn on_screen(rows: &[Row], index: usize, offset: usize, area: Rect) -> Option<(u16, u16)> {
    let start = start_line(rows, index);
    let first = start.max(offset);
    let last = (start + block(&rows[index])).min(offset.saturating_add(usize::from(area.height)));
    let content = start + usize::from(rows[index].height());
    // The blank line under a row is not the row, so a row scrolled down to its
    // last line has nothing left to show a bar over.
    let last = last.min(content);
    if first >= last {
        return None;
    }
    let top = area
        .y
        .saturating_add(u16::try_from(first - offset).unwrap_or(u16::MAX));
    let height = u16::try_from(last - first).unwrap_or(u16::MAX);
    Some((top, height))
}

fn bar(frame: &mut Frame, area: Rect, rows: &[Row], offset: usize, index: usize, style: Style) {
    if index >= rows.len() {
        return;
    }
    let heading = u16::from(rows[index].heading.is_some());
    let content_height = rows[index].height().saturating_sub(heading).max(1);
    let start = start_line(rows, index) + usize::from(heading);
    // A bar is clipped by the view the same way the row under it is, so a row
    // scrolled half out of sight does not paint over the lines above it.
    let first = start.max(offset);
    let last =
        (start + usize::from(content_height)).min(offset.saturating_add(usize::from(area.height)));
    if first >= last {
        return;
    }
    let bar = Rect {
        x: area.x,
        y: area
            .y
            .saturating_add(u16::try_from(first - offset).unwrap_or(u16::MAX)),
        width: area.width,
        height: u16::try_from(last - first).unwrap_or(u16::MAX),
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

fn padding_span(spaces: &str, selected: bool) -> Span<'static> {
    if selected {
        Span::styled(spaces.to_owned(), palette::inactive_selection())
    } else {
        Span::raw(spaces.to_owned())
    }
}
