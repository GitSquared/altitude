use std::collections::HashSet;

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::row::{MARK_COLUMN, Row};

// One row of the list, as the lines it takes: the heading of the group it opens
// and its own two lines, or only the heading when that group is folded.
pub(super) struct Unit {
    pub(super) index: usize,
    pub(super) folded: bool,
    pub(super) heading: Option<String>,
    pub(super) top: usize,
    pub(super) height: u16,
}

impl Unit {
    pub(super) fn opens_group(&self) -> bool {
        self.heading.is_some()
    }

    // Where the row's own lines start and how many there are: a folded row is its
    // heading and nothing else, so the heading is what the cursor sits on and what
    // a bar covers.
    pub(super) fn own(&self) -> (usize, u16) {
        if self.opens_group() && self.folded {
            return (self.top, 1);
        }
        let heading = u16::from(self.opens_group());
        (self.top + usize::from(heading), self.height - heading)
    }
}

// A column of the table: which of a row's facts it holds. Where the facts land
// is the table's business, so a row without this one leaves its cell blank rather
// than moving the cells after it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Cell {
    Reference(usize),
    Age,
    Note,
}

impl Cell {
    // The age is the one column that reads towards the border: it is a measure of
    // time, and measures line up on their right.
    pub(super) fn is_right(self) -> bool {
        matches!(self, Self::Age)
    }
}

// The width is cut into equal shares — the title takes four, every column after it
// takes one — so the table spreads across the whole screen at any size instead of
// crowding the border the moment the title has room to spare. The title's share is
// twice what the number of columns would give it: it is the one that wraps, and the
// age ends at the border, so the eye needs the room beside it.
#[derive(Default)]
pub(super) struct Grid {
    pub(super) width: usize,
    pub(super) title: usize,
    pub(super) cells: Vec<Cell>,
    // Where each cell's text starts: one column of air after the one before it,
    // and the last cell running on to the border.
    pub(super) starts: Vec<usize>,
    pub(super) rooms: Vec<usize>,
}

#[derive(Default)]
pub(super) struct Layout {
    pub(super) units: Vec<Unit>,
    pub(super) height: u16,
    pub(super) grid: Grid,
}

impl Layout {
    pub(super) fn of(rows: &[Row], folded: &HashSet<String>, width: usize, height: u16) -> Self {
        let indent = indent_of(rows);
        let grid = Grid::of(rows, width);
        let mut units = Vec::with_capacity(rows.len());
        let mut top = 0;
        let mut hidden = false;
        for (index, row) in rows.iter().enumerate() {
            let opens = row.heading.as_ref().map(|heading| heading.title.clone());
            if let Some(title) = &opens {
                hidden = folded.contains(title);
            } else if hidden {
                continue;
            }
            // Every row is followed by air, which is also the air before the next
            // section's heading.
            if !units.is_empty() {
                top += 1;
            }
            let height = if hidden {
                1
            } else {
                u16::from(opens.is_some()) + row.height(indent, &grid)
            };
            units.push(Unit {
                index,
                folded: hidden,
                heading: opens,
                top,
                height,
            });
            top += usize::from(height);
        }
        Self {
            units,
            height,
            grid,
        }
    }

    pub(super) fn content_lines(&self) -> usize {
        self.units
            .last()
            .map_or(0, |unit| unit.top + usize::from(unit.height))
    }

    pub(super) fn top_of(&self, row: usize) -> Option<usize> {
        self.units
            .iter()
            .find(|unit| unit.index == row)
            .map(|unit| unit.top)
    }

    pub(super) fn height_of(&self, row: usize) -> u16 {
        // The heading counts: a row that opens a group needs a line more on screen
        // than its own.
        self.units
            .iter()
            .find(|unit| unit.index == row)
            .map_or(1, |unit| unit.height)
    }

    pub(super) fn rows(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.units.iter().map(|unit| (unit.top, unit.index))
    }

    // The group a row belongs to: the heading of the nearest row above it that
    // opens one.
    pub(super) fn group_of(&self, row: usize) -> Option<(&str, bool)> {
        let at = self.units.iter().position(|unit| unit.index == row)?;
        self.units[..=at]
            .iter()
            .rev()
            .find_map(|unit| unit.heading.as_deref().map(|title| (title, unit.folded)))
    }
}

const TITLE_SHARES: usize = 4;

impl Grid {
    fn of(rows: &[Row], width: usize) -> Self {
        let references = rows
            .iter()
            .map(|row| row.references.len())
            .max()
            .unwrap_or(0);
        // The count comes first: it is the reason the eye stops on the row at all,
        // and a heading carries one for the group.
        let mut cells: Vec<Cell> = Vec::with_capacity(references + 2);
        let counted = rows.iter().any(|row| {
            row.note.is_some()
                || row
                    .heading
                    .as_ref()
                    .is_some_and(|heading| !heading.note.is_empty())
        });
        if counted {
            cells.push(Cell::Note);
        }
        cells.extend((0..references).map(Cell::Reference));
        if rows.iter().any(|row| row.age.is_some()) {
            cells.push(Cell::Age);
        }
        let share = (width / (TITLE_SHARES + cells.len())).max(1);
        let title = width.saturating_sub(share * cells.len());
        let mut starts = Vec::with_capacity(cells.len());
        let mut rooms = Vec::with_capacity(cells.len());
        let mut at = title;
        for index in 0..cells.len() {
            let last = index + 1 == cells.len();
            let room = if last {
                width.saturating_sub(at)
            } else {
                share.saturating_sub(1)
            };
            starts.push(at);
            rooms.push(room);
            at = at
                .saturating_add(room)
                .saturating_add(u16::from(!last).into());
        }
        Self {
            width,
            title,
            cells,
            starts,
            rooms,
        }
    }

    // Where a cell's text starts, which is where its name starts too.
    pub(super) fn start(&self, index: usize) -> usize {
        self.starts.get(index).copied().unwrap_or(self.width)
    }

    pub(super) fn column_of(&self, cell: Cell) -> Option<usize> {
        self.cells.iter().position(|held| *held == cell)
    }
}

pub(super) fn indent_of(rows: &[Row]) -> usize {
    if rows.iter().any(|row| row.heading.is_some()) {
        MARK_COLUMN
    } else {
        0
    }
}

pub(super) fn visible(area: Rect, start: usize, height: usize, offset: usize) -> Option<Rect> {
    let first = start.max(offset);
    let last = (start + height).min(offset + usize::from(area.height));
    if first >= last {
        return None;
    }
    Some(Rect {
        x: area.x,
        y: area
            .y
            .saturating_add(u16::try_from(first - offset).unwrap_or(u16::MAX)),
        width: area.width,
        height: u16::try_from(last - first).unwrap_or(u16::MAX),
    })
}

// The bar is written, not merely styled: the field is painted into cells that
// still hold the open marker, and a cell that only carries a background is still
// one of them.
pub(super) fn bar(frame: &mut ratatui::Frame, rect: Rect, style: Style) {
    let blank = " ".repeat(usize::from(rect.width));
    for y in rect.top()..rect.bottom() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(blank.clone(), style))),
            Rect {
                y,
                height: 1,
                ..rect
            },
        );
    }
}
