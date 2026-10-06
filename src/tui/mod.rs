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
use tui_term::vt100;

use crate::{
    agent::AgentKind,
    config::{Config, SettingsError, SettingsFile},
    development::DevelopmentEnvironment,
    isolation::{EnvironmentProgress, IsolationEngine, OutputStream},
    tasks::{
        Task, TaskExecutionMode, TaskExecutionStatus, TaskOperatorSignal, TaskService,
        TaskServiceError,
    },
    tasks::{TaskEventLoop, TaskExecutionUpdate},
};

mod boot;
mod environment_picker;
mod operator_signals;
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
    Harness,
    SetupCommand,
}

pub struct App {
    tasks: Vec<Task>,
    latest_output: HashMap<String, String>,
    output_previews: HashMap<String, vt100::Parser>,
    /// What each provisioning task is waiting on, refreshed while any task is
    /// pending.
    environment_builds: HashMap<String, EnvironmentProgress>,
    /// Advances the highlight on that waiting text.
    shimmer: usize,
    /// What the host will run at once, so a wait can say whether there is room.
    /// Absent until the first refresh, and never stale by more than one interval.
    capacity: Option<usize>,
    notice: Option<String>,
    removing: HashSet<String>,
    selected: usize,
    first_visible_task: usize,
    prompt: Option<Prompt>,
    shells: HashMap<String, Shell>,
    open_shell: Option<String>,
    operator_signals: Vec<TaskOperatorSignal>,
    selected_operator_signal: usize,
    /// Which of the selected signal's actions has focus.
    selected_signal_action: usize,
    operator_signal_scroll: u16,
    open_operator_signals: bool,
    answering_operator_signal: Option<String>,
    selected_environment: usize,
    /// True while the picker is asking which environment a new task uses.
    open_environment_picker: bool,
    /// The new-task prompt held while the picker is open.
    pending_command: Option<String>,
    /// Where `config.toml` lives, so the manage view can persist changes.
    config: Config,
    settings_file: Option<SettingsFile>,
    open_environment_settings: bool,
    settings_prompt: Option<SettingsPrompt>,
    /// Name collected by the add-environment chain, held until its harness and
    /// setup command arrive.
    pending_environment_name: Option<String>,
    pending_environment_harness: Option<AgentKind>,
}

impl App {
    pub fn new(
        tasks: Vec<Task>,
        config: Config,
        settings: Result<SettingsFile, SettingsError>,
    ) -> Self {
        let latest_output = tasks
            .iter()
            .filter_map(|task| {
                task.facts
                    .latest_turn_summary
                    .clone()
                    .map(|summary| (task.id.clone(), summary))
            })
            .collect();
        let (settings_file, settings_error) = match settings {
            Ok(file) => (Some(file), None),
            Err(error) => (
                None,
                Some(format!(
                    "Settings unavailable. Fix {}, then press s to reload.\n{error}",
                    config.settings_path().display()
                )),
            ),
        };
        Self {
            tasks,
            latest_output,
            output_previews: HashMap::new(),
            notice: settings_error,
            removing: HashSet::new(),
            selected: 0,
            first_visible_task: 0,
            prompt: None,
            shells: HashMap::new(),
            open_shell: None,
            operator_signals: Vec::new(),
            selected_operator_signal: 0,
            selected_signal_action: 0,
            operator_signal_scroll: 0,
            open_operator_signals: false,
            answering_operator_signal: None,
            selected_environment: 0,
            open_environment_picker: false,
            pending_command: None,
            config,
            settings_file,
            open_environment_settings: false,
            settings_prompt: None,
            pending_environment_name: None,
            pending_environment_harness: None,
            environment_builds: HashMap::new(),
            shimmer: 0,
            capacity: None,
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

    fn environments(&self) -> &[DevelopmentEnvironment] {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().environments.as_slice())
            .unwrap_or_default()
    }

    fn default_environment(&self) -> &str {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().default_environment.as_str())
            .unwrap_or_default()
    }

