use std::{collections::HashSet, io, time::Duration};

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::{signal::unix::Signal, sync::mpsc, time};

use crate::{
    app::{TaskEvent, TaskRuntime},
    isolation::{IsolationEngine, ShellEvent},
    tasks::{Task, TaskServiceError, TaskStatus},
};

mod boot;
mod overview;
mod prompt;
mod shell;

use prompt::{Prompt, PromptAction};
use shell::Shell;

pub use boot::{
    database as boot_database, isolation as boot_isolation, task_resume as boot_task_resume,
};

struct App {
    tasks: Vec<Task>,
    removing: HashSet<String>,
    selected: usize,
    prompt: Option<Prompt>,
    shell: Option<Shell>,
}

impl App {
    fn new(tasks: Vec<Task>) -> Self {
        Self {
            tasks,
            removing: HashSet::new(),
            selected: 0,
            prompt: None,
            shell: None,
        }
    }

    fn replace_task(&mut self, task: Task) {
        if let Some(current) = self.tasks.iter_mut().find(|current| current.id == task.id) {
            *current = task;
        } else {
            self.tasks.insert(0, task);
        }
    }

    fn select_next(&mut self) {
        if !self.tasks.is_empty() {
            self.selected = (self.selected + 1) % self.tasks.len();
        }
    }

    fn select_previous(&mut self) {
        if !self.tasks.is_empty() {
            self.selected = (self.selected + self.tasks.len() - 1) % self.tasks.len();
        }
    }

    fn selected_task(&self) -> Option<Task> {
        self.tasks.get(self.selected).cloned()
    }

    fn remove_task(&mut self, id: &str) {
        self.removing.remove(id);
        if let Some(index) = self.tasks.iter().position(|task| task.id == id) {
            self.tasks.remove(index);
            self.selected = self.selected.min(self.tasks.len().saturating_sub(1));
        }
    }

    fn begin_removing(&mut self, id: &str) {
        self.removing.insert(id.to_owned());
    }

    fn cancel_removing(&mut self, id: &str) {
        self.removing.remove(id);
    }

    fn is_removing(&self, id: &str) -> bool {
        self.removing.contains(id)
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
    runtime: TaskRuntime<E>,
    tasks: Vec<Task>,
    task_events: mpsc::UnboundedReceiver<TaskEvent>,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    let result = run_loop(
        terminal,
        terminate,
        interrupt,
        runtime.clone(),
        tasks,
        task_events,
    )
    .await;
    runtime.suspend_all().await.map_err(io::Error::other)?;
    result
}

async fn run_loop<E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    runtime: TaskRuntime<E>,
    tasks: Vec<Task>,
    mut task_events: mpsc::UnboundedReceiver<TaskEvent>,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    let mut events = EventStream::new();
    let mut refresh = time::interval(Duration::from_secs(2));
    let mut app = App::new(tasks);
    let (shell_tx, mut shell_events) = mpsc::unbounded_channel();

    loop {
        terminal.draw(|frame| {
            overview::draw(frame, &app.tasks, app.selected);
            if let Some(prompt) = &app.prompt {
                prompt.draw(frame);
            }
            if let Some(shell) = &app.shell {
                shell.draw(frame);
            }
        })?;
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = refresh.tick(), if app.shell.is_none() => {
                if let Ok(tasks) = runtime.list().await {
                    app.tasks = tasks;
                    app.selected = app.selected.min(app.tasks.len().saturating_sub(1));
                }
            }
            Some(event) = task_events.recv() => match event {
                TaskEvent::Started(Ok(task)) => app.replace_task(task),
                TaskEvent::Started(Err(_)) => {}
                TaskEvent::Finished(id, Ok(task)) if !app.is_removing(&id) => app.replace_task(task),
                TaskEvent::Finished(id, Err(error)) if !app.is_removing(&id) => set_error(&mut app, &id, error),
                TaskEvent::Removed(id, Ok(())) => app.remove_task(&id),
                TaskEvent::Removed(id, Err(error)) => {
                    app.cancel_removing(&id);
                    set_error(&mut app, &id, error);
                }
                _ => {}
            },
            Some(event) = shell_events.recv() => if let Some(shell) = &mut app.shell { shell.process(event); },
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                if handle_event(event?, terminal, &runtime, &shell_tx, &mut app).await? { return Ok(()); }
            }
        }
    }
}

fn set_error(app: &mut App, id: &str, error: TaskServiceError) {
    if let Some(task) = app.tasks.iter_mut().find(|task| task.id == id) {
        task.status = TaskStatus::Errored;
        task.last_error = Some(error.to_string());
    }
}

async fn handle_event<E>(
    event: Event,
    terminal: &DefaultTerminal,
    runtime: &TaskRuntime<E>,
    shell_tx: &mpsc::UnboundedSender<ShellEvent>,
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
    if let Some(prompt) = &mut app.prompt {
        match prompt.handle_key(key) {
            Some(PromptAction::Cancel) => app.prompt = None,
            Some(PromptAction::Submit(command)) => {
                app.prompt = None;
                runtime.create(command);
            }
            None => {}
        }
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
        KeyCode::Char('c') => app.prompt = Some(Prompt::new()),
        KeyCode::Tab => app.select_next(),
        KeyCode::BackTab => app.select_previous(),
        KeyCode::Backspace | KeyCode::Delete => {
            if let Some(task) = app.selected_task() {
                app.begin_removing(&task.id);
                runtime.remove(task);
            }
        }
        KeyCode::Enter => {
            if let Some(task) = app.selected_task()
                && let Some(sandbox_id) = task.sandbox_id
                && matches!(task.status, TaskStatus::InProgress | TaskStatus::Errored)
            {
                let (rows, cols) = shell::size(terminal.size()?.into());
                let session = runtime
                    .open_shell(sandbox_id, rows, cols)
                    .await
                    .map_err(io::Error::other)?;
                let mut shell_events = session.events;
                let shell_tx = shell_tx.clone();
                tokio::spawn(async move {
                    while let Some(event) = shell_events.recv().await {
                        if shell_tx.send(event).is_err() {
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
