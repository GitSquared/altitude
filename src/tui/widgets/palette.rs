use ratatui::style::{Color, Modifier, Style};

pub fn focus() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

pub fn inactive_selection() -> Style {
    Style::new().fg(Color::Reset).bg(Color::DarkGray)
}

pub fn selection_detail() -> Style {
    Style::new().fg(Color::Gray)
}

pub fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

pub fn heading() -> Style {
    Style::new().fg(Color::Reset).add_modifier(Modifier::BOLD)
}

pub fn primary() -> Style {
    Style::new().fg(Color::Reset)
}

pub fn muted() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub fn backdrop() -> Style {
    muted().add_modifier(Modifier::DIM)
}

pub fn good() -> Style {
    Style::new().fg(Color::Green)
}

pub fn attention() -> Style {
    Style::new().fg(Color::Yellow)
}

pub fn decision() -> Style {
    Style::new().fg(Color::LightBlue)
}

pub fn bad() -> Style {
    Style::new().fg(Color::Red)
}

pub fn waiting() -> Style {
    Style::new().fg(Color::Reset)
}

pub fn waiting_rest() -> Style {
    muted()
}
