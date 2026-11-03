use std::{iter, mem};

use pulldown_cmark::Alignment;
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::super::palette;

pub(super) struct Table {
    pub alignments: Vec<Alignment>,
    pub head: Vec<String>,
    pub body: Vec<Vec<String>>,
}

// One cell of padding on each side of a column, and a border between neighbours.
const LEAST: usize = 1;

// A table is a grid with borders of its own: every cell is padded to the width of its
// column, so the separators line up instead of drifting with the text.
pub(super) fn lines(table: &Table, width: usize) -> Vec<Line<'static>> {
    let columns = table
        .head
        .len()
        .max(table.body.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return Vec::new();
    }
    let mut widths = vec![0usize; columns];
    for row in iter::once(&table.head).chain(table.body.iter()) {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.width());
        }
    }
    let total = |widths: &[usize]| widths.iter().map(|w| w + 3).sum::<usize>() + 1;
    while total(&widths) > width {
        // Taken from the widest column that can spare a cell, so a narrow one is not
        // squeezed to nothing for the sake of a wide neighbour.
        let Some((widest, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, column)| **column > LEAST)
            .max_by_key(|(_, column)| **column)
        else {
            break;
        };
        widths[widest] -= 1;
    }
    if total(&widths) > width {
        // Too many columns for the room it has: the plain form is a better lie than a
        // grid that does not fit.
        return flat(table);
    }

    let mut out = vec![Line::styled(rule(&widths, '┌', '┬', '┐'), palette::muted())];
    if !table.head.is_empty() {
        row(
            &mut out,
            &table.head,
            &widths,
            &table.alignments,
            palette::heading(),
        );
        out.push(Line::styled(rule(&widths, '├', '┼', '┤'), palette::muted()));
    }
    for cells in &table.body {
        row(
            &mut out,
            cells,
            &widths,
            &table.alignments,
            palette::primary(),
        );
    }
    out.push(Line::styled(rule(&widths, '└', '┴', '┘'), palette::muted()));
    out
}

fn rule(widths: &[usize], left: char, middle: char, right: char) -> String {
    let mut line = String::from(left);
    for (index, column) in widths.iter().enumerate() {
        line.push_str(&"─".repeat(column + 2));
        line.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    line
}

fn row(
    out: &mut Vec<Line<'static>>,
    cells: &[String],
    widths: &[usize],
    alignments: &[Alignment],
    tone: Style,
) {
    // A cell taller than one line makes the whole row that tall, so a wrapped cell
    // does not push the rest of its row out of line.
    let wrapped: Vec<Vec<String>> = widths
        .iter()
        .enumerate()
        .map(|(index, column)| {
            cells
                .get(index)
                .map(|cell| wrap(cell.trim(), *column))
                .unwrap_or_default()
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for line in 0..height {
        // The separators are borders: they take the border's tone, not the tone of the
        // words between them, or they read as emphasis.
        let mut spans = vec![Span::styled("│", palette::muted())];
        for (index, column) in widths.iter().enumerate() {
            let content = wrapped[index].get(line).map_or("", String::as_str);
            let padded = format!(" {} ", aligned(content, *column, alignments.get(index)));
            spans.push(Span::styled(padded, tone));
            spans.push(Span::styled("│", palette::muted()));
        }
        out.push(Line::from(spans));
    }
}

fn flat(table: &Table) -> Vec<Line<'static>> {
    iter::once(&table.head)
        .chain(table.body.iter())
        .filter(|row| !row.is_empty())
        .map(|cells| Line::styled(cells.join(" │ "), palette::primary()))
        .collect()
}

// A cell wraps inside its column rather than at the end of the line.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let taken = current.width() + usize::from(!current.is_empty());
            let room = width.saturating_sub(taken);
            if word.width() <= room {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
                break;
            }
            if current.is_empty() {
                // A word wider than the column is cut at its own edge.
                let cut = word
                    .char_indices()
                    .take_while(|(index, _)| word[..*index].width() < width)
                    .last()
                    .map_or(word.len(), |(index, character)| {
                        index + character.len_utf8()
                    });
                let (head, tail) = word.split_at(cut);
                lines.push(head.to_owned());
                word = tail;
            } else {
                lines.push(mem::take(&mut current));
            }
            if word.is_empty() {
                break;
            }
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn aligned(text: &str, width: usize, alignment: Option<&Alignment>) -> String {
    let room = width.saturating_sub(text.width());
    match alignment {
        Some(Alignment::Right) => format!("{}{text}", " ".repeat(room)),
        Some(Alignment::Center) => {
            let left = room / 2;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(room - left))
        }
        _ => format!("{text}{}", " ".repeat(room)),
    }
}
