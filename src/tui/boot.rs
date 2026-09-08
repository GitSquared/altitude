use std::{error::Error, future::Future, io, time::Duration};

use crossterm::event::{EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::Paragraph,
};
use tokio::{signal::unix::Signal, sync::watch, time};

use crate::isolation::IsolationStatus;

const TITLE: &str = "⛰  altitude";
const SHIMMER_WIDTH: usize = 3;

pub async fn database<T, E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    initialize: impl Future<Output = Result<T, E>>,
) -> io::Result<Option<T>>
where
    E: Error + Send + Sync + 'static,
{
    let mut events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(80));
    let mut shimmer = 0;
    tokio::pin!(initialize);

    loop {
        terminal.draw(|frame| draw(frame, "migrating database...", shimmer))?;

        tokio::select! {
            _ = terminate.recv() => return Ok(None),
            _ = interrupt.recv() => return Ok(None),
            result = &mut initialize => return result.map(Some).map_err(io::Error::other),
            _ = animation.tick() => shimmer = shimmer.wrapping_add(1),
            event = events.next() => {
                let Some(event) = event else { return Ok(None) };
                if let Some(key) = event?.as_key_press_event()
                    && (key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)))
                {
                    return Ok(None);
                }
            }
        }
    }
}

pub async fn isolation(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    mut status: watch::Receiver<IsolationStatus>,
) -> io::Result<Option<watch::Receiver<IsolationStatus>>> {
    let mut events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(80));
    let mut shimmer = 0;

    loop {
        terminal.draw(|frame| draw(frame, isolation_label(&status.borrow()), shimmer))?;

        let current_status = status.borrow().clone();
        match current_status {
            IsolationStatus::Ready => return Ok(Some(status)),
            IsolationStatus::Failed(error) => return Err(io::Error::other(error)),
            _ => {}
        }

        tokio::select! {
            _ = terminate.recv() => return Ok(None),
            _ = interrupt.recv() => return Ok(None),
            changed = status.changed() => {
                if changed.is_err() {
                    return Err(io::Error::other("isolation setup stopped unexpectedly"));
                }
            }
            _ = animation.tick() => shimmer = shimmer.wrapping_add(1),
            event = events.next() => {
                let Some(event) = event else { return Ok(None) };
                if let Some(key) = event?.as_key_press_event()
                    && (key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)))
                {
                    return Ok(None);
                }
            }
        }
    }
}

fn isolation_label(status: &IsolationStatus) -> &'static str {
    match status {
        IsolationStatus::Starting | IsolationStatus::InstallingRuntime => {
            "setting up isolation runtime..."
        }
        IsolationStatus::PreparingBaseImage => "fetching base sandbox image...",
        IsolationStatus::Ready | IsolationStatus::Failed(_) => "setting up isolation runtime...",
    }
}

fn draw(frame: &mut Frame, label: &str, shimmer: usize) {
    let [_, title_area, _, status, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(TITLE).white().bold()).centered(),
        title_area,
    );

    let position = shimmer % (label.chars().count() + SHIMMER_WIDTH);
    let shimmer_text = label.chars().enumerate().map(|(index, character)| {
        let is_shimmering = index.abs_diff(position) < SHIMMER_WIDTH;
        let style = if is_shimmering {
            Style::new().fg(Color::Gray)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        Span::styled(character.to_string(), style)
    });

    frame.render_widget(
        Paragraph::new(Line::from_iter(shimmer_text)).centered(),
        status,
    );
}
