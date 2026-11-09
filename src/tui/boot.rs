use std::{error::Error, future::Future, io, time::Instant};

use crossterm::event::{EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    text::{Line, Span},
    widgets::Paragraph,
};
use tokio::{signal::unix::Signal, sync::watch, time};

use crate::tui::{
    background::terrain,
    keys,
    widgets::{palette, progress, question},
};
use crate::{
    isolation::{ImagePull, IsolationStatus, PackageInstall},
    tui::widgets::shimmer::{shimmer, shimmer_clock},
};

use crate::brand;

const TITLE: &str = brand::TITLE;

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
        || BootStatus::Working("migrating the database…"),
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
        || isolation_status(&shown.borrow()),
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
        || BootStatus::Working("resuming tasks…"),
        async { Ok(resume.await) },
    )
    .await
}

async fn wait_for<T>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    animations: bool,
    label: impl Fn() -> BootStatus,
    operation: impl Future<Output = io::Result<T>>,
) -> io::Result<Option<T>> {
    let mut events = EventStream::new();
    let mut repaint = shimmer_clock();
    let mut backdrop = time::interval(terrain::CLOCK);
    backdrop.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut terrain = terrain::Terrain::default();
    let mut pulled = Pull::default();
    let mut shimmer = 0;
    tokio::pin!(operation);

    loop {
        let status = label();
        let rate = pulled.rate(status);
        terminal.draw(|frame| {
            draw(
                frame,
                status,
                rate,
                animations.then_some(shimmer),
                animations.then_some(&mut terrain),
            )
        })?;
        tokio::select! {
            _ = terminate.recv() => return Ok(None),
            _ = interrupt.recv() => return Ok(None),
            result = &mut operation => return result.map(Some),
            _ = repaint.tick() => {
                // The clock repaints the screen so progress keeps moving even
                // when animations are off; only the shimmer is animation.
                if animations {
                    shimmer = shimmer.wrapping_add(1);
                }
            },
            _ = backdrop.tick(), if animations => terrain.advance(),
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

pub async fn confirm(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    animations: bool,
    title: &str,
    question: &str,
    answers: &[&str],
) -> io::Result<Option<usize>> {
    let mut events = EventStream::new();
    let mut backdrop = time::interval(terrain::CLOCK);
    backdrop.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut terrain = terrain::Terrain::default();
    let mut chosen = 0;
    loop {
        terminal.draw(|frame| {
            draw_question(
                frame,
                title,
                question,
                answers,
                chosen,
                animations.then_some(&mut terrain),
            );
        })?;
        tokio::select! {
            _ = terminate.recv() => return Ok(None),
            _ = interrupt.recv() => return Ok(None),
            _ = backdrop.tick(), if animations => terrain.advance(),
            event = events.next() => {
                let Some(event) = event else { return Ok(None) };
                let Some(key) = event?.as_key_press_event() else { continue };
                if keys::is_dismiss(key) {
                    return Ok(None);
                }
                match key.code {
                    KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                        chosen = chosen.saturating_sub(1);
                    }
                    KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                        chosen = (chosen + 1).min(answers.len().saturating_sub(1));
                    }
                    KeyCode::Enter => return Ok(Some(chosen)),
                    _ => {}
                }
            }
        }
    }
}

fn draw_question(
    frame: &mut Frame,
    title: &str,
    question: &str,
    answers: &[&str],
    chosen: usize,
    mut terrain: Option<&mut terrain::Terrain>,
) {
    let area = frame.area();
    if let Some(terrain) = &mut terrain {
        terrain.mark(frame.buffer_mut(), area);
    }
    question::draw(frame, area, title, question, answers, chosen);
    if let Some(terrain) = &mut terrain {
        terrain.draw(frame.buffer_mut(), area);
    }
}

fn isolation_status(status: &IsolationStatus) -> BootStatus {
    match status {
        IsolationStatus::Starting | IsolationStatus::InstallingRuntime => {
            BootStatus::Working("starting up…")
        }
        IsolationStatus::PreparingBaseImage(pull) => BootStatus::Downloading {
            label: "fetching the sandbox image…",
            pull: *pull,
        },
        IsolationStatus::PreparingBasePackages(install) => BootStatus::Installing {
            label: "installing base packages…",
            install: *install,
        },
        IsolationStatus::Ready | IsolationStatus::Failed(_) => BootStatus::Working("starting up…"),
    }
}

#[derive(Clone, Copy)]
enum BootStatus {
    Working(&'static str),
    Downloading {
        label: &'static str,
        pull: ImagePull,
    },
    Installing {
        label: &'static str,
        install: PackageInstall,
    },
}

impl BootStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Working(label)
            | Self::Downloading { label, .. }
            | Self::Installing { label, .. } => label,
        }
    }

    fn pull(self) -> Option<ImagePull> {
        // The manifest says how large the image is before a byte of it moves, and
        // an image that is already here moves none: neither is a download to
        // report, so the line waits for the first byte.
        match self {
            Self::Downloading { pull, .. } if pull.downloaded_bytes > 0 => Some(pull),
            _ => None,
        }
    }
}

