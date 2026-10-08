use std::{
    collections::{HashMap, HashSet},
    io, mem,
    time::Duration,
};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::{signal::unix::Signal, sync::mpsc, time};

use crate::{
    agent::AgentKind,
    config::{Config, SettingsError, SettingsFile},
    isolation::Environment,
    isolation::{EnvironmentDescription, EnvironmentProgress, IsolationEngine, ProfileSummary},
    remote::{RemoteKey, RemoteKeys},
    task::{
        Task, TaskExecutionMode, TaskExecutionStatus, TaskOperatorSignal, TaskService,
        TaskServiceError,
    },
    task::{TaskEventLoop, TaskExecutionUpdate},
};

mod boot;
mod popups;
mod screens;
mod widgets;

use popups::confirm::{PendingConfirm, handle_confirm_key};
use popups::new_task::NewTask;
use popups::reply::Reply;
use popups::reply::{Prompt, PromptAction};
use screens::settings::{Depth, Focus, Form, Panel, RowKind, Section, Setting, Target};
use screens::shell::Shell;
use widgets::chrome::Hint;
use widgets::keys::{is_dismiss, is_help, is_remove};

pub use boot::{
    database as boot_database, isolation as boot_isolation, task_resume as boot_task_resume,
};

/// What the keyboard is answering to, most immediate first.
///
/// One value, because the order is the whole point. The keys, the guidance line
/// and the drawing each worked it out separately, and could disagree about it:
/// the shell offered a `?` it never sees, because every key in a shell goes to
/// the terminal.
///
/// A question owns its state, because it exists only while it is in front. A
/// view does not: the tasks, the signals, the shells and the settings are the
/// work, and they live with the application.
#[derive(Default)]
enum Foreground {
    /// The keys for wherever you are, over whatever is there. Help is stacked
    /// rather than exclusive: dismissing it returns to what it covered.
    Keys(Box<Foreground>),
    /// A question waiting on an answer, and what the answer would do.
    Confirm(PendingConfirm),
    /// A reply being written to a signal.
    Reply(Reply),
    /// The creation question, holding the prompt until it is answered.
    NewTask(NewTask),
    /// The settings panel: environments, remotes, the interface, the identity.
    Panel,
    /// A task's signals, over the list it was opened from.
    Signals,
    /// A task's terminal, taking every key there is.
    Shell(String),
    #[default]
    Tasks,
}

pub struct App {
    tasks: Vec<Task>,
    /// How many cards fit across, from the last drawing. Written by the
    /// drawing, read by the keys, because a step within the grid is one card or
    /// one whole row of them.
    grid_columns: usize,
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
    shells: HashMap<String, Shell>,
    /// The selected task's signals, and where you are among them. Read when the
    /// signals open; the pop-up is a view over what the task already recorded.
    operator_signals: Vec<TaskOperatorSignal>,
    selected_operator_signal: usize,
    /// Which of the selected signal's actions has focus.
    selected_signal_action: usize,
    operator_signal_scroll: u16,
    /// Where `config.toml` lives, so the manage view can persist changes.
    config: Config,
    settings_file: Option<SettingsFile>,
    /// The settings panel: environments, remotes, the interface, and the identity.
    panel: Panel,
    /// What the keyboard is answering to.
    foreground: Foreground,

    /// What each address last answered with. Written by the dial once there is
    /// one; read when the panel opens.
    remote_keys: RemoteKeys,
    /// What those addresses answered with, as of the panel being opened.
    remembered_keys: Vec<RemoteKey>,
    /// What the built base environment is made of, as of the panel being opened.
    base_description: Option<EnvironmentDescription>,
    /// The profiles a task mounts, read when the panel opens.
    profiles: Vec<ProfileSummary>,
}

