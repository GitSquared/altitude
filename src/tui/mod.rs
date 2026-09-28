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
    config::{Config, Settings, SettingsError},
    development::DevelopmentEnvironment,
    isolation::{IsolationEngine, OutputStream},
    tasks::{
        Task, TaskExecutionMode, TaskExecutionStatus, TaskOperatorAttentionLevel,
        TaskOperatorNotice, TaskService, TaskServiceError,
    },
    tasks::{TaskEventLoop, TaskExecutionUpdate},
};

mod boot;
mod environment_picker;
mod operator_notices;
mod overview;
mod prompt;
mod shell;
mod utils;

use prompt::{Prompt, PromptAction};
use shell::Shell;

pub use boot::{
    database as boot_database, isolation as boot_isolation, task_resume as boot_task_resume,
};

/// A step of the add-environment prompt chain.
///
/// The chain reuses the single-line prompt rather than introducing a form
/// widget: it already handles editing, cancellation, and resize.
enum SettingsPrompt {
    Name,
    SetupCommand,
}

struct App {
    tasks: Vec<Task>,
    latest_output: HashMap<String, String>,
    notice: Option<String>,
    removing: HashSet<String>,
    selected: usize,
    prompt: Option<Prompt>,
    shells: HashMap<String, Shell>,
    open_shell: Option<String>,
    operator_notices: Vec<TaskOperatorNotice>,
    selected_operator_notice: usize,
    open_operator_notices: bool,
    /// Every configured environment, in configuration order.
    environments: Vec<DevelopmentEnvironment>,
    /// Name of the configured default, used to mark it in the picker.
    default_environment: String,
    selected_environment: usize,
    /// True while the picker is asking which environment a new task uses.
    open_environment_picker: bool,
    /// The new-task prompt held while the picker is open.
    pending_command: Option<String>,
    /// Where `config.toml` lives, so the manage view can persist changes.
    config: Config,
    open_environment_settings: bool,
    settings_prompt: Option<SettingsPrompt>,
    /// Name collected by the add-environment chain, held until its setup
    /// command arrives.
    pending_environment_name: Option<String>,
}

