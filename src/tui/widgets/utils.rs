use ratatui::{
    layout::{Constraint, Flex, Layout, Rect},
    style::Style,
    text::Span,
};
use tokio::time::{self, Duration, Interval, MissedTickBehavior};

use crate::tui::widgets::palette;

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

pub struct Dot {
    glyph: char,
    style: Style,
    travelling: bool,
}

impl Dot {
    pub fn still(glyph: char, style: Style) -> Self {
        Self {
            glyph,
            style,
            travelling: false,
        }
    }

    pub fn travelling(glyph: char, style: Style) -> Self {
        Self {
            glyph,
            style,
            travelling: true,
        }
    }

    pub fn spans(&self, tick: Option<usize>) -> Vec<Span<'static>> {
        let text = format!("{} ", self.glyph);
        if self.travelling && tick.is_some() {
            shimmer(&text, tick)
        } else {
            vec![Span::styled(text, self.style)]
        }
    }
}

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

pub fn since(at_ms: i64, now_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(at_ms) / 1000;
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

pub fn ago(at_ms: i64, now_ms: i64) -> String {
    if now_ms.saturating_sub(at_ms) < 60_000 {
        "just now".to_owned()
    } else {
        format!("{} ago", since(at_ms, now_ms))
    }
}

// The hour and minute on the operator's own clock, because a time is something to
// read whereas a countdown is a number that goes stale in front of them.
pub fn clock(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms)
        .map(|at| at.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}
