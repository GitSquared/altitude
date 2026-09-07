use std::io;

use crossterm::event::{EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, HorizontalAlignment, Layout},
    style::Stylize,
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph},
};
use tokio::signal::unix::{Signal, SignalKind, signal};

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminal = ratatui::init();

    let result = run(&mut terminal, &mut terminate, &mut interrupt).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
) -> io::Result<()> {
    let mut events = EventStream::new();

    loop {
        terminal.draw(draw)?;

        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                if let Some(key) = event?.as_key_press_event()
                    && (key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)))
                {
                    return Ok(());
                }
            }
        }
    }
}

fn draw(frame: &mut Frame) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .margin(1)
    .spacing(1)
    .areas(frame.area());

    let [title, version] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(12)]).areas(header);
    frame.render_widget(Line::from("⛰  altitude".cyan().bold()), title);
    frame.render_widget(
        Line::from(concat!("v", env!("CARGO_PKG_VERSION")).dim()).right_aligned(),
        version,
    );
    frame.render_widget(
        Paragraph::new("No tasks yet".dim()).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .padding(Padding::uniform(1)),
        ),
        body,
    );
    frame.render_widget(
        Line::from("press q to quit".dim()).alignment(HorizontalAlignment::Right),
        footer,
    );
}