impl App {
    fn new(
        tasks: Vec<Task>,
        config: Config,
        environments: Vec<DevelopmentEnvironment>,
        default_environment: String,
    ) -> Self {
        let latest_output = tasks
            .iter()
            .filter_map(|task| {
                task.current_state
                    .latest_turn_summary
                    .clone()
                    .map(|summary| (task.id.clone(), summary))
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
            operator_notices: Vec::new(),
            selected_operator_notice: 0,
            open_operator_notices: false,
            environments,
            default_environment,
            selected_environment: 0,
            open_environment_picker: false,
            pending_command: None,
            config,
            open_environment_settings: false,
            settings_prompt: None,
            pending_environment_name: None,
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

    /// The only configured environment, when there is exactly one.
    ///
    /// A single environment is not a decision, so the picker is skipped rather
    /// than shown with one row.
    fn single_environment(&self) -> Option<DevelopmentEnvironment> {
        match self.environments.as_slice() {
            [environment] => Some(environment.clone()),
            _ => None,
        }
    }

    /// Hold a new-task prompt until the operator picks an environment.
    ///
    /// Selection starts on the configured default so the common case is a
    /// single Enter.
    fn hold_command_for_environment_pick(&mut self, command: String) {
        self.selected_environment = self
            .environments
            .iter()
            .position(|environment| environment.name == self.default_environment)
            .unwrap_or(0);
        self.pending_command = Some(command);
        self.open_environment_picker = true;
    }

    fn chosen_environment(&self) -> Option<DevelopmentEnvironment> {
        self.environments.get(self.selected_environment).cloned()
    }

    fn select_next_environment(&mut self) {
        if !self.environments.is_empty() {
            self.selected_environment = (self.selected_environment + 1) % self.environments.len();
        }
    }

    fn select_previous_environment(&mut self) {
        if !self.environments.is_empty() {
            self.selected_environment =
                (self.selected_environment + self.environments.len() - 1) % self.environments.len();
        }
    }

    /// Persist the environment configuration.
    ///
    /// The TUI is the only writer of `config.toml`, so the file follows what was
    /// changed here. Hand edits are read at startup.
    fn write_settings(&self) -> Result<(), SettingsError> {
        self.config.save_settings(&Settings {
            default_environment: self.default_environment.clone(),
            environments: self.environments.clone(),
        })
    }

    /// Continue the add-environment prompt chain.
    fn continue_settings_prompt(&mut self, step: SettingsPrompt, input: String) {
        match step {
            SettingsPrompt::Name => {
                self.pending_environment_name = Some(input);
                self.settings_prompt = Some(SettingsPrompt::SetupCommand);
                self.prompt = Some(Prompt::new("setup command"));
            }
            SettingsPrompt::SetupCommand => {
                let Some(name) = self.pending_environment_name.take() else {
                    return;
                };
                let environment = DevelopmentEnvironment {
                    name,
                    setup_command: input,
                };
                // Reject a bad name before it reaches the list, so the overlay
                // never shows an environment that cannot be saved.
                if let Err(error) = environment.validate() {
                    self.notice = Some(error.to_string());
                    return;
                }
                let previous_default = self.default_environment.clone();
                // An empty configuration has no default to inherit, so the first
                // environment added becomes it.
                if self.environments.is_empty() {
                    self.default_environment = environment.name.clone();
                }
                self.environments.push(environment);
                self.selected_environment = self.environments.len() - 1;
                if let Err(error) = self.write_settings() {
                    // Keep the list and the file consistent: a configuration the
                    // file rejected must not linger on screen.
                    self.environments.pop();
                    self.selected_environment = self.selected_environment.saturating_sub(1);
                    self.default_environment = previous_default;
                    self.notice = Some(error.to_string());
                }
            }
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

    fn select_next_operator_notice(&mut self) {
        if !self.operator_notices.is_empty() {
            self.selected_operator_notice =
                (self.selected_operator_notice + 1) % self.operator_notices.len();
        }
    }

    fn select_previous_operator_notice(&mut self) {
        if !self.operator_notices.is_empty() {
            self.selected_operator_notice =
                (self.selected_operator_notice + self.operator_notices.len() - 1)
                    % self.operator_notices.len();
        }
    }
}

pub async fn run<E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    event_loop: TaskEventLoop<E>,
    task_service: TaskService<E>,
    config: Config,
    environments: Vec<DevelopmentEnvironment>,
    default_environment: String,
    tasks: Vec<Task>,
    task_execution_updates: mpsc::UnboundedReceiver<TaskExecutionUpdate>,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    let result = run_loop(
        terminal,
        terminate,
        interrupt,
        event_loop.clone(),
        task_service,
        config,
        environments,
        default_environment,
        tasks,
        task_execution_updates,
    )
    .await;
    let mouse_cleanup = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    event_loop.suspend_all().await.map_err(io::Error::other)?;
    mouse_cleanup?;
    result
}

async fn run_loop<E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    event_loop: TaskEventLoop<E>,
    task_service: TaskService<E>,
    config: Config,
    environments: Vec<DevelopmentEnvironment>,
    default_environment: String,
    tasks: Vec<Task>,
    mut task_execution_updates: mpsc::UnboundedReceiver<TaskExecutionUpdate>,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    execute!(io::stdout(), EnableBracketedPaste)?;
    let mut events = EventStream::new();
    let mut refresh = time::interval(Duration::from_secs(2));
    let mut shell_notice = time::interval(Duration::from_millis(100));
    let mut app = App::new(tasks, config, environments, default_environment);
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
            if app.open_operator_notices {
                operator_notices::draw(frame, &app.operator_notices, app.selected_operator_notice);
            }
            if app.open_environment_picker {
                environment_picker::draw(
                    frame,
                    environment_picker::Purpose::NewTask,
                    &app.environments,
                    &app.default_environment,
                    app.selected_environment,
                );
            }
            if app.open_environment_settings {
                environment_picker::draw(
                    frame,
                    environment_picker::Purpose::Manage,
                    &app.environments,
                    &app.default_environment,
                    app.selected_environment,
                );
            }
        })?;
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = refresh.tick(), if app.open_shell.is_none() => {
                if let Ok(tasks) = event_loop.list().await {
                    app.tasks = tasks;
                    app.selected = app.selected.min(app.tasks.len().saturating_sub(1));
                }
            }
            _ = shell_notice.tick(), if app.shell_has_active_notice() => {}
            Some(event) = task_execution_updates.recv() => match event {
                TaskExecutionUpdate::Created(Ok(task)) => app.replace_task(task),
                TaskExecutionUpdate::Created(Err(error)) => app.notice = Some(error.to_string()),
                TaskExecutionUpdate::Started(Ok(task)) => app.replace_task(task),
                TaskExecutionUpdate::Started(Err(error)) => app.notice = Some(error.to_string()),
                TaskExecutionUpdate::Finished(id, Ok(task)) if !app.is_removing(&id) => {
                    app.remove_shell(&id);
                    app.replace_task(task);
                }
                TaskExecutionUpdate::Finished(id, Err(error)) if !app.is_removing(&id) => set_error(&mut app, &id, error),
                TaskExecutionUpdate::Removed(id, Ok(())) => app.remove_task(&id),
                TaskExecutionUpdate::Removed(id, Err(error)) => {
                    app.cancel_removing(&id);
                    set_error(&mut app, &id, error);
                }
                TaskExecutionUpdate::Output { task_id, stream, bytes } => {
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
                TaskExecutionUpdate::OperatorReady(task) => {
                    let id = task.id.clone();
                    app.replace_task(task);
                    let (rows, cols) = shell::size(terminal.size()?.into());
                    let history = event_loop.cached_terminal_output(&id);
                    event_loop.resize_terminal(&id, rows, cols);
                    app.shells.insert(id.clone(), Shell::new(rows, cols, history));
                    app.open_shell = Some(id);
                }
                TaskExecutionUpdate::Report {
                    task,
                    summary,
                    operator_notices,
                } => {
                    let id = task.id.clone();
                    app.replace_task(task);
                    app.latest_output.insert(id, summary);
                    if let Some(operator_notice) = operator_notices.iter().find(|operator_notice| operator_notice.level == TaskOperatorAttentionLevel::Alarm) {
                        app.notice = Some(format!("ALARM: {}", operator_notice.title));
                    }
                }
                _ => {}
            },
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                if handle_event(event?, terminal, &event_loop, &task_service, &mut app).await? { return Ok(()); }
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
        task.current_state.execution_status = TaskExecutionStatus::Failed;
        task.current_state.execution_pause_reason = None;
        task.current_state.last_error = Some(error.to_string());
    }
}

async fn handle_event<E>(
    event: Event,
    terminal: &DefaultTerminal,
    event_loop: &TaskEventLoop<E>,
    task_service: &TaskService<E>,
    app: &mut App,
) -> io::Result<bool>
where
    E: IsolationEngine + Clone + 'static,
{
    if let Event::Resize(_, _) = event
        && let Some(id) = &app.open_shell
    {
        let (rows, cols) = shell::size(terminal.size()?.into());
        event_loop.resize_terminal(id, rows, cols);
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
            event_loop.send_terminal_input(id, bytes);
        }
        return Ok(false);
    }
    if let Event::Mouse(mouse) = event {
        if let Some(id) = &app.open_shell
            && let Some(shell) = app.shells.get_mut(id)
            && let Some(bytes) = shell.mouse_bytes(mouse, terminal.size()?.into())
        {
            event_loop.send_terminal_input(id, bytes);
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
            Some(PromptAction::Cancel) => {
                app.prompt = None;
                // A cancelled settings prompt must not swallow the next task
                // prompt, so the chain is abandoned here rather than left
                // half-open.
                app.settings_prompt = None;
                app.pending_environment_name = None;
            }
            Some(PromptAction::Submit(command)) => {
                app.prompt = None;
                if let Some(step) = app.settings_prompt.take() {
                    app.continue_settings_prompt(step, command);
                } else if let Some(environment) = app.single_environment() {
                    event_loop.create_claude(command, environment);
                } else if app.environments.is_empty() {
                    app.notice = Some("no environments are configured".into());
                } else {
                    app.hold_command_for_environment_pick(command);
                }
            }
            None => {}
        }
        return Ok(false);
    }
    if let Some(id) = app.open_shell.clone() {
        if shell::is_exit_key(key) {
            event_loop.return_to_autopilot(&id);
            app.hide_shell();
        } else if let Some(shell) = app.shells.get_mut(&id)
            && let Some(bytes) = shell.key_bytes(key)
        {
            event_loop.send_terminal_input(&id, bytes);
        }
        return Ok(false);
    }
    if app.open_environment_settings {
        match key.code {
            KeyCode::Esc | KeyCode::Char('s') => app.open_environment_settings = false,
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => app.select_next_environment(),
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('h') => {
                app.select_previous_environment()
            }
            KeyCode::Char('n') => {
                app.settings_prompt = Some(SettingsPrompt::Name);
                app.prompt = Some(Prompt::new("new environment name"));
            }
            KeyCode::Enter => {
                if let Some(environment) = app.chosen_environment() {
                    let previous =
                        std::mem::replace(&mut app.default_environment, environment.name);
                    if let Err(error) = app.write_settings() {
                        app.default_environment = previous;
                        app.notice = Some(error.to_string());
                    }
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    if app.open_environment_picker {
        match key.code {
            KeyCode::Esc => {
                app.open_environment_picker = false;
                app.pending_command = None;
            }
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => app.select_next_environment(),
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('h') => {
                app.select_previous_environment()
            }
            KeyCode::Enter => {
                if let (Some(command), Some(environment)) =
                    (app.pending_command.take(), app.chosen_environment())
                {
                    app.open_environment_picker = false;
                    event_loop.create_claude(command, environment);
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    if app.open_operator_notices {
        match key.code {
            KeyCode::Esc | KeyCode::Char('i') => app.open_operator_notices = false,
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => app.select_next_operator_notice(),
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('h') => {
                app.select_previous_operator_notice()
            }
            KeyCode::Enter => {
                if let Some(operator_notice) = app
                    .operator_notices
                    .get(app.selected_operator_notice)
                    .cloned()
                {
                    match task_service
                        .acknowledge_operator_notice(&operator_notice.id)
                        .await
                    {
                        Ok(()) => {
                            app.operator_notices.remove(app.selected_operator_notice);
                            app.selected_operator_notice = app
                                .selected_operator_notice
                                .min(app.operator_notices.len().saturating_sub(1));
                        }
                        Err(error) => app.notice = Some(error.to_string()),
                    }
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Char('c') => app.prompt = Some(Prompt::new("what should Claude do?")),
        KeyCode::Char('s') => {
            app.selected_environment = app
                .environments
                .iter()
                .position(|environment| environment.name == app.default_environment)
                .unwrap_or(0);
            app.open_environment_settings = true;
        }
        KeyCode::Char('i') => {
            app.operator_notices = task_service
                .unacknowledged_operator_notices(TaskOperatorAttentionLevel::Nominal)
                .await
                .map_err(io::Error::other)?;
            app.selected_operator_notice = 0;
            app.open_operator_notices = true;
        }
        KeyCode::Tab | KeyCode::Char('j') => app.select_next(),
        KeyCode::BackTab | KeyCode::Char('h') => app.select_previous(),
        KeyCode::Backspace | KeyCode::Delete => {
            if let Some(task) = app.selected_task() {
                tracing::info!(task_id = %task.id, "removing task from overview");
                app.begin_removing(&task.id);
                app.remove_shell(&task.id);
                event_loop.remove(task);
            }
        }
        KeyCode::Enter => {
            if let Some(task) = app.selected_task() {
                if matches!(
                    task.current_state.execution_status,
                    TaskExecutionStatus::Paused | TaskExecutionStatus::Failed
                ) {
                    event_loop.start_operator_control(task);
                    app.notice = Some("restoring Claude in the terminal…".into());
                    return Ok(false);
                }
                if !matches!(
                    task.current_state.execution_status,
                    TaskExecutionStatus::Running
                ) {
                    return Ok(false);
                }
                if matches!(
                    task.current_state.execution_mode,
                    TaskExecutionMode::Autopilot | TaskExecutionMode::AutopilotReport
                ) {
                    event_loop.take_control(&task.id);
                    app.notice = Some("bringing Claude to the terminal…".into());
                    return Ok(false);
                }
                if app.shells.contains_key(&task.id) {
                    let (rows, cols) = shell::size(terminal.size()?.into());
                    event_loop.resize_terminal(&task.id, rows, cols);
                    if let Some(shell) = app.shells.get_mut(&task.id) {
                        shell.resize(rows, cols);
                    }
                    tracing::info!(task_id = %task.id, "reattaching terminal");
                    app.open_shell = Some(task.id);
                    return Ok(false);
                }
                if !event_loop.has_active_command(&task.id) {
                    return Ok(false);
                }
                let (rows, cols) = shell::size(terminal.size()?.into());
                let history = event_loop.cached_terminal_output(&task.id);
                event_loop.resize_terminal(&task.id, rows, cols);
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
