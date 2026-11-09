use std::io;

use crossterm::event::{EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    text::Line,
    widgets::Paragraph,
};
use tokio::{
    signal::unix::{Signal, SignalKind, signal},
    time,
};

use super::{background::terrain, keys, widgets::palette};
use crate::{brand, version::VERSION};

// The boot screen with nothing to wait for: the same field and the same title,
// left running until someone quits it.
pub async fn run() -> io::Result<()> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminal = ratatui::init();
    let result = until_quit(&mut terminal, &mut terminate, &mut interrupt).await;
    ratatui::restore();
    result
}

async fn until_quit(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
) -> io::Result<()> {
    let mut events = EventStream::new();
    let mut clock = time::interval(terrain::CLOCK);
    clock.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut terrain = terrain::Terrain::default();
    loop {
        terminal.draw(|frame| draw(frame, &mut terrain))?;
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = clock.tick() => terrain.advance(),
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                let Some(key) = event?.as_key_press_event() else { continue };
                if keys::is_dismiss(key)
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)) {
                    return Ok(());
                }
            }
        }
    }
}

fn draw(frame: &mut Frame, terrain: &mut terrain::Terrain) {
    let area = frame.area();
    terrain.mark(frame.buffer_mut(), area);
    let rows = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::styled(brand::TITLE, palette::heading())).centered(),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(VERSION, palette::backdrop())).centered(),
        rows[2],
    );
    terrain.draw(frame.buffer_mut(), area);
}
