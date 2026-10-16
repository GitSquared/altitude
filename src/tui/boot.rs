use std::{error::Error, future::Future, io};

use crossterm::event::{EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    text::Line,
    widgets::Paragraph,
};
use tokio::{signal::unix::Signal, sync::watch};

use crate::tui::{background::terrain, widgets::palette};
use crate::{
    isolation::IsolationStatus,
    tui::widgets::utils::{shimmer, shimmer_clock},
};

const TITLE: &str = "⛰  altitude";

pub async fn database<T, E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    animations: bool,
    initialize: impl Future<Output = Result<T, E>>,
) -> io::Result<Option<T>>
where
    E: Error + Send + Sync + 'static,
{
    wait_for(
        terminal,
        terminate,
        interrupt,
        animations,
        || "migrating database...",
        async { initialize.await.map_err(io::Error::other) },
    )
    .await
}

pub async fn isolation(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    status: watch::Receiver<IsolationStatus>,
    animations: bool,
) -> io::Result<Option<watch::Receiver<IsolationStatus>>> {
    // Two handles: one reports what is happening, the other waits for it to
    // change. The label is read every frame, so it follows the status.
    let shown = status.clone();
    let mut changes = status;
    wait_for(
        terminal,
        terminate,
        interrupt,
        animations,
        || isolation_label(&shown.borrow()),
        async move {
            loop {
                // Read it out first: the guard would otherwise outlive the match,
                // and the receiver is what gets returned.
                let current = changes.borrow().clone();
                match current {
                    IsolationStatus::Ready => return Ok(changes),
                    IsolationStatus::Failed(error) => return Err(io::Error::other(error)),
                    _ => {}
                }
                if changes.changed().await.is_err() {
                    return Err(io::Error::other("isolation setup stopped unexpectedly"));
                }
            }
        },
    )
    .await
}

pub async fn task_resume<T>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    animations: bool,
    resume: impl Future<Output = T>,
) -> io::Result<Option<T>> {
    wait_for(
        terminal,
        terminate,
        interrupt,
        animations,
        || "resuming tasks...",
        async { Ok(resume.await) },
    )
    .await
}

/// Show a labelled status while one operation finishes, and let the operator give
/// up on it: `q` or ctrl-c returns nothing rather than an answer.
async fn wait_for<T>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    animations: bool,
    label: impl Fn() -> &'static str,
    operation: impl Future<Output = io::Result<T>>,
) -> io::Result<Option<T>> {
    let mut events = EventStream::new();
    let mut animation = shimmer_clock();
    let mut shimmer = 0;
    tokio::pin!(operation);

    loop {
        terminal.draw(|frame| draw(frame, label(), animations.then_some(shimmer)))?;
        tokio::select! {
            _ = terminate.recv() => return Ok(None),
            _ = interrupt.recv() => return Ok(None),
            result = &mut operation => return result.map(Some),
            _ = animation.tick(), if animations => shimmer = shimmer.wrapping_add(1),
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

fn draw(frame: &mut Frame, label: &str, tick: Option<usize>) {
    if let Some(tick) = tick {
        terrain::draw(frame, frame.area(), tick);
    }
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
