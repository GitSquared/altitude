use ratatui::{
    layout::{Constraint, Flex, Layout, Rect},
    style::Style,
    text::Span,
};
use tokio::time::{self, Duration, Interval, MissedTickBehavior};

use crate::tui::widgets::palette;

/// How many characters wide the travelling highlight is.
const SHIMMER_WIDTH: usize = 3;

pub fn shimmer_clock() -> Interval {
    let mut clock = time::interval(Duration::from_millis(80));
    clock.set_missed_tick_behavior(MissedTickBehavior::Skip);
    clock
}

pub fn centered_width(area: Rect, width: u16, height_percent: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    Layout::vertical([Constraint::Percentage(height_percent)])
        .flex(Flex::Center)
        .areas::<1>(area)[0]
}

/// A mark in front of a list row: what the last answer about that row said.
///
/// Lists that ask something on their own clock mark their rows with one, so the
/// same thing in front of a row means the same thing wherever it is: filled when the
/// answer came back, hollow while it has not, and travelling while it is still
/// coming. A mark that is not an answer, like the house on the machine you are
/// on, stands for itself.
pub struct Dot {
    glyph: char,
    style: Style,
    travelling: bool,
}

impl Dot {
    /// A mark for an answer that has arrived, or will not.
    pub fn still(glyph: char, style: Style) -> Self {
        Self {
            glyph,
            style,
            travelling: false,
        }
    }

    /// A mark for a question that is still out.
    pub fn travelling(glyph: char, style: Style) -> Self {
        Self {
            glyph,
            style,
            travelling: true,
        }
    }

    /// The mark, and the space after it, ready to be the first span of a row.
    ///
    /// A travelling mark only travels while there is a frame counter to move it:
    /// with animations off it sits still, in the style it would have travelled in.
    pub fn spans(&self, tick: Option<usize>) -> Vec<Span<'static>> {
        let text = format!("{} ", self.glyph);
        if self.travelling && tick.is_some() {
            shimmer(&text, tick)
        } else {
            vec![Span::styled(text, self.style)]
        }
    }
}

/// A highlight that travels across `text`, so a wait reads as work in progress.
///
/// `tick` is any increasing counter; each call wraps it, so callers only need
/// to keep counting. `None` is a wait with no animation to draw: the text is
/// shown whole, in the colour the highlight would have reached.
pub fn shimmer(text: &str, tick: Option<usize>) -> Vec<Span<'static>> {
    let Some(tick) = tick else {
        return vec![Span::styled(text.to_owned(), palette::waiting())];
    };
    let position = tick % (text.chars().count() + SHIMMER_WIDTH);
    text.chars()
        .enumerate()
        .map(|(index, character)| {
            let style = if index.abs_diff(position) < SHIMMER_WIDTH {
                palette::waiting()
            } else {
                palette::waiting_rest()
            };
            Span::styled(character.to_string(), style)
        })
        .collect()
}

/// What a terminal would have drawn, as text.
///
/// A command's output is written for a terminal: colours, cursor questions, window
/// titles. Read as text, those are noise — and a question is noise a program asked
/// something that was not there to answer.
pub fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => match characters.next() {
                // A control sequence ends at the first letter.
                Some('[') => {
                    characters.by_ref().any(|c| c.is_ascii_alphabetic());
                }
                // An operating system command ends at a bell or a string terminator.
                Some(']') => {
                    let mut previous = ' ';
                    for c in characters.by_ref() {
                        if c == '\u{7}' || (previous == '\u{1b}' && c == '\\') {
                            break;
                        }
                        previous = c;
                    }
                }
                Some(_) | None => {}
            },
            '\n' | '\t' => plain.push(character),
            c if !c.is_control() => plain.push(c),
            _ => {}
        }
    }
    plain
}

/// Shortens `text` to `width` characters, marking the cut with an ellipsis.
pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut truncated = text.chars().take(width - 1).collect::<String>();
    truncated.push('…');
    truncated
}

/// How long ago something happened, in the fewest characters that still say it.
///
/// Ages are read at a glance and compared down a column, so they stay short and
/// change unit rather than growing words: `12m`, not `12 minutes ago`. Anything
/// under a minute is `just now`: a clock that counts seconds makes an idle
/// screen look busy.
pub fn since(at_ms: i64, now_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(at_ms) / 1000;
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// The same age as a phrase: `just now`, or `12m ago`.
pub fn ago(at_ms: i64, now_ms: i64) -> String {
    if now_ms.saturating_sub(at_ms) < 60_000 {
        "just now".to_owned()
    } else {
        format!("{} ago", since(at_ms, now_ms))
    }
}