// The pull's own speed, which the status only carries the bytes of: how fast the
// image is coming down is what says whether the wait is worth watching.
#[derive(Default)]
struct Pull {
    sampled: Option<(Instant, u64)>,
    rate: Option<f64>,
}

impl Pull {
    fn rate(&mut self, status: BootStatus) -> Option<f64> {
        let Some(pull) = status.pull() else {
            self.sampled = None;
            return None;
        };
        let now = Instant::now();
        match self.sampled {
            Some((then, was)) => {
                let elapsed = now.duration_since(then).as_secs_f64();
                if elapsed >= 0.25 && pull.downloaded_bytes >= was {
                    let sample = (pull.downloaded_bytes - was) as f64 / elapsed;
                    self.rate = Some(match self.rate {
                        Some(previous) => (previous + sample) / 2.0,
                        None => sample,
                    });
                    self.sampled = Some((now, pull.downloaded_bytes));
                }
            }
            None => self.sampled = Some((now, pull.downloaded_bytes)),
        }
        self.rate
    }
}

fn draw(
    frame: &mut Frame,
    status: BootStatus,
    rate: Option<f64>,
    tick: Option<usize>,
    terrain: Option<&mut terrain::Terrain>,
) {
    let area = frame.area();
    if let Some(terrain) = terrain.as_ref() {
        terrain.mark(frame.buffer_mut(), area);
    }
    // What is being measured, if anything: the numbers are one line, and the bar
    // that used to sit above them says nothing the percentage does not.
    let measured = match status {
        BootStatus::Downloading { .. } => status.pull().map(|pull| downloaded(pull, rate)),
        BootStatus::Installing { install, .. } if install.total > 0 => Some(installed(install)),
        _ => None,
    };
    let mut rows = vec![
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ];
    if measured.is_some() {
        rows.push(Constraint::Length(1));
    }
    rows.push(Constraint::Fill(1));
    let rows = Layout::vertical(rows).split(area);

    frame.render_widget(
        Paragraph::new(Line::styled(TITLE, palette::heading())).centered(),
        rows[1],
    );

    frame.render_widget(
        Paragraph::new(Line::from_iter(shimmer(status.label(), tick))).centered(),
        rows[3],
    );

    if let Some(measured) = measured {
        frame.render_widget(
            Paragraph::new(measured).centered().style(palette::muted()),
            rows[4],
        );
    }

    if let Some(terrain) = terrain {
        terrain.draw(frame.buffer_mut(), area);
    }
}

fn downloaded(pull: ImagePull, rate: Option<f64>) -> Line<'static> {
    // A pull that outran its own manifest is still a pull that is nearly done.
    let total = pull.total_bytes.max(pull.downloaded_bytes);
    let percent = pull.downloaded_bytes * 100 / total;
    let mut line = vec![
        Span::styled(format!("{percent:>3}%"), palette::primary()),
        Span::raw(" · "),
        Span::raw(format!(
            "{} of {}",
            progress::amount(pull.downloaded_bytes),
            progress::amount(total)
        )),
    ];
    // A pull at the end is not moving any more: what is left of the wait is the
    // sandbox unpacking, and a speed that keeps falling would say otherwise.
    if let Some(rate) = rate.filter(|_| pull.downloaded_bytes < total) {
        line.push(Span::raw(" · "));
        line.push(Span::raw(format!("{}/s", progress::amount(rate as u64))));
    }
    Line::from(line)
}

fn installed(install: PackageInstall) -> Line<'static> {
    Line::styled(
        format!(
            "{} of {}",
            install.installed.min(install.total),
            install.total
        ),
        palette::primary(),
    )
}
