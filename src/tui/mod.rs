use std::{
    collections::{HashMap, HashSet},
    io,
    time::Duration,
};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyModifiers,
    },
    execute,
};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::{signal::unix::Signal, sync::mpsc, time};

use crate::{
    app::{TaskEvent, TaskRuntime},
    isolation::{IsolationEngine, OutputStream},
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
    latest_output: HashMap<String, String>,
    notice: Option<String>,
    removing: HashSet<String>,
    selected: usize,
    prompt: Option<Prompt>,
    shells: HashMap<String, Shell>,
    open_shell: Option<String>,
}

impl App {
    fn new(tasks: Vec<Task>) -> Self {
        let latest_output = tasks
            .iter()
            .filter_map(|task| {
                task.last_output.as_ref().map(|output| {
                    (
                        task.id.clone(),
                        String::from_utf8_lossy(output).into_owned(),
                    )
                })
            })
            .collect();
        Self {
            tasks,
            latest_output,
            notice: None,
            removing: HashSet::new(),
            selected: 0,
            prompt: None,
            shells: HashMap::new(),
            open_shell: None,
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

    fn append_output(&mut self, id: String, stream: OutputStream, bytes: Vec<u8>) {
        let output = self.latest_output.entry(id).or_default();
        if matches!(stream, OutputStream::Stderr) {
            output.push_str("stderr · ");
        }
        output.push_str(&String::from_utf8_lossy(&bytes));
        if output.len() > 240 {
            let mut start = output.len() - 240;
            while !output.is_char_boundary(start) {
                start += 1;
            }
            output.drain(..start);
        }
    }

    fn hide_shell(&mut self) {
        tracing::info!(task_id = ?self.open_shell, "detaching terminal");
        self.open_shell = None;
    }

    fn remove_shell(&mut self, id: &str) {
        self.shells.remove(id);
        if self.open_shell.as_deref() == Some(id) {
            self.open_shell = None;
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
    let mouse_cleanup = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    runtime.suspend_all().await.map_err(io::Error::other)?;
    mouse_cleanup?;
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
    execute!(io::stdout(), EnableBracketedPaste)?;
    let mut events = EventStream::new();
    let mut refresh = time::interval(Duration::from_secs(2));
    let mut shell_notice = time::interval(Duration::from_millis(100));
    let mut app = App::new(tasks);
    let mut mouse_captured = false;
    loop {
        if mouse_captured != app.open_shell.is_some() {
            mouse_captured = app.open_shell.is_some();
            if mouse_captured {
                execute!(io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(io::stdout(), DisableMouseCapture)?;
            }
        }
        terminal.draw(|frame| {
            overview::draw(
                frame,
                &app.tasks,
                app.selected,
                &app.latest_output,
                app.notice.as_deref(),
            );
            if let Some(prompt) = &app.prompt {
                prompt.draw(frame);
            }
            if let Some(shell) = app.open_shell.as_ref().and_then(|id| app.shells.get(id)) {
                shell.draw(frame);
            }
        })?;
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = refresh.tick(), if app.open_shell.is_none() => {
                if let Ok(tasks) = runtime.list().await {
                    app.tasks = tasks;
                    app.selected = app.selected.min(app.tasks.len().saturating_sub(1));
                }
            }
            _ = shell_notice.tick(), if app.shell_has_active_notice() => {}
            Some(event) = task_events.recv() => match event {
                TaskEvent::Created(Ok(task)) => app.replace_task(task),
                TaskEvent::Created(Err(error)) => app.notice = Some(error.to_string()),
                TaskEvent::Started(Ok(task)) => app.replace_task(task),
                TaskEvent::Started(Err(error)) => app.notice = Some(error.to_string()),
                TaskEvent::Finished(id, Ok(task)) if !app.is_removing(&id) => {
                    app.remove_shell(&id);
                    app.replace_task(task);
                }
                TaskEvent::Finished(id, Err(error)) if !app.is_removing(&id) => set_error(&mut app, &id, error),
                TaskEvent::Removed(id, Ok(())) => app.remove_task(&id),
                TaskEvent::Removed(id, Err(error)) => {
                    app.cancel_removing(&id);
                    set_error(&mut app, &id, error);
                }
                TaskEvent::Output { task_id, stream, bytes } => {
                    app.append_output(task_id.clone(), stream, bytes.clone());
                    if let Some(shell) = app.shells.get_mut(&task_id)
                        && let Some(text) = shell.process(bytes)
                    {
                        match copy_to_clipboard(text) {
                            Ok(()) => {
                                shell.show_clipboard_confirmation();
                                tracing::info!(task_id = %task_id, "copied terminal text to clipboard");
                            }
                            Err(error) => tracing::error!(task_id = %task_id, %error, "could not copy terminal text"),
                        }
                    }
                }
                _ => {}
            },
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                if handle_event(event?, terminal, &runtime, &mut app).await? { return Ok(()); }
            }
        }
    }
}

impl App {
    fn shell_has_active_notice(&self) -> bool {
        self.open_shell
            .as_ref()
            .and_then(|id| self.shells.get(id))
            .is_some_and(Shell::has_active_notice)
    }
}

fn copy_to_clipboard(text: String) -> Result<(), arboard::Error> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text)
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
    app: &mut App,
) -> io::Result<bool>
where
    E: IsolationEngine + Clone + 'static,
{
    if let Event::Resize(_, _) = event
        && let Some(id) = &app.open_shell
    {
        let (rows, cols) = shell::size(terminal.size()?.into());
        runtime.resize(id, rows, cols);
        if let Some(shell) = app.shells.get_mut(id) {
            shell.resize(rows, cols);
        }
        return Ok(false);
    }
    if let Event::Paste(text) = event {
        if let Some(prompt) = &mut app.prompt {
            prompt.paste(&text);
        } else if let Some(id) = &app.open_shell
            && let Some(shell) = app.shells.get(id)
            && let Some(bytes) = shell.paste_bytes(text)
        {
            runtime.send_input(id, bytes);
        }
        return Ok(false);
    }
    if let Event::Mouse(mouse) = event {
        if let Some(id) = &app.open_shell
            && let Some(shell) = app.shells.get_mut(id)
            && let Some(bytes) = shell.mouse_bytes(mouse, terminal.size()?.into())
        {
            runtime.send_input(id, bytes);
        }
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
                runtime.create_claude(command);
            }
            None => {}
        }
        return Ok(false);
    }
    if let Some(id) = app.open_shell.clone() {
        if shell::is_exit_key(key) {
            app.hide_shell();
        } else if let Some(shell) = app.shells.get_mut(&id)
            && let Some(bytes) = shell.key_bytes(key)
        {
            runtime.send_input(&id, bytes);
        }
        return Ok(false);
    }
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Char('c') => app.prompt = Some(Prompt::new("what should Claude do?")),
        KeyCode::Tab => app.select_next(),
        KeyCode::BackTab => app.select_previous(),
        KeyCode::Backspace | KeyCode::Delete => {
            if let Some(task) = app.selected_task() {
                tracing::info!(task_id = %task.id, "removing task from overview");
                app.begin_removing(&task.id);
                app.remove_shell(&task.id);
                runtime.remove(task);
            }
        }
        KeyCode::Enter => {
            if let Some(task) = app.selected_task()
                && matches!(task.status, TaskStatus::InProgress)
            {
                if app.shells.contains_key(&task.id) {
                    let (rows, cols) = shell::size(terminal.size()?.into());
                    runtime.resize(&task.id, rows, cols);
                    if let Some(shell) = app.shells.get_mut(&task.id) {
                        shell.resize(rows, cols);
                    }
                    tracing::info!(task_id = %task.id, "reattaching terminal");
                    app.open_shell = Some(task.id);
                    return Ok(false);
                }
                if !runtime.has_command(&task.id) {
                    return Ok(false);
                }
                let (rows, cols) = shell::size(terminal.size()?.into());
                let history = runtime
                    .output_history(&task.id)
                    .await
                    .map_err(io::Error::other)?;
                runtime.resize(&task.id, rows, cols);
                app.shells
                    .insert(task.id.clone(), Shell::new(rows, cols, history));
                app.open_shell = Some(task.id);
                tracing::info!("attached terminal");
            }
        }
        _ => {}
    }
    Ok(false)
}