impl App {
    pub fn new(
        tasks: Vec<Task>,
        config: Config,
        settings: Result<SettingsFile, SettingsError>,
        remote_keys: RemoteKeys,
    ) -> Self {
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
            grid_columns: 1,
            notice: settings_error,
            removing: HashSet::new(),
            selected: 0,
            first_visible_task: 0,
            shells: HashMap::new(),
            operator_signals: Vec::new(),
            selected_operator_signal: 0,
            selected_signal_action: 0,
            operator_signal_scroll: 0,
            config,
            settings_file,
            panel: Panel::new(),
            foreground: Foreground::default(),

            remote_keys,
            remembered_keys: Vec::new(),
            base_description: None,
            profiles: Vec::new(),
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

    /// Move within the grid: one card across, or one row of cards.
    ///
    /// The grid does not wrap, because a card's neighbours are where it is and
    /// wrapping from the last column to the first is a jump, not a step.
    fn select_adjacent(&mut self, step: isize, columns: usize) {
        let count = self.tasks.len();
        if count == 0 {
            return;
        }
        let moved = self.selected as isize + step;
        self.selected = moved.clamp(0, count as isize - 1) as usize;
        let _ = columns;
    }

    /// How many cards fit across the body, which is what a grid step needs.
    fn grid_columns(&self) -> usize {
        self.grid_columns.max(1)
    }

    /// Everything a task can run in, base first.
    ///
    /// Built from the settings rather than stored on the app: the list is two or
    /// three entries, and a copy kept here would be one more thing to keep in
    /// step with the file.
    fn environments(&self) -> Vec<Environment> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().environments())
            .unwrap_or_else(|| vec![Environment::base()])
    }

    fn default_environment(&self) -> &str {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().default_environment.as_str())
            .unwrap_or_default()
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

    /// The coding agent Altitude drives, once one has been chosen.
    fn agent(&self) -> Option<AgentKind> {
        self.settings_file
            .as_ref()
            .and_then(|file| file.settings().agent())
    }

    /// Choose the coding agent, and keep it.
    fn set_agent(&mut self, agent: AgentKind) {
        let Some(file) = self.settings_file.as_mut() else {
            self.notice = Some("settings are unavailable; fix the file and reopen".to_owned());
            return;
        };
        let mut proposed = file.settings().clone();
        proposed.set_agent(agent);
        match file.save(proposed) {
            Ok(()) => self.notice = None,
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    /// Whether the keys that work are listed at the bottom of the screen.
    fn keyboard_hints(&self) -> bool {
        self.settings_file
            .as_ref()
            .is_none_or(|file| file.settings().interface().keyboard_hints)
    }

    /// The keys that would do something where the focus is.
    ///
    /// The frame shows these, and the help popup lists them, so a key that
    /// works is never a key you have to guess at. They follow what is in front
    /// rather than the fields behind it, so the line cannot offer a key that
    /// something above it would take first.
    fn hints(&self) -> Vec<Hint> {
        self.hints_for(&self.foreground)
    }

    /// The keys of a given state, which help needs for the state it covers.
    fn hints_for(&self, front: &Foreground) -> Vec<Hint> {
        let dismiss = || Hint::new("esc", "close");
        match front {
            // Help lists the keys of what it covers, which is what the line
            // under it goes on showing.
            Foreground::Keys(behind) => self.hints_for(behind),
            Foreground::Confirm(pending) => pending.hints(),
            Foreground::Reply(_) => vec![
                Hint::new("enter", "send"),
                dismiss(),
                Hint::new("?", "keys"),
            ],
            Foreground::NewTask(new_task) => new_task.hints(),
            Foreground::Panel => {
                if self.panel.form.is_some() {
                    return vec![
                        Hint::new("tab", "move"),
                        Hint::new("enter", "validate, or add a line"),
                        Hint::new("esc", "leave the box, then the page"),
                    ];
                }
                let mut hints = vec![Hint::new("tab", "change column"), Hint::new("j/k", "move")];
                if self.panel.section.grows() {
                    hints.push(Hint::new("enter", "open"));
                    hints.push(Hint::new("n", "new"));
                    hints.push(Hint::new("d", "delete"));
                } else if self.panel.section == Section::Interface {
                    hints.push(Hint::new("enter", "toggle"));
                }
                hints.push(dismiss());
                hints
            }
            Foreground::Signals => vec![
                Hint::new("tab", "action"),
                Hint::new("enter", "validate"),
                Hint::new("j/k", "signal"),
                Hint::new("pgup/pgdn", "scroll"),
                dismiss(),
            ],
            // A shell takes every key there is, so it has no `?` to offer.
            Foreground::Shell(_) => vec![Hint::new("ctrl-c", "hand back to autopilot")],
            Foreground::Tasks => {
                // Only what there is something to do it to.
                let mut hints = vec![Hint::new("c", "create"), Hint::new("s", "settings")];
                if !self.tasks.is_empty() {
                    hints.extend([
                        Hint::new("i", "info"),
                        Hint::new("h/j/k/l", "select"),
                        Hint::new("enter", "shell"),
                        Hint::new("d", "remove"),
                    ]);
                }
                hints.push(Hint::new("?", "keys"));
                hints.push(Hint::new("q", "quit"));
                hints
            }
        }
    }

    /// Whether a text box has the keys, in which case `?` is a character.
    fn is_typing(&self) -> bool {
        match &self.foreground {
            Foreground::Reply(_) => true,
            Foreground::NewTask(new_task) => new_task.is_typing(),
            _ => self
                .panel
                .form
                .as_ref()
                .is_some_and(|form| form.focus.takes_text()),
        }
    }

    /// The task whose terminal is in front, if one is.
    fn open_shell(&self) -> Option<&str> {
        match &self.foreground {
            Foreground::Shell(id) => Some(id),
            _ => None,
        }
    }

    fn panel_key(&mut self, key: KeyEvent) {
        let rows = self.row_count();
        match key.code {
            _ if is_dismiss(key) || key.code == KeyCode::Char('s') => {
                self.foreground = Foreground::Tasks;
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                // A row with nothing inside it has no deeper level to open.
                if self.panel.depth != Depth::List
                    || self.selected_row().is_some_and(|row| row.is_actionable())
                {
                    self.panel.depth = self.panel.depth.deeper();
                }
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.panel.depth = self.panel.depth.shallower()
            }
            KeyCode::Down | KeyCode::Char('j') => self.panel.move_selection(true, rows),
            KeyCode::Up | KeyCode::Char('k') => self.panel.move_selection(false, rows),
            KeyCode::Char('n') if self.panel.section.grows() => {
                self.panel.form = Some(match self.panel.section {
                    Section::Remotes => Form::new(Target::NewRemote, "", ""),
                    _ => Form::new(Target::NewEnvironment, "", ""),
                });
            }
            _ if is_remove(key) && self.panel.section.grows() => self.ask_to_delete(),
            KeyCode::Enter => self.edit_selected(),
            _ => {}
        }
    }

    fn edit_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        self.panel.form = Some(match row.kind {
            // The base has no form: it is described instead.
            RowKind::Base => {
                self.notice = Some("the base environment is built in".to_owned());
                return;
            }
            RowKind::Switch { setting, on } => {
                self.toggle(setting, !on);
                return;
            }
            RowKind::Agent { kind, .. } => {
                self.set_agent(kind);
                return;
            }
            RowKind::Environment { name, setup, .. } => Form::new(
                Target::Environment {
                    original: name.clone(),
                },
                &name,
                &setup,
            ),
            RowKind::Remote { address, .. } => Form::new(
                Target::Remote {
                    original: address.clone(),
                },
                &address,
                "",
            ),
        });
    }

    fn ask_to_delete(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        // The question is built first: it borrows the name the target then moves.
        let (question, target) = match row.kind {
            RowKind::Base | RowKind::Switch { .. } | RowKind::Agent { .. } => return,
            RowKind::Environment { name, .. } => (
                format!("Delete the {name} environment?"),
                Target::Environment { original: name },
            ),
            RowKind::Remote { address, .. } => (
                format!("Delete {address}, and the key remembered for it?"),
                Target::Remote { original: address },
            ),
        };
        self.foreground = Foreground::Confirm(PendingConfirm::deleting(target, question));
    }

    /// Turn a preference on or off, and keep it.
    fn toggle(&mut self, setting: Setting, on: bool) {
        let Some(file) = self.settings_file.as_mut() else {
            self.notice = Some("settings are unavailable; fix the file and reopen".to_owned());
            return;
        };
        let mut proposed = file.settings().clone();
        match setting {
            Setting::KeyboardHints => proposed.set_keyboard_hints(on),
        }
        match file.save(proposed) {
            Ok(()) => self.notice = None,
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn panel_form_key(&mut self, key: KeyEvent) {
        let Some(mut form) = self.panel.form.take() else {
            return;
        };
        let mut keep = true;
        let mut save = false;
        match key.code {
            // Escape leaves the text box first, and the page only on the second
            // press, so a setup command can hold newlines without trapping the
            // operator in it.
            KeyCode::Esc if form.focus.takes_text() => form.focus = Focus::Cancel,
            _ if is_dismiss(key) && !form.focus.takes_text() => keep = false,
            KeyCode::BackTab => form.move_focus(false),
            KeyCode::Tab => form.move_focus(true),
            KeyCode::Enter if form.focus == Focus::Setup => form.setup.handle_key(key),
            KeyCode::Enter => match form.focus {
                Focus::Cancel => keep = false,
                Focus::Save => save = true,
                _ => form.move_focus(true),
            },
            _ => match form.focus {
                Focus::Primary => form.primary.handle_key(key),
                Focus::Setup => form.setup.handle_key(key),
                _ => {}
            },
        }
        if save {
            // A refused save keeps the page open, so the reason and the work
            // are both still there.
            if !self.save_form(&form) {
                self.panel.form = Some(form);
            }
        } else if keep {
            self.panel.form = Some(form);
        }
    }

    /// Save what a form holds, answering whether it was saved.
    fn save_form(&mut self, form: &Form) -> bool {
        // Refused before the file is touched, so the reason is the form's rather
        // than a validation message about a name that was never given.
        if let Some(refusal) = form.refusal() {
            self.notice = Some(refusal.to_owned());
            return false;
        }
        let Some(file) = self.settings_file.as_mut() else {
            self.notice = Some("settings are unavailable; fix the file and reopen".to_owned());
            return false;
        };
        let mut proposed = file.settings().clone();
        match &form.target {
            Target::NewEnvironment | Target::Environment { .. } => {
                let name = form.primary.value().trim().to_owned();
                if let Some(original) = form.target.original() {
                    // Renaming the default must not quietly hand the default
                    // back to the base, so it is carried across the change.
                    let was_default = proposed.default_environment.eq_ignore_ascii_case(original);
                    proposed.remove_environment(original);
                    if was_default {
                        proposed.set_default_environment(&name);
                    }
                }
                proposed.add_environment(Environment {
                    name,
                    setup_command: form.setup.value(),
                });
            }
            Target::NewRemote | Target::Remote { .. } => {
                if let Some(original) = form.target.original() {
                    proposed.remove_remote(original);
                }
                proposed.add_remote(form.primary.value());
            }
        }
        match file.save(proposed) {
            Ok(()) => {
                self.panel.form = None;
                self.notice = None;
                true
            }
            Err(error) => {
                self.notice = Some(error.to_string());
                false
            }
        }
    }

    /// Destroy what a confirmation was about.
    async fn delete(&mut self, target: Target) {
        let Some(file) = self.settings_file.as_mut() else {
            return;
        };
        let mut proposed = file.settings().clone();
        match &target {
            Target::Environment { original } => proposed.remove_environment(original),
            Target::Remote { original } => {
                proposed.remove_remote(original);
                // A forgotten remote's key is forgotten with it: the next
                // sighting is a first sighting again, which is what forgetting
                // means.
                if let Err(error) = self.remote_keys.forget(original).await {
                    self.notice = Some(format!("could not forget the key for {original}: {error}"));
                }
            }
            _ => {}
        }
        match file.save(proposed) {
            Ok(()) => {
                self.panel.selected = self.panel.selected.saturating_sub(1);
                self.notice = None;
            }
            Err(error) => self.notice = Some(error.to_string()),
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

    fn hide_shell(&mut self) {
        tracing::info!(task_id = ?self.open_shell(), "detaching terminal");
        self.foreground = Foreground::Tasks;
    }

    fn remove_shell(&mut self, id: &str) {
        self.shells.remove(id);
        if self.open_shell() == Some(id) {
            self.foreground = Foreground::Tasks;
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
    fn selected_signal_actions(&self) -> &'static [popups::signals::SignalAction] {
        self.operator_signals
            .get(self.selected_operator_signal)
            .map_or(&[], |signal| popups::signals::actions_for(signal.kind))
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
        if mouse_captured != app.open_shell().is_some() {
            mouse_captured = app.open_shell().is_some();
            if mouse_captured {
                execute!(io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(io::stdout(), DisableMouseCapture)?;
            }
        }
        terminal.draw(|frame| {
            let hints = app.hints();
            let content =
                widgets::chrome::draw(frame, &hints, app.keyboard_hints(), app.notice.as_deref());
            screens::tasks::draw(frame, content, &mut app);
            if let Some(shell) = app.open_shell().and_then(|id| app.shells.get(id)) {
                shell.draw(frame, content);
            }
            if matches!(app.foreground, Foreground::Panel) {
                // Fullscreen: it is the screen, not something over it.
                let view = app.settings_view();
                screens::settings::draw(frame, content, &app.panel, &view);
            } else if matches!(app.foreground, Foreground::Signals) {
                widgets::chrome::dim(frame, content);
                popups::signals::draw(
                    frame,
                    content,
                    &app.operator_signals,
                    app.selected_task()
                        .and_then(|task| task.facts.latest_turn_summary)
                        .as_deref(),
                    app.selected_operator_signal,
                    app.selected_signal_action,
                    &mut app.operator_signal_scroll,
                );
            }
            if let Foreground::Confirm(confirm) = &app.foreground {
                widgets::chrome::dim(frame, content);
                confirm.draw(frame, content);
            }
            if let Foreground::NewTask(new_task) = &app.foreground {
                widgets::chrome::dim(frame, content);
                popups::new_task::draw(frame, content, new_task);
            }
            if matches!(app.foreground, Foreground::Keys(_)) {
                widgets::chrome::dim(frame, content);
                widgets::chrome::draw_help(frame, content, &hints);
            }
            // Input belongs above the settings overlay that opened it.
            if let Foreground::Reply(reply) = &app.foreground {
                widgets::chrome::dim(frame, content);
                reply.prompt.draw(frame, content);
            }
        })?;
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = refresh.tick(), if app.open_shell().is_none() => {
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
            Some(update) = task_execution_updates.recv() =>
                app.apply(update, terminal, &event_loop).await?,
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                if handle_event(event?, terminal, &event_loop, &task_service, &mut app).await? { return Ok(()); }
            }
        }
    }
}

impl App {
    /// Close the keys, returning to what they covered.
    fn dismiss_keys(&mut self) {
        if let Foreground::Keys(behind) = mem::take(&mut self.foreground) {
            self.foreground = *behind;
        }
    }

    /// Take the answer to a reply, and act on it.
    fn reply_key<E>(&mut self, key: KeyEvent, event_loop: &TaskEventLoop<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::Reply(Reply {
            mut prompt,
            answering,
        }) = mem::take(&mut self.foreground)
        else {
            return;
        };
        match prompt.handle_key(key) {
            None => self.foreground = Foreground::Reply(Reply { prompt, answering }),
            // Leaving the box without answering returns to the signals it was
            // opened from, rather than all the way out to the list.
            Some(PromptAction::Cancel) => self.foreground = Foreground::Signals,
            Some(PromptAction::Submit(command)) => {
                // Answering the last signal leaves nothing to go back to, so that
                // is the one case that returns to the task list.
                let was_last = self
                    .operator_signals
                    .last()
                    .is_some_and(|signal| signal.id == answering);
                event_loop.answer_operator_signal(answering, command);
                if !was_last {
                    self.foreground = Foreground::Signals;
                }
            }
        }
    }

    /// Send a key to the terminal in front, or hand control back.
    fn shell_key<E>(&mut self, key: KeyEvent, event_loop: &TaskEventLoop<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(id) = self.open_shell().map(str::to_owned) else {
            return;
        };
        if screens::shell::is_exit_key(key) {
            if !event_loop.return_to_autopilot(&id) {
                self.notice =
                    Some("this task is no longer running; control stayed with autopilot".into());
            }
            self.hide_shell();
        } else if let Some(shell) = self.shells.get_mut(&id)
            && let Some(bytes) = shell.key_bytes(key)
        {
            event_loop.send_terminal_input(&id, bytes);
        }
    }

    /// Take the answer to the creation question, and act on it.
    fn new_task_key<E>(&mut self, key: KeyEvent, event_loop: &TaskEventLoop<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::NewTask(mut question) = mem::take(&mut self.foreground) else {
            return;
        };
        match question.handle_key(key) {
            None => self.foreground = Foreground::NewTask(question),
            Some(popups::new_task::Answer::Cancel) => {}
            Some(popups::new_task::Answer::Create {
                task,
                environment,
                agent,
                remember,
            }) => {
                // The first answer becomes the default, because there was nothing
                // to default to and guessing is not Altitude's to do.
                if let Some(agent) = remember {
                    self.set_agent(agent);
                }
                event_loop.create(task, environment, agent);
            }
        }
    }

    /// The panel's keys, which are its form's when a form is open.
    fn panel_keys(&mut self, key: KeyEvent) {
        if self.panel.form.is_some() {
            self.panel_form_key(key);
        } else {
            self.panel_key(key);
        }
    }

    /// The keys of the signals pop-up.
    async fn signals_key<E>(&mut self, key: KeyEvent, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        match key.code {
            KeyCode::PageDown => {
                self.operator_signal_scroll = self.operator_signal_scroll.saturating_add(5)
            }
            KeyCode::PageUp => {
                self.operator_signal_scroll = self.operator_signal_scroll.saturating_sub(5)
            }
            KeyCode::Home => self.operator_signal_scroll = 0,
            KeyCode::End => self.operator_signal_scroll = u16::MAX,
            _ if is_dismiss(key) || key.code == KeyCode::Char('i') => {
                self.foreground = Foreground::Tasks
            }
            KeyCode::Tab => self.focus_next_signal_action(),
            KeyCode::BackTab => self.focus_previous_signal_action(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_operator_signal(),
            KeyCode::Up | KeyCode::Char('h') => self.select_previous_operator_signal(),
            KeyCode::Enter => {
                let actions = self.selected_signal_actions();
                let Some(action) = actions
                    .get(
                        self.selected_signal_action
                            .min(actions.len().saturating_sub(1)),
                    )
                    .copied()
                else {
                    return;
                };
                let Some(signal) = self
                    .operator_signals
                    .get(self.selected_operator_signal)
                    .cloned()
                else {
                    return;
                };
                match action {
                    popups::signals::SignalAction::Reply => {
                        if self.tasks.iter().any(|task| task.id == signal.task_id) {
                            self.foreground = Foreground::Reply(Reply {
                                prompt: Prompt::new("answer operator signal"),
                                answering: signal.id,
                            });
                        } else {
                            self.notice = Some("this task is no longer in the list".into());
                        }
                    }
                    popups::signals::SignalAction::Acknowledge => {
                        match task_service.acknowledge_operator_signal(&signal.id).await {
                            Ok(task) => {
                                self.replace_task(task);
                                self.resolved_operator_signal(self.selected_operator_signal);
                            }
                            Err(error) => self.notice = Some(error.to_string()),
                        }
                    }
                    popups::signals::SignalAction::Confirm => {
                        match task_service.confirm_operator_signal(&signal.id).await {
                            Ok(task) => {
                                self.replace_task(task);
                                self.resolved_operator_signal(self.selected_operator_signal);
                                self.notice =
                                    Some("task closed; its capsule was disposed of".into());
                            }
                            Err(error) => self.notice = Some(error.to_string()),
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// The keys of the task list.
    async fn list_key<E>(
        &mut self,
        key: KeyEvent,
        terminal: &DefaultTerminal,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
    ) -> io::Result<bool>
    where
        E: IsolationEngine + Clone + 'static,
    {
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Char('c') => {
                if self.reload_settings() {
                    self.foreground = Foreground::NewTask(NewTask::new(
                        self.environments(),
                        self.default_environment(),
                        self.agent(),
                    ));
                }
            }
            KeyCode::Char('s') => {
                open_panel(self, task_service).await;
            }
            KeyCode::Char('i') => {
                if let Some(task) = self.selected_task() {
                    self.operator_signals = task_service
                        .operator_signals(&task.id)
                        .await
                        .map_err(io::Error::other)?;
                    let task = task_service
                        .read_operator_signals(&task.id)
                        .await
                        .map_err(io::Error::other)?;
                    self.replace_task(task);
                    for signal in &mut self.operator_signals {
                        signal.read = true;
                    }
                    self.selected_operator_signal = 0;
                    self.selected_signal_action = 0;
                    self.operator_signal_scroll = 0;
                    self.foreground = Foreground::Signals;
                }
            }
            // A grid step: one card across, or one whole row of them.
            KeyCode::Char('j') | KeyCode::Down => {
                let columns = self.grid_columns();
                self.select_adjacent(columns as isize, columns);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let columns = self.grid_columns();
                self.select_adjacent(-(columns as isize), columns);
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                let columns = self.grid_columns();
                self.select_adjacent(1, columns);
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => {
                let columns = self.grid_columns();
                self.select_adjacent(-1, columns);
            }
            _ if is_remove(key) => {
                if let Some(task) = self.selected_task() {
                    let question = format!(
                        "Remove \"{}\" and dispose of its capsule?",
                        task.facts.intent
                    );
                    self.foreground =
                        Foreground::Confirm(PendingConfirm::removing(task.id, question));
                }
            }
            KeyCode::Enter => {
                if let Some(task) = self.selected_task() {
                    if matches!(
                        task.facts.execution_status,
                        TaskExecutionStatus::Paused | TaskExecutionStatus::Failed
                    ) {
                        event_loop.start_operator_control(task);
                        self.notice = Some("restoring the agent in the terminal…".into());
                        return Ok(false);
                    }
                    if !matches!(task.facts.execution_status, TaskExecutionStatus::Running) {
                        return Ok(false);
                    }
                    if matches!(
                        task.facts.execution_mode,
                        TaskExecutionMode::Autopilot | TaskExecutionMode::AutopilotReport
                    ) {
                        self.notice = Some(if event_loop.take_control(&task.id) {
                            "bringing the agent to the terminal…".into()
                        } else {
                            "this task has no run to take over".into()
                        });
                        return Ok(false);
                    }
                    if self.shells.contains_key(&task.id) {
                        let (rows, cols) = screens::shell::size(terminal.size()?.into());
                        event_loop.resize_terminal(&task.id, rows, cols);
                        if let Some(shell) = self.shells.get_mut(&task.id) {
                            shell.resize(rows, cols);
                        }
                        tracing::info!(task_id = %task.id, "reattaching terminal");
                        self.foreground = Foreground::Shell(task.id);
                        return Ok(false);
                    }
                    if !event_loop.has_active_run(&task.id) {
                        return Ok(false);
                    }
                    let (rows, cols) = screens::shell::size(terminal.size()?.into());
                    let history = event_loop.cached_terminal_output(&task.id);
                    event_loop.resize_terminal(&task.id, rows, cols);
                    self.shells
                        .insert(task.id.clone(), Shell::new(rows, cols, history));
                    self.foreground = Foreground::Shell(task.id);
                    tracing::info!("attached terminal");
                }
            }
            _ => {}
        }
        Ok(false)
    }

    /// Take one change from a task's run, and make the interface match it.
    async fn apply<E>(
        &mut self,
        update: TaskExecutionUpdate,
        terminal: &mut DefaultTerminal,
        event_loop: &TaskEventLoop<E>,
    ) -> io::Result<()>
    where
        E: IsolationEngine + Clone + 'static,
    {
        match update {
            TaskExecutionUpdate::Created(Ok(task)) => self.replace_task(task),
            TaskExecutionUpdate::Created(Err(error)) => self.notice = Some(error.to_string()),
            TaskExecutionUpdate::Started(Ok(task)) => self.replace_task(task),
            TaskExecutionUpdate::Started(Err(error)) => self.notice = Some(error.to_string()),
            TaskExecutionUpdate::Finished(id, Ok(task)) if !self.is_removing(&id) => {
                self.remove_shell(&id);
                self.replace_task(task);
            }
            // A run's failure is the task's own news rather than the
            // interface's: the card turns red and says so, and the reason
            // belongs to the task rather than to the band under the list.
            TaskExecutionUpdate::Finished(id, Err(error)) if !self.is_removing(&id) => {
                tracing::warn!(task_id = %id, %error, "a task's run ended with an error");
            }
            TaskExecutionUpdate::Removed(id, Ok(())) => self.remove_task(&id),
            TaskExecutionUpdate::Removed(id, Err(error)) => {
                self.cancel_removing(&id);
                show_task_error(self, &id, error);
            }
            TaskExecutionUpdate::Output {
                task_id,
                stream,
                bytes,
            } => {
                let _ = stream;
                if let Some(shell) = self.shells.get_mut(&task_id)
                    && let Some(text) = shell.process(bytes)
                {
                    match copy_to_clipboard(text) {
                        Ok(()) => {
                            shell.show_clipboard_confirmation();
                            tracing::info!(task_id = %task_id, "copied terminal text to clipboard");
                        }
                        Err(error) => {
                            tracing::error!(task_id = %task_id, %error, "could not copy terminal text")
                        }
                    }
                }
            }
            TaskExecutionUpdate::OperatorReady(task) => {
                let id = task.id.clone();
                self.replace_task(task);
                let (rows, cols) = screens::shell::size(terminal.size()?.into());
                let history = event_loop.cached_terminal_output(&id);
                event_loop.resize_terminal(&id, rows, cols);
                self.shells
                    .insert(id.clone(), Shell::new(rows, cols, history));
                self.foreground = Foreground::Shell(id);
            }
            TaskExecutionUpdate::Report { task, summary } => {
                // The agent's own words are already recorded in the task's
                // history; the card does not repeat them.
                let _ = summary;
                self.replace_task(task);
            }
            _ => {}
        }
        Ok(())
    }

    fn shell_has_active_notice(&self) -> bool {
        self.open_shell()
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

/// Open the settings panel, reading back the two things it cannot see for
/// itself: what each address last answered with, and what the built base
/// environment is made of.
async fn open_panel<E>(app: &mut App, task_service: &TaskService<E>)
where
    E: IsolationEngine + Clone + 'static,
{
    app.foreground = Foreground::Panel;
    app.panel.depth = Depth::List;
    app.panel.form = None;
    if !app.reload_settings() {
        return;
    }
    app.base_description = task_service.base_description();
    app.profiles = task_service.profiles();
    match app.remote_keys.list().await {
        Ok(keys) => app.remembered_keys = keys,
        Err(error) => app.notice = Some(format!("could not read remembered remotes: {error}")),
    }
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
        && let Some(id) = app.open_shell().map(str::to_owned)
    {
        let (rows, cols) = screens::shell::size(terminal.size()?.into());
        event_loop.resize_terminal(&id, rows, cols);
        if let Some(shell) = app.shells.get_mut(&id) {
            shell.resize(rows, cols);
        }
        return Ok(false);
    }
    if let Event::Paste(text) = event {
        if let Foreground::Reply(reply) = &mut app.foreground {
            reply.prompt.paste(&text);
        } else if let Foreground::NewTask(new_task) = &mut app.foreground {
            new_task.paste(&text);
        } else if let Some(form) = &mut app.panel.form {
            match form.focus {
                Focus::Primary => form.primary.paste(&text),
                Focus::Setup => form.setup.paste(&text),
                _ => {}
            }
        } else if let Some(id) = app.open_shell().map(str::to_owned)
            && let Some(shell) = app.shells.get(&id)
            && let Some(bytes) = shell.paste_bytes(text)
        {
            event_loop.send_terminal_input(&id, bytes);
        }
        return Ok(false);
    }
    if let Event::Mouse(mouse) = event {
        if let Some(id) = app.open_shell().map(str::to_owned)
            && let Some(shell) = app.shells.get_mut(&id)
            && let Some(bytes) = shell.mouse_bytes(mouse, terminal.size()?.into())
        {
            event_loop.send_terminal_input(&id, bytes);
        }
        return Ok(false);
    }
    let Event::Key(key) = event else {
        return Ok(false);
    };
    if !key.is_press() {
        return Ok(false);
    }
    // Anything at all closes help: a help screen is not somewhere to be.
    if matches!(app.foreground, Foreground::Keys(_)) {
        app.dismiss_keys();
        return Ok(false);
    }
    if is_help(key) && !app.is_typing() {
        app.foreground = Foreground::Keys(Box::new(mem::take(&mut app.foreground)));
        return Ok(false);
    }
    // In the order the states are declared: whatever is in front takes the key,
    // and the line under it says which one that is.
    match &app.foreground {
        Foreground::Keys(_) => app.dismiss_keys(),
        Foreground::Confirm(_) => handle_confirm_key(app, event_loop, key).await,
        Foreground::Reply(_) => app.reply_key(key, event_loop),
        Foreground::NewTask(_) => app.new_task_key(key, event_loop),
        Foreground::Panel => app.panel_keys(key),
        Foreground::Signals => app.signals_key(key, task_service).await,
        Foreground::Shell(_) => app.shell_key(key, event_loop),
        Foreground::Tasks => {
            if app
                .list_key(key, terminal, event_loop, task_service)
                .await?
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
