use ratatui::{style::Style, text::Span};
use tokio::time::{self, Duration, Interval, MissedTickBehavior};

use crate::tui::widgets::palette;

const SHIMMER_WIDTH: usize = 3;

pub fn shimmer_clock() -> Interval {
    let mut clock = time::interval(Duration::from_millis(80));
    clock.set_missed_tick_behavior(MissedTickBehavior::Skip);
    clock
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
