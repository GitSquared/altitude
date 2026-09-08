use std::{io, time::Duration};

use crate::isolation::{
    IsolationEngine, IsolationError, IsolationStatus, Sandbox, SandboxStatus, ShellEvent,
};
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use throbber_widgets_tui::ThrobberState;
use tokio::{
    signal::unix::Signal,
    sync::{mpsc, watch},
    time,
};

mod overview;
mod shell;

use shell::Shell;

struct App {
    next_sandbox: usize,
    sandboxes: Vec<Sandbox>,
    selected: usize,
    shell: Option<Shell>,
}

enum AppEvent {
    SandboxCreated(String, Result<Sandbox, IsolationError>),
    SandboxRemoved(String, Result<(), IsolationError>),
    Shell(ShellEvent),
}

impl App {
    fn new() -> Self {
        Self {
            next_sandbox: 1,
            sandboxes: Vec::new(),
            selected: 0,
            shell: None,
        }
    }

    fn begin_sandbox(&mut self) -> String {
        let id = format!("altitude-{}", self.next_sandbox);
        self.next_sandbox += 1;
        self.sandboxes.push(Sandbox {
            id: id.clone(),
            status: SandboxStatus::Starting,
        });
        id
    }

    fn finish_sandbox(&mut self, id: String, result: Result<Sandbox, IsolationError>) {
        let Some(sandbox) = self.sandboxes.iter_mut().find(|sandbox| sandbox.id == id) else {
            return;
        };

        sandbox.status = match result {
            Ok(created) => created.status,
            Err(error) => SandboxStatus::Failed(error.to_string()),
        };
    }

    fn refresh_sandboxes(&mut self, refreshed: Vec<Sandbox>) {
        for refreshed in refreshed {
            if let Some(sandbox) = self
                .sandboxes
                .iter_mut()
                .find(|sandbox| sandbox.id == refreshed.id)
            {
                sandbox.status = refreshed.status;
            }
        }
    }

    fn select_next(&mut self) {
        if !self.sandboxes.is_empty() {
            self.selected = (self.selected + 1) % self.sandboxes.len();
        }
    }

    fn select_previous(&mut self) {
        if !self.sandboxes.is_empty() {
            self.selected = (self.selected + self.sandboxes.len() - 1) % self.sandboxes.len();
        }
    }

    fn selected_shellable_id(&self) -> Option<String> {
        let sandbox = self.sandboxes.get(self.selected)?;
        matches!(
            sandbox.status,
            SandboxStatus::Running | SandboxStatus::Stopped
        )
        .then(|| sandbox.id.clone())
    }

    fn selected_id(&self) -> Option<String> {
        self.sandboxes
            .get(self.selected)
            .map(|sandbox| sandbox.id.clone())
    }

    fn remove_sandbox(&mut self, id: String) {
        let Some(index) = self.sandboxes.iter().position(|sandbox| sandbox.id == id) else {
            return;
        };
        self.sandboxes.remove(index);
        self.selected = self.selected.min(self.sandboxes.len().saturating_sub(1));
    }

    fn close_shell(&mut self) {
        if let Some(shell) = self.shell.take() {
            shell.close();
        }
    }
}

pub async fn run<E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    mut isolation_status: watch::Receiver<IsolationStatus>,
    isolation: E,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    let result = run_loop(
        terminal,
        terminate,
        interrupt,
        &mut isolation_status,
        isolation.clone(),
    )
    .await;
    let cleanup = isolation.stop_all().await.map_err(io::Error::other);
    result.and(cleanup)
}

async fn run_loop<E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    isolation_status: &mut watch::Receiver<IsolationStatus>,
    isolation: E,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    let mut events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(80));
    let mut status_refresh = time::interval(Duration::from_secs(2));
    let mut spinner = ThrobberState::default();
    let mut status_open = true;
    let mut app = App::new();
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();

    loop {
        terminal.draw(|frame| {
            overview::draw(
                frame,
                &isolation_status.borrow(),
                &spinner,
                &app.sandboxes,
                app.selected,
            );
            if let Some(shell) = &app.shell {
                shell.draw(frame);
            }
        })?;

        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            changed = isolation_status.changed(), if status_open => {
                status_open = changed.is_ok();
            }
            _ = animation.tick(), if isolation_status.borrow().is_pending() => {
                spinner.calc_next();
            }
            _ = status_refresh.tick(), if !app.sandboxes.is_empty() && app.shell.is_none() => {
                let ids = app.sandboxes.iter().map(|sandbox| sandbox.id.clone()).collect();
                if let Ok(sandboxes) = isolation.statuses(ids).await {
                    app.refresh_sandboxes(sandboxes);
                }
            }
            Some(event) = app_rx.recv() => match event {
                AppEvent::SandboxCreated(id, result) => app.finish_sandbox(id, result),
                AppEvent::SandboxRemoved(id, Ok(())) => app.remove_sandbox(id),
                AppEvent::SandboxRemoved(id, Err(error)) => {
                    app.finish_sandbox(id, Err(error));
                }
                AppEvent::Shell(event) => {
                    if let Some(shell) = &mut app.shell {
                        shell.process(event);
                    }
                }
            },
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                if handle_event(event?, terminal, isolation_status, &isolation, &app_tx, &mut app).await? {
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_event<E>(
    event: Event,
    terminal: &DefaultTerminal,
    isolation_status: &watch::Receiver<IsolationStatus>,
    isolation: &E,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
    app: &mut App,
) -> io::Result<bool>
where
    E: IsolationEngine + Clone + 'static,
{
    if let Event::Resize(_, _) = event
        && let Some(shell) = &app.shell
    {
        shell.resize(terminal.size()?.into());
        return Ok(false);
    }

    let Event::Key(key) = event else {
        return Ok(false);
    };
    if !key.is_press() {
        return Ok(false);
    }

    if app.shell.is_some() {
        if shell::is_exit_key(key) {
            app.close_shell();
        } else if let Some(shell) = &app.shell {
            shell.send_key(key);
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Char('c') if matches!(*isolation_status.borrow(), IsolationStatus::Ready) => {
            let id = app.begin_sandbox();
            let engine = isolation.clone();
            let app_tx = app_tx.clone();
            tokio::spawn(async move {
                let result = engine.create(id.clone()).await;
                let _ = app_tx.send(AppEvent::SandboxCreated(id, result));
            });
        }
        KeyCode::Tab => app.select_next(),
        KeyCode::BackTab => app.select_previous(),
        KeyCode::Backspace | KeyCode::Delete => {
            if let Some(id) = app.selected_id() {
                let engine = isolation.clone();
                let app_tx = app_tx.clone();
                tokio::spawn(async move {
                    let result = engine.remove(id.clone()).await;
                    let _ = app_tx.send(AppEvent::SandboxRemoved(id, result));
                });
            }
        }
        KeyCode::Enter => {
            if let Some(id) = app.selected_shellable_id() {
                let (rows, cols) = shell::size(terminal.size()?.into());
                let session = isolation
                    .open_shell(id, rows, cols)
                    .await
                    .map_err(io::Error::other)?;
                let mut shell_events = session.events;
                let app_tx = app_tx.clone();
                tokio::spawn(async move {
                    while let Some(event) = shell_events.recv().await {
                        if app_tx.send(AppEvent::Shell(event)).is_err() {
                            break;
                        }
                    }
                });
                app.shell = Some(Shell::new(rows, cols, session.commands));
            }
        }
        _ => {}
    }
    Ok(false)
}
