use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph},
};

use crate::tui::widgets::{palette, utils::shimmer};

const WIDTH: u16 = 62;

pub struct Progress<'a> {
    pub title: &'a str,
    pub label: &'a str,
    pub stage: &'a str,
    pub detail: Detail,
}

pub enum Detail {
    Bytes {
        done: u64,
        total: u64,
        rate: Option<f64>,
    },
    Files {
        done: u64,
        total: u64,
    },
}

impl Detail {
    fn totals(&self) -> (u64, u64) {
        match self {
            Self::Bytes { done, total, .. } | Self::Files { done, total } => (*done, *total),
        }
    }
}

impl Progress<'_> {
    fn has_total(&self) -> bool {
        let (done, total) = self.detail.totals();
        total > 0 && done <= total
    }

    fn percent(&self) -> u16 {
        let (done, total) = self.detail.totals();
        if total == 0 {
            return 0;
        }
        ((done.min(total) * 100) / total) as u16
    }
}

pub fn draw(frame: &mut Frame, area: Rect, progress: &Progress<'_>, tick: Option<usize>) -> Rect {
    let [row] = Layout::horizontal([Constraint::Length(WIDTH.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    // One blank row above the content and one below, and the bar itself is given
    // a row above and below so it reads as the measurement rather than as another
    // line of text.
    let content_rows = if progress.has_total() { 5 } else { 2 };
    let height = content_rows + 4;
    let [popup] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(row);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(palette::accent())
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            format!(" {} ", progress.title),
            palette::heading(),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < content_rows {
        return popup;
    }
    let content = Rect {
        y: inner.y + 1,
        height: content_rows.min(inner.height),
        ..inner
    };
    let rows = Layout::vertical(vec![Constraint::Length(1); content_rows as usize]).split(content);

    frame.render_widget(
        Paragraph::new(Line::styled(progress.label, palette::primary())),
        rows[0],
    );

    if !progress.has_total() {
        frame.render_widget(
            Paragraph::new(Line::from_iter(shimmer(progress.stage, tick))),
            rows[1],
        );
        return popup;
    }
    draw_bar(frame, rows[2], progress);
    // What is being counted, and what it is waiting on if it is waiting on
    // anything: a counted bar does not need the phase word instead of its numbers.
    let mut detail = match progress.detail {
        Detail::Bytes {
            done,
            total,
            rate: Some(rate),
        } if rate > 0.0 => {
            let mut numbers = format!("{} of {}", amount(done), amount(total));
            numbers.push_str(&format!(" · {}/s", amount(rate as u64)));
            if let Some(left) = remaining(total.saturating_sub(done), rate) {
                numbers.push_str(&format!(" · {left} left"));
            }
            numbers
        }
        Detail::Bytes { done, total, .. } => format!("{} of {}", amount(done), amount(total)),
        Detail::Files { done, total } => format!("{done} of {total} files"),
    };
    if !progress.stage.is_empty() {
        detail.push_str(&format!(" · {}", progress.stage));
    }
    frame.render_widget(
        Paragraph::new(Line::styled(detail, palette::muted())),
        Rect {
            height: 1,
            ..rows[4]
        },
    );
    popup
}

fn draw_bar(frame: &mut Frame, area: Rect, progress: &Progress<'_>) {
    const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let percent = format!("{:>4}%", progress.percent());
    // A space between the bar and the numbers, and the bar is what is left.
    let bar_width = usize::from(area.width).saturating_sub(percent.chars().count() + 1);
    let (done, total) = progress.detail.totals();
    let eighths = (bar_width as u64 * 8 * done.min(total))
        .checked_div(total)
        .unwrap_or(0);
    let filled = usize::try_from(eighths / 8).unwrap_or(0).min(bar_width);
    let part = usize::try_from(eighths % 8).unwrap_or(0);
    let hollow = bar_width.saturating_sub(filled + usize::from(part > 0));
    let line = Line::from(vec![
        Span::styled("█".repeat(filled), palette::accent()),
        Span::styled(PARTIAL[part].to_owned(), palette::accent()),
        Span::styled("░".repeat(hollow), palette::backdrop()),
        Span::raw(" "),
        Span::styled(percent, palette::primary()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn amount(bytes: u64) -> String {
    const KILO: f64 = 1_000.0;
    let bytes = bytes as f64;
    match bytes {
        bytes if bytes < KILO => format!("{bytes:.0} B"),
        bytes if bytes < KILO * KILO => format!("{:.1} kB", bytes / KILO),
        bytes if bytes < KILO * KILO * KILO => format!("{:.1} MB", bytes / (KILO * KILO)),
        bytes => format!("{:.2} GB", bytes / (KILO * KILO * KILO)),
    }
}

fn remaining(bytes: u64, rate: f64) -> Option<String> {
    let seconds = bytes as f64 / rate;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(match seconds {
        seconds if seconds < 1.0 => "under a second".to_owned(),
        seconds if seconds < 10.0 => format!("{seconds:.1}s"),
        seconds if seconds < 60.0 => format!("{seconds:.0}s"),
        seconds if seconds < 3600.0 => format!("{:.0}m", seconds / 60.0),
        seconds => format!("{:.0}h", seconds / 3600.0),
    })
}
