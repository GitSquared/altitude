use std::{error::Error, future::Future, io, time::Duration};

use crossterm::event::{EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    text::Line,
    widgets::Paragraph,
};
use tokio::{signal::unix::Signal, sync::watch, time};

use super::palette;
use crate::{isolation::IsolationStatus, tui::utils::shimmer};

const TITLE: &str = "⛰  altitude";

pub async fn database<T, E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    initialize: impl Future<Output = Result<T, E>>,
) -> io::Result<Option<T>>
where
    E: Error + Send + Sync + 'static,
{
    run_future(
        terminal,
        terminate,
        interrupt,
        "migrating database...",
        async { initialize.await.map_err(io::Error::other) },
    )
    .await
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

pub async fn task_resume<T>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    resume: impl Future<Output = T>,
) -> io::Result<Option<T>> {
    run_future(terminal, terminate, interrupt, "resuming tasks...", async {
        Ok(resume.await)
    })
    .await
}

async fn run_future<T>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    label: &'static str,
    operation: impl Future<Output = io::Result<T>>,
) -> io::Result<Option<T>> {
    let mut events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(80));
    let mut shimmer = 0;
    tokio::pin!(operation);

    loop {
        terminal.draw(|frame| draw(frame, label, shimmer))?;
        tokio::select! {
            _ = terminate.recv() => return Ok(None),
            _ = interrupt.recv() => return Ok(None),
            result = &mut operation => return result.map(Some),
            _ = animation.tick() => shimmer = shimmer.wrapping_add(1),
            event = events.next() => {
                let Some(event) = event else { return Ok(None) };
                if let Some(key) = event?.as_key_press_event()
                    && (key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL))) {
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
        IsolationStatus::PreparingWorkspacePackages => "preparing workspace packages...",
        IsolationStatus::Ready | IsolationStatus::Failed(_) => "setting up isolation runtime...",
    }
}

fn draw(frame: &mut Frame, label: &str, tick: usize) {
    let [_, title_area, _, status, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(frame.area());

    frame.render_widget(
        Paragraph::new(Line::styled(TITLE, palette::heading())).centered(),
        title_area,
    );

    frame.render_widget(
        Paragraph::new(Line::from_iter(shimmer(label, tick))).centered(),
        status,
    );
}