    /// The only configured environment, when there is exactly one.
    ///
    /// A single environment is not a decision, so the picker is skipped rather
    /// than shown with one row.
    fn single_environment(&self) -> Option<DevelopmentEnvironment> {
        match self.environments() {
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
            .environments()
            .iter()
            .position(|environment| {
                environment
                    .name
                    .eq_ignore_ascii_case(self.default_environment())
            })
            .unwrap_or(0);
        self.pending_command = Some(command);
        self.open_environment_picker = true;
    }

    fn chosen_environment(&self) -> Option<DevelopmentEnvironment> {
        self.environments().get(self.selected_environment).cloned()
    }

    fn select_next_environment(&mut self) {
        if !self.environments().is_empty() {
            self.selected_environment = (self.selected_environment + 1) % self.environments().len();
        }
    }

    fn select_previous_environment(&mut self) {
        if !self.environments().is_empty() {
            self.selected_environment = (self.selected_environment + self.environments().len() - 1)
                % self.environments().len();
        }
    }

    fn reload_settings(&mut self) -> bool {
        match self.config.load_or_create_settings() {
            Ok(file) => {
                self.settings_file = Some(file);
                self.notice = None;
                true
            }
            Err(error) => {
                let message = format!(
                    "Settings unavailable. Fix {}, then press s to reload.\n{error}",
                    self.config.settings_path().display()
                );
                self.settings_file = None;
                self.notice = Some(message);
                false
            }
        }
    }

    /// Continue the add-environment prompt chain.
    fn continue_settings_prompt(&mut self, step: SettingsPrompt, input: String) {
        match step {
            SettingsPrompt::Name => {
                self.pending_environment_name = Some(input);
                self.settings_prompt = Some(SettingsPrompt::Harness);
                self.prompt = Some(Prompt::new("harness: claude or pi"));
            }
            SettingsPrompt::Harness => {
                let harness = if input.trim().is_empty() {
                    AgentKind::default()
                } else {
                    match input.trim().parse::<AgentKind>() {
                        Ok(harness) => harness,
                        Err(error) => {
                            // The chain keeps its collected name and asks again
                            // rather than discarding the operator's input.
                            self.notice = Some(error.to_string());
                            self.settings_prompt = Some(SettingsPrompt::Harness);
                            self.prompt = Some(Prompt::new("harness: claude or pi"));
                            return;
                        }
                    }
                };
                self.pending_environment_harness = Some(harness);
                self.settings_prompt = Some(SettingsPrompt::SetupCommand);
                self.prompt = Some(Prompt::new("setup command"));
            }
            SettingsPrompt::SetupCommand => {
                let (Some(name), Some(harness)) = (
                    self.pending_environment_name.take(),
                    self.pending_environment_harness.take(),
                ) else {
                    return;
                };
                let environment = DevelopmentEnvironment {
                    name,
                    setup_command: input,
                    harness,
                };
                let Some(file) = self.settings_file.as_mut() else {
                    return;
                };
                let mut proposed = file.settings().clone();
                proposed.environments.push(environment);
                match file.save(proposed) {
                    Ok(()) => {
                        self.selected_environment = file.settings().environments.len() - 1;
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
        }
    }

    fn selected_task(&self) -> Option<Task> {
        self.tasks.get(self.selected).cloned()
    }

    fn remove_task(&mut self, id: &str) {
        self.output_previews.remove(id);
        self.latest_output.remove(id);
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

    fn append_output(&mut self, id: String, _stream: OutputStream, bytes: Vec<u8>) {
        let parser = self
            .output_previews
            .entry(id.clone())
            .or_insert_with(|| vt100::Parser::new(24, 120, 0));
        parser.process(&bytes);
        if let Some(line) = parser
            .screen()
            .contents()
            .lines()
            .rfind(|line| !line.trim().is_empty())
        {
            self.latest_output.insert(id, line.to_owned());
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

    /// Drop a signal the operator has just resolved, keeping the highlight inside
    /// the list that remains.
    fn resolved_operator_signal(&mut self, index: usize) {
        if index < self.operator_signals.len() {
            self.operator_signals.remove(index);
        }
        self.operator_signal_scroll = 0;
        self.selected_signal_action = 0;
        self.selected_operator_signal = self
            .selected_operator_signal
            .min(self.operator_signals.len().saturating_sub(1));
    }

    /// The actions the selected signal offers.
    fn selected_signal_actions(&self) -> &'static [operator_signals::SignalAction] {
        self.operator_signals
            .get(self.selected_operator_signal)
            .map_or(&[], |signal| operator_signals::actions_for(signal.kind))
    }

    /// Move the focus between the selected signal's actions.
    fn focus_next_signal_action(&mut self) {
        let count = self.selected_signal_actions().len();
        if count > 0 {
            self.selected_signal_action = (self.selected_signal_action + 1) % count;
        }
    }

    fn focus_previous_signal_action(&mut self) {
        let count = self.selected_signal_actions().len();
        if count > 0 {
            self.selected_signal_action = (self.selected_signal_action + count - 1) % count;
        }
    }

    fn select_next_operator_signal(&mut self) {
        self.operator_signal_scroll = 0;
        // Focus follows the selection rather than the list position, so moving to
        // another signal always starts from its first action.
        self.selected_signal_action = 0;
        if !self.operator_signals.is_empty() {
            self.selected_operator_signal =
                (self.selected_operator_signal + 1) % self.operator_signals.len();
        }
    }

    fn select_previous_operator_signal(&mut self) {
        self.operator_signal_scroll = 0;
        self.selected_signal_action = 0;
        if !self.operator_signals.is_empty() {
            self.selected_operator_signal =
                (self.selected_operator_signal + self.operator_signals.len() - 1)
                    % self.operator_signals.len();
        }
    }
}

pub async fn run<E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    event_loop: TaskEventLoop<E>,
    task_service: TaskService<E>,
    app: App,
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
        app,
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
    mut app: App,
    mut task_execution_updates: mpsc::UnboundedReceiver<TaskExecutionUpdate>,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    execute!(io::stdout(), EnableBracketedPaste)?;
    let mut events = EventStream::new();
    let mut refresh = time::interval(Duration::from_secs(2));
    let mut shell_notice = time::interval(Duration::from_millis(100));
    let mut shimmer = time::interval(Duration::from_millis(80));
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
            overview::draw(frame, &mut app);
            if let Some(shell) = app.open_shell.as_ref().and_then(|id| app.shells.get(id)) {
                shell.draw(frame);
            }
            if app.open_operator_signals {
                operator_signals::draw(
                    frame,
                    &app.operator_signals,
                    app.selected_operator_signal,
                    app.selected_signal_action,
                    &mut app.operator_signal_scroll,
                );
            }
            if app.open_environment_picker {
                environment_picker::draw(
                    frame,
                    environment_picker::Purpose::NewTask,
                    app.environments(),
                    app.default_environment(),
                    app.selected_environment,
                );
            }
            if app.open_environment_settings {
                environment_picker::draw(
                    frame,
                    environment_picker::Purpose::Manage,
                    app.environments(),
                    app.default_environment(),
                    app.selected_environment,
                );
            }
            // Input belongs above the settings overlay that opened it.
            if let Some(prompt) = &app.prompt {
                prompt.draw(frame);
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
                app.capacity = Some(event_loop.capacity_limit());
            }
            _ = shell_notice.tick(), if app.shell_has_active_notice() => {}
            _ = shimmer.tick(), if app.is_preparing() => {
                app.shimmer = app.shimmer.wrapping_add(1);
                app.environment_builds = task_service.environment_builds();
            }
            Some(event) = task_execution_updates.recv() => match event {
                TaskExecutionUpdate::Created(Ok(task)) => app.replace_task(task),
                TaskExecutionUpdate::Created(Err(error)) => app.notice = Some(error.to_string()),
                TaskExecutionUpdate::Started(Ok(task)) => {
                    app.output_previews.remove(&task.id);
                    app.latest_output.remove(&task.id);
                    app.replace_task(task);
                }
                TaskExecutionUpdate::Started(Err(error)) => app.notice = Some(error.to_string()),
                TaskExecutionUpdate::Finished(id, Ok(task)) if !app.is_removing(&id) => {
                    app.remove_shell(&id);
                    app.replace_task(task);
                }
                TaskExecutionUpdate::Finished(id, Err(error)) if !app.is_removing(&id) => show_task_error(&mut app, &id, error),
                TaskExecutionUpdate::Removed(id, Ok(())) => app.remove_task(&id),
                TaskExecutionUpdate::Removed(id, Err(error)) => {
                    app.cancel_removing(&id);
                    show_task_error(&mut app, &id, error);
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
                TaskExecutionUpdate::Report { task, summary } => {
                    let id = task.id.clone();
                    app.replace_task(task);
                    app.latest_output.insert(id, summary);
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

    /// Whether any task can currently be waiting on an environment build.
    fn is_preparing(&self) -> bool {
        self.tasks
            .iter()
            .any(|task| task.facts.execution_status == TaskExecutionStatus::Pending)
    }
}

fn copy_to_clipboard(text: String) -> Result<(), arboard::Error> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text)
}

fn show_task_error(app: &mut App, id: &str, error: TaskServiceError) {
    let title = app
        .tasks
        .iter()
        .find(|task| task.id == id)
        .map_or(id, |task| task.facts.intent.as_str());
    app.notice = Some(format!("{title}: {error}"));
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
                app.pending_environment_harness = None;
                app.answering_operator_signal = None;
            }
            Some(PromptAction::Submit(command)) => {
                app.prompt = None;
                if let Some(signal_id) = app.answering_operator_signal.take() {
                    event_loop.answer_operator_signal(signal_id, command);
                } else if let Some(step) = app.settings_prompt.take() {
                    app.continue_settings_prompt(step, command);
                } else if let Some(environment) = app.single_environment() {
                    event_loop.create(command, environment);
                } else if app.environments().is_empty() {
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
            if !event_loop.return_to_autopilot(&id) {
                app.notice =
                    Some("this task is no longer running; control stayed with autopilot".into());
            }
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
                if let Some(environment) = app.chosen_environment()
                    && let Some(file) = app.settings_file.as_mut()
                {
                    let mut proposed = file.settings().clone();
                    proposed.default_environment = environment.name;
                    match file.save(proposed) {
                        Ok(()) => app.notice = None,
                        Err(error) => app.notice = Some(error.to_string()),
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
                    event_loop.create(command, environment);
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    if app.open_operator_signals {
        match key.code {
            KeyCode::PageDown => {
                app.operator_signal_scroll = app.operator_signal_scroll.saturating_add(5)
            }
            KeyCode::PageUp => {
                app.operator_signal_scroll = app.operator_signal_scroll.saturating_sub(5)
            }
            KeyCode::Home => app.operator_signal_scroll = 0,
            KeyCode::End => app.operator_signal_scroll = u16::MAX,
            KeyCode::Esc | KeyCode::Char('i') => app.open_operator_signals = false,
            KeyCode::Tab => app.focus_next_signal_action(),
            KeyCode::BackTab => app.focus_previous_signal_action(),
            KeyCode::Down | KeyCode::Char('j') => app.select_next_operator_signal(),
            KeyCode::Up | KeyCode::Char('h') => app.select_previous_operator_signal(),
            KeyCode::Enter => {
                let actions = app.selected_signal_actions();
                let Some(action) = actions
                    .get(
                        app.selected_signal_action
                            .min(actions.len().saturating_sub(1)),
                    )
                    .copied()
                else {
                    return Ok(false);
                };
                let Some(signal) = app
                    .operator_signals
                    .get(app.selected_operator_signal)
                    .cloned()
                else {
                    return Ok(false);
                };
                match action {
                    operator_signals::SignalAction::Reply => {
                        if app.tasks.iter().any(|task| task.id == signal.task_id) {
                            app.answering_operator_signal = Some(signal.id);
                            app.prompt = Some(Prompt::new("answer operator signal"));
                            app.open_operator_signals = false;
                        } else {
                            app.notice = Some("this task is no longer in the list".into());
                        }
                    }
                    operator_signals::SignalAction::Acknowledge => {
                        match task_service.acknowledge_operator_signal(&signal.id).await {
                            Ok(task) => {
                                app.replace_task(task);
                                app.resolved_operator_signal(app.selected_operator_signal);
                            }
                            Err(error) => app.notice = Some(error.to_string()),
                        }
                    }
                    operator_signals::SignalAction::Confirm => {
                        match task_service.confirm_operator_signal(&signal.id).await {
                            Ok(task) => {
                                app.replace_task(task);
                                app.resolved_operator_signal(app.selected_operator_signal);
                                app.notice =
                                    Some("task closed; its capsule was disposed of".into());
                            }
                            Err(error) => app.notice = Some(error.to_string()),
                        }
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
        KeyCode::Char('c') => {
            if app.reload_settings() {
                app.prompt = Some(Prompt::new("what should the agent do?"));
            }
        }
        KeyCode::Char('s') => {
            if !app.reload_settings() {
                return Ok(false);
            }
            app.selected_environment = app
                .environments()
                .iter()
                .position(|environment| {
                    environment
                        .name
                        .eq_ignore_ascii_case(app.default_environment())
                })
                .unwrap_or(0);
            app.open_environment_settings = true;
        }
        KeyCode::Char('i') => {
            if let Some(task) = app.selected_task() {
                app.operator_signals = task_service
                    .operator_signals(&task.id)
                    .await
                    .map_err(io::Error::other)?;
                let task = task_service
                    .read_operator_signals(&task.id)
                    .await
                    .map_err(io::Error::other)?;
                app.replace_task(task);
                for signal in &mut app.operator_signals {
                    signal.read = true;
                }
                app.selected_operator_signal = 0;
                app.selected_signal_action = 0;
                app.operator_signal_scroll = 0;
                app.open_operator_signals = true;
            }
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
                    task.facts.execution_status,
                    TaskExecutionStatus::Paused | TaskExecutionStatus::Failed
                ) {
                    event_loop.start_operator_control(task);
                    app.notice = Some("restoring the agent in the terminal…".into());
                    return Ok(false);
                }
                if !matches!(task.facts.execution_status, TaskExecutionStatus::Running) {
                    return Ok(false);
                }
                if matches!(
                    task.facts.execution_mode,
                    TaskExecutionMode::Autopilot | TaskExecutionMode::AutopilotReport
                ) {
                    app.notice = Some(if event_loop.take_control(&task.id) {
                        "bringing the agent to the terminal…".into()
                    } else {
                        "this task has no run to take over".into()
                    });
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
                if !event_loop.has_active_run(&task.id) {
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
