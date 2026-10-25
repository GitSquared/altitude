//! Bytes moving, and how fast.
//!
//! A transfer is the one wait here that has something to count, so it is the one
//! wait that draws a bar rather than a travelling highlight. What it counts is
//! always bytes: the pieces of an archive on their way to another machine, or the
//! same pieces arriving from one.

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph},
};

use crate::tui::widgets::{palette, utils::shimmer};

/// The modal is wide enough for two columns of numbers and no wider: it is a
/// measurement, not a page.
const WIDTH: u16 = 62;

/// What a transfer is doing, in the words the modal has room for.
pub struct Progress<'a> {
    /// What the window is about: pulling a task here, sending one away.
    pub title: &'a str,
    /// The thing moving, named the way the operator named it.
    pub label: &'a str,
    /// What the transfer is waiting on. Empty while bytes are moving, when the
    /// numbers say it instead.
    pub stage: &'a str,
    /// Bytes landed, and the whole of them. Equal while there is nothing to
    /// count yet, which is what makes the bar indeterminate.
    pub done: u64,
    pub total: u64,
    /// Bytes per second over the last moment, when there is one to show.
    pub rate: Option<f64>,
}

impl Progress<'_> {
    /// Whether there are bytes to count yet, which is what draws the bar.
    fn has_total(&self) -> bool {
        self.total > 0 && self.done <= self.total
    }

    /// How far along, as whole percent.
    fn percent(&self) -> u16 {
        if self.total == 0 {
            return 0;
        }
        ((self.done.min(self.total) * 100) / self.total) as u16
    }
}

/// Draw the transfer, and answer with the box it occupies.
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
    let waiting = !progress.stage.is_empty();
    let detail = Rect {
        height: 1,
        ..rows[4]
    };
    if waiting {
        // A wait inside a transfer keeps the bar where it got to, and says what
        // it is waiting for where the numbers would be.
        frame.render_widget(
            Paragraph::new(Line::from_iter(shimmer(progress.stage, tick))),
            detail,
        );
    } else {
        let mut numbers = format!("{} of {}", amount(progress.done), amount(progress.total));
        if let Some(rate) = progress.rate.filter(|rate| *rate > 0.0) {
            numbers.push_str(&format!(" · {}/s", amount(rate as u64)));
            if let Some(left) = remaining(progress.total - progress.done, rate) {
                numbers.push_str(&format!(" · {left} left"));
            }
        }
        frame.render_widget(
            Paragraph::new(Line::styled(numbers, palette::muted())),
            detail,
        );
    }
    popup
}

/// The bar, with the whole percent at its right edge where it does not move.
fn draw_bar(frame: &mut Frame, area: Rect, progress: &Progress<'_>) {
    let percent = format!("{:>4}%", progress.percent());
    // A space between the bar and the numbers, and the bar is what is left.
    let bar_width = usize::from(area.width).saturating_sub(percent.chars().count() + 1);
    let filled = bar_width * usize::from(progress.percent()) / 100;
    let line = Line::from(vec![
        Span::styled("█".repeat(filled), palette::accent()),
        Span::styled("░".repeat(bar_width - filled), palette::backdrop()),
        Span::raw(" "),
        Span::styled(percent, palette::primary()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Bytes, in the units a person reads a transfer in.
///
/// Decimal rather than binary, because a link is sold and measured in decimal and
/// a bar that reports 64% of 71.9 MB is easier to check against a 75 MB file than
/// the same bytes spelled in MiB.
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

/// How long the rest will take at this rate, in the rough words a wait deserves.
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
