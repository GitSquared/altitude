//! The application: what it is holding, and what it does with it.
//!
//! One value holds everything a running client knows — the tasks, the settings,
//! the shells, and what the keyboard is answering to — and the methods here are
//! what it does in response. Nothing in this file talks to a terminal except to
//! draw, and nothing here reads an event.

use std::{
    collections::{HashMap, HashSet},
    fs, io, mem,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{DefaultTerminal, style::Style};
use tokio::sync::mpsc;

use crate::{
    agent::AgentKind,
    config::{Config, DEFAULT_PORT, SettingsError, SettingsFile},
    isolation::{
        Environment, EnvironmentDescription, EnvironmentProgress, IsolationEngine, ProfileSummary,
    },
    remote::{
        Announcement, ConfigBundle, LinkClient, RemoteKey, RemoteKeyStatus, RemoteKeys,
        RemoteState, meet,
    },
    task::{
        Task, TaskEventLoop, TaskExecutionMode, TaskExecutionStatus, TaskExecutionUpdate,
        TaskOperatorSignal, TaskService, TaskServiceError,
    },
    tui::keys::{is_dismiss, is_remove},
    tui::popups,
    tui::screens,
    tui::widgets::chrome::Hint,
    tui::widgets::palette,
};

use popups::confirm::PendingConfirm;
use popups::new_task::NewTask;
use popups::reply::{Prompt, PromptAction, Reply};
use screens::settings::{Depth, Focus, Form, Panel, RowKind, Section, Setting, Target};
use screens::shell::Shell;

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
pub(in crate::tui) enum Foreground {
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

/// What the panel last heard from an address.
///
/// Asking is automatic, because a status you have to ask for is one you will not
/// see: opening the list dials every address in it, one at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tui) enum RemoteStatus {
    /// Not asked yet.
    Unknown,
    /// Asked, and nothing has come back.
    Asking,
    /// Answered with the key remembered for it.
    Reached,
    /// Being sent this client's configuration.
    Syncing,
    /// Answered with a different key, which is never accepted on its own.
    Changed,
    /// Did not answer.
    Silent(String),
}

impl RemoteStatus {
    /// The dot in front of the name. Filled when something answered.
    pub(in crate::tui) fn glyph(&self) -> char {
        match self {
            Self::Asking | Self::Syncing => '◌',
            Self::Reached | Self::Changed => '●',
            Self::Unknown | Self::Silent(_) => '○',
        }
    }

    /// Whether something is happening, in which case the dot travels like every
    /// other wait in the interface.
    pub(in crate::tui) fn is_in_progress(&self) -> bool {
        matches!(self, Self::Asking | Self::Syncing)
    }

    pub(in crate::tui) fn style(&self) -> Style {
        match self {
            Self::Unknown => palette::muted(),
            Self::Asking | Self::Syncing => palette::attention(),
            Self::Reached => palette::good(),
            Self::Changed | Self::Silent(_) => palette::bad(),
        }
    }
}

/// What came back from reaching an address. Sent from the dial, which runs on its
/// own so a slow one cannot hold up the interface.
pub(in crate::tui) enum RemoteAnswer {
    Reached {
        address: String,
        announcement: Announcement,
        key_status: RemoteKeyStatus,
    },
    Silent {
        address: String,
        reason: String,
    },
}

/// How long the list is shown before its addresses are dialled, so moving through
/// the sections does not reach anything.
const ASK_AFTER: Duration = Duration::from_millis(400);

pub struct App {
    pub(in crate::tui) tasks: Vec<Task>,
    /// How many cards fit across, from the last drawing. Written by the
    /// drawing, read by the keys, because a step within the grid is one card or
    /// one whole row of them.
    pub(in crate::tui) grid_columns: usize,
    /// What each provisioning task is waiting on, refreshed while any task is
    /// pending.
    pub(in crate::tui) environment_builds: HashMap<String, EnvironmentProgress>,
    /// Advances the highlight on that waiting text.
    pub(in crate::tui) shimmer: usize,
    /// What the host will run at once, so a wait can say whether there is room.
    /// Absent until the first refresh, and never stale by more than one interval.
    pub(in crate::tui) capacity: Option<usize>,
    pub(in crate::tui) notice: Option<String>,
    pub(in crate::tui) removing: HashSet<String>,
    pub(in crate::tui) selected: usize,
    pub(in crate::tui) first_visible_task: usize,
    pub(in crate::tui) shells: HashMap<String, Shell>,
    /// The selected task's signals, and where you are among them. Read when the
    /// signals open; the pop-up is a view over what the task already recorded.
    pub(in crate::tui) operator_signals: Vec<TaskOperatorSignal>,
    pub(in crate::tui) selected_operator_signal: usize,
    /// Which of the selected signal's actions has focus.
    pub(in crate::tui) selected_signal_action: usize,
    pub(in crate::tui) operator_signal_scroll: u16,
    /// Where `config.toml` lives, so the manage view can persist changes.
    pub(in crate::tui) config: Config,
    pub(in crate::tui) settings_file: Option<SettingsFile>,
    /// The settings panel: environments, remotes, the interface, and the identity.
    pub(in crate::tui) panel: Panel,
    /// What the keyboard is answering to.
    pub(in crate::tui) foreground: Foreground,
    /// What each address last answered, for the list to show.
    pub(in crate::tui) remote_status: HashMap<String, RemoteStatus>,
    /// When the remotes list started showing, which the wait before dialling is
    /// measured from. Absent while it is not showing.
    remotes_visible_since: Option<Instant>,
    /// When this client last sent its configuration to each address. Held in
    /// memory: a restart forgets, and sending a small file again is cheaper than a
    /// record that can disagree with what the remote holds.
    remote_synced: HashMap<String, i64>,

    /// What each address last answered with. Written by the dial once there is
    /// one; read when the panel opens.
    pub(in crate::tui) remote_keys: RemoteKeys,
    /// What those addresses answered with, as of the panel being opened.
    pub(in crate::tui) remembered_keys: Vec<RemoteKey>,
    /// What the built base environment is made of, as of the panel being opened.
    pub(in crate::tui) base_description: Option<EnvironmentDescription>,
    /// The profiles a task mounts, read when the panel opens.
    pub(in crate::tui) profiles: Vec<ProfileSummary>,
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
            remote_status: HashMap::new(),
            remotes_visible_since: None,
            remote_synced: HashMap::new(),

            remote_keys,
            remembered_keys: Vec::new(),
            base_description: None,
            profiles: Vec::new(),
            environment_builds: HashMap::new(),
            shimmer: 0,
            capacity: None,
        }
    }
    pub(in crate::tui) fn replace_task(&mut self, task: Task) {
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
    pub(in crate::tui) fn select_adjacent(&mut self, step: isize, columns: usize) {
        let count = self.tasks.len();
        if count == 0 {
            return;
        }
        let moved = self.selected as isize + step;
        self.selected = moved.clamp(0, count as isize - 1) as usize;
        let _ = columns;
    }

    /// How many cards fit across the body, which is what a grid step needs.
    pub(in crate::tui) fn grid_columns(&self) -> usize {
        self.grid_columns.max(1)
    }

    /// Everything a task can run in, base first.
    ///
    /// Built from the settings rather than stored on the app: the list is two or
    /// three entries, and a copy kept here would be one more thing to keep in
    /// step with the file.
    pub(in crate::tui) fn environments(&self) -> Vec<Environment> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().environments())
            .unwrap_or_else(|| vec![Environment::base()])
    }

    pub(in crate::tui) fn default_environment(&self) -> &str {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().default_environment.as_str())
            .unwrap_or_default()
    }

    pub(in crate::tui) fn reload_settings(&mut self) -> bool {
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
    pub(in crate::tui) fn agent(&self) -> Option<AgentKind> {
        self.settings_file
            .as_ref()
            .and_then(|file| file.settings().agent())
    }

    /// Choose the coding agent, and keep it.
    pub(in crate::tui) fn set_agent(&mut self, agent: AgentKind) {
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
    pub(in crate::tui) fn keyboard_hints(&self) -> bool {
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
    pub(in crate::tui) fn hints(&self) -> Vec<Hint> {
        self.hints_for(&self.foreground)
    }

    /// The keys of a given state, which help needs for the state it covers.
    pub(in crate::tui) fn hints_for(&self, front: &Foreground) -> Vec<Hint> {
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
                        Hint::new("tab", "next"),
                        Hint::new("enter", "next, or add a line"),
                        Hint::new("esc", "leave the box, then the page"),
                    ];
                }
                let mut hints = vec![Hint::new("tab", "change column"), Hint::new("j/k", "move")];
                if self.panel.section == Section::Remotes {
                    hints.push(Hint::new("enter", "check"));
                    hints.push(Hint::new("p", "send config"));
                    hints.push(Hint::new("e", "edit"));
                } else if self.panel.section.grows() {
                    hints.push(Hint::new("enter", "edit"));
                }
                if self.panel.section.grows() {
                    hints.push(Hint::new("n", "new"));
                    hints.push(Hint::new("d", "delete"));
                }
                if !self.panel.section.grows() && self.panel.section == Section::Interface {
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
                        Hint::new("enter", "signals"),
                        Hint::new("o", "take over"),
                        Hint::new("h/j/k/l", "select"),
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
    pub(in crate::tui) fn is_typing(&self) -> bool {
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
    pub(in crate::tui) fn open_shell(&self) -> Option<&str> {
        match &self.foreground {
            Foreground::Shell(id) => Some(id),
            _ => None,
        }
    }

    pub(in crate::tui) async fn panel_key(&mut self, key: KeyEvent) {
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
            KeyCode::Char('e') if self.panel.section.grows() => self.edit_selected(),
            KeyCode::Char('p') if self.panel.section == Section::Remotes => {
                if let Some(RowKind::Remote { address, .. }) =
                    self.selected_row().map(|row| row.kind)
                {
                    self.push_config(&address).await;
                }
            }
            KeyCode::Char('n') if self.panel.section.grows() => {
                self.panel.form = Some(match self.panel.section {
                    Section::Remotes => Form::new(Target::NewRemote, "", ""),
                    _ => Form::new(Target::NewEnvironment, "", ""),
                });
            }
            _ if is_remove(key) && self.panel.section.grows() => self.ask_to_delete(),
            KeyCode::Enter => self.activate_selected().await,
            _ => {}
        }
    }

    /// What `enter` does where the focus is: the row's own action, which for a
    /// remote is reaching it rather than editing the address it is reached at.
    pub(in crate::tui) async fn activate_selected(&mut self) {
        match self.selected_row().map(|row| row.kind) {
            Some(RowKind::Remote { address, .. }) => self.check_remote(&address).await,
            _ => self.edit_selected(),
        }
    }

    /// Whether a remote holds the configuration this client has.
    pub(in crate::tui) fn remote_needs_config(&self, address: &str) -> bool {
        self.remote_synced
            .get(address)
            .is_none_or(|sent| *sent < self.config.stamp())
    }

    /// Send this installation's configuration to a remote, so it can build the
    /// environments this client runs tasks in.
    pub(in crate::tui) async fn push_config(&mut self, address: &str) {
        self.notice = Some(format!("sending the configuration to {address}…"));
        self.remote_status
            .insert(address.to_owned(), RemoteStatus::Syncing);
        let stamp = self.config.stamp();
        match self.send_config(address).await {
            Ok(()) => {
                self.remote_synced.insert(address.to_owned(), stamp);
                self.notice = Some(format!("{address} holds this configuration now"));
            }
            Err(error) => {
                self.notice = Some(format!("{address} did not take it: {error}"));
            }
        }
        // Asked again either way, so the dot says what is true now rather than what
        // was hoped for.
        self.remote_status.remove(address);
    }

    /// The dial, which is what makes this a push rather than a preference.
    async fn send_config(&mut self, address: &str) -> Result<(), String> {
        let settings =
            fs::read_to_string(self.config.settings_path()).map_err(|e| e.to_string())?;
        let remembered = self.remote_keys.remembered(address).await.ok().flatten();
        let sighting = meet(address, DEFAULT_PORT, remembered.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        // The profiles it says it already has are left out: sending a configuration
        // string again costs nothing, and sending a directory of logins again does.
        let profiles: Vec<&str> = self.profiles.iter().map(|held| held.profile).collect();
        let bundle = ConfigBundle::of(
            settings,
            self.config.config_dir(),
            &profiles,
            &sighting.announcement.profiles,
        )
        .map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec(&bundle).map_err(|error| error.to_string())?;
        let client = LinkClient::bind()
            .await
            .map_err(|error| error.to_string())?;
        let sent = client
            .put_config(sighting.ticket, &bytes)
            .await
            .map_err(|error| error.to_string());
        let _ = client.close().await;
        sent
    }

    /// Note whether the remotes list is showing, and start its clock when it
    /// appears.
    pub(in crate::tui) fn sync_remotes(&mut self) {
        let showing =
            matches!(self.foreground, Foreground::Panel) && self.panel.section == Section::Remotes;
        match (showing, self.remotes_visible_since) {
            // A fresh look every time the list is opened, so what it says is about
            // now rather than about whenever it was last visited.
            (true, None) => {
                self.remotes_visible_since = Some(Instant::now());
                self.remote_status.clear();
            }
            (false, Some(_)) => self.remotes_visible_since = None,
            _ => {}
        }
    }

    /// Whether any address is being asked about right now, which is what keeps the
    /// dot moving while it waits.
    pub(in crate::tui) fn is_asking_remotes(&self) -> bool {
        self.remote_status
            .values()
            .any(RemoteStatus::is_in_progress)
    }

    /// Whether it is time to ask about another address: the list is showing, it
    /// has been showing long enough to mean it, and nothing is in flight.
    pub(in crate::tui) fn wants_remote_check(&self) -> bool {
        let Some(since) = self.remotes_visible_since else {
            return false;
        };
        since.elapsed() >= ASK_AFTER
            && !self
                .remote_status
                .values()
                .any(|s| *s == RemoteStatus::Asking)
            && self.next_unasked_remote().is_some()
    }

    /// The first address in the list that has not been asked about.
    fn next_unasked_remote(&self) -> Option<String> {
        self.configured_remotes()
            .into_iter()
            .find(|address| !self.remote_status.contains_key(address))
    }

    /// Dial the next address in the list, and let it report back when it answers.
    pub(in crate::tui) async fn ask_next_remote(
        &mut self,
        answers: &mpsc::UnboundedSender<RemoteAnswer>,
    ) {
        let Some(address) = self.next_unasked_remote() else {
            return;
        };
        self.remote_status
            .insert(address.clone(), RemoteStatus::Asking);
        let remembered = self.remote_keys.remembered(&address).await.ok().flatten();
        let answers = answers.clone();
        tokio::spawn(async move {
            let answer = match meet(&address, DEFAULT_PORT, remembered.as_deref()).await {
                Ok(sighting) => RemoteAnswer::Reached {
                    address,
                    announcement: sighting.announcement,
                    key_status: sighting.key_status,
                },
                Err(error) => RemoteAnswer::Silent {
                    address,
                    reason: error.to_string(),
                },
            };
            let _ = answers.send(answer);
        });
    }

    /// Take an answer from the dial, and write down what it said.
    pub(in crate::tui) async fn remote_answered(&mut self, answer: RemoteAnswer) {
        match answer {
            RemoteAnswer::Reached {
                address,
                announcement,
                key_status,
            } => {
                let status = match key_status {
                    RemoteKeyStatus::New | RemoteKeyStatus::Known => {
                        self.remember(&address, &announcement).await;
                        // It answered, but a machine still building what it needs
                        // cannot be given work, and that is worth knowing before you
                        // delegate to it.
                        match announcement.state {
                            RemoteState::Preparing => RemoteStatus::Syncing,
                            RemoteState::Ready => RemoteStatus::Reached,
                        }
                    }
                    // Reported, and not written: accepting a changed key means
                    // forgetting the address first, which is a second act.
                    RemoteKeyStatus::Changed => RemoteStatus::Changed,
                };
                self.remote_status.insert(address, status);
                self.reload_remembered_keys().await;
            }
            RemoteAnswer::Silent { address, reason } => {
                self.remote_status
                    .insert(address, RemoteStatus::Silent(reason));
            }
        }
    }

    /// Reach a remote at this address, and write down what it answers with.
    ///
    /// The first key an address gives is unauthenticated, so meeting it is the
    /// operator's decision, and this is the gesture that makes it. A key that
    /// differs from the one remembered is reported and *not* written: accepting a
    /// change means forgetting the address first, which is a second, deliberate
    /// act rather than something that happens because someone pressed enter.
    pub(in crate::tui) async fn check_remote(&mut self, address: &str) {
        self.notice = Some(format!("reaching {address}…"));
        let remembered = self.remote_keys.remembered(address).await.ok().flatten();
        self.notice = match meet(address, DEFAULT_PORT, remembered.as_deref()).await {
            Ok(sighting) => {
                let said = &sighting.announcement;
                match sighting.key_status {
                    RemoteKeyStatus::New => {
                        self.remember(address, said).await;
                        Some(format!(
                            "{} answered, and its key is remembered",
                            said.label
                        ))
                    }
                    RemoteKeyStatus::Known => {
                        self.remember(address, said).await;
                        Some(format!(
                            "{} answered with the same key as before",
                            said.label
                        ))
                    }
                    RemoteKeyStatus::Changed => Some(format!(
                        "{address} answered with a different key. d forgets the one you have, \
                         then enter accepts this one."
                    )),
                }
            }
            Err(error) => Some(format!("{address} did not answer: {error}")),
        };
        self.reload_remembered_keys().await;
    }

    /// Write down what a remote said about itself.
    async fn remember(&mut self, address: &str, said: &Announcement) {
        if let Err(error) = self
            .remote_keys
            .remember(address, &said.id, &said.label, &said.version)
            .await
        {
            tracing::error!(%error, address, "could not remember a remote");
        }
    }

    /// Read back what each address has answered with, which is what the panel shows.
    async fn reload_remembered_keys(&mut self) {
        match self.remote_keys.list().await {
            Ok(keys) => self.remembered_keys = keys,
            Err(error) => tracing::error!(%error, "could not read the remembered remotes"),
        }
    }

    pub(in crate::tui) fn edit_selected(&mut self) {
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

    pub(in crate::tui) fn ask_to_delete(&mut self) {
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
    pub(in crate::tui) fn toggle(&mut self, setting: Setting, on: bool) {
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

    pub(in crate::tui) async fn panel_form_key(&mut self, key: KeyEvent) {
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
            let added = match form.target {
                Target::NewRemote => Some(form.primary.value().trim().to_owned()),
                _ => None,
            };
            // A refused save keeps the page open, so the reason and the work
            // are both still there.
            if !self.save_form(&form) {
                self.panel.form = Some(form);
            } else if let Some(address) = added {
                // A remote that has just been added is sent this configuration, so
                // that it becomes a replica rather than a machine waiting to be told
                // what to build.
                self.push_config(&address).await;
            }
        } else if keep {
            self.panel.form = Some(form);
        }
    }

    /// Save what a form holds, answering whether it was saved.
    pub(in crate::tui) fn save_form(&mut self, form: &Form) -> bool {
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
    pub(in crate::tui) async fn delete(&mut self, target: Target) {
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

    pub(in crate::tui) fn selected_task(&self) -> Option<Task> {
        self.tasks.get(self.selected).cloned()
    }

    pub(in crate::tui) fn remove_task(&mut self, id: &str) {
        self.removing.remove(id);
        if let Some(index) = self.tasks.iter().position(|task| task.id == id) {
            self.tasks.remove(index);
            self.selected = self.selected.min(self.tasks.len().saturating_sub(1));
        }
    }

    pub(in crate::tui) fn begin_removing(&mut self, id: &str) {
        self.removing.insert(id.to_owned());
    }

    pub(in crate::tui) fn cancel_removing(&mut self, id: &str) {
        self.removing.remove(id);
    }

    pub(in crate::tui) fn is_removing(&self, id: &str) -> bool {
        self.removing.contains(id)
    }

    pub(in crate::tui) fn hide_shell(&mut self) {
        tracing::info!(task_id = ?self.open_shell(), "detaching terminal");
        self.foreground = Foreground::Tasks;
    }

    pub(in crate::tui) fn remove_shell(&mut self, id: &str) {
        self.shells.remove(id);
        if self.open_shell() == Some(id) {
            self.foreground = Foreground::Tasks;
        }
    }

    /// Drop a signal the operator has just resolved, keeping the highlight inside
    /// the list that remains.
    pub(in crate::tui) fn resolved_operator_signal(&mut self, index: usize) {
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
    pub(in crate::tui) fn selected_signal_actions(
        &self,
    ) -> &'static [popups::signals::SignalAction] {
        self.operator_signals
            .get(self.selected_operator_signal)
            .map_or(&[], |signal| popups::signals::actions_for(signal.kind))
    }

    /// Move the focus between the selected signal's actions.
    pub(in crate::tui) fn focus_next_signal_action(&mut self) {
        let count = self.selected_signal_actions().len();
        if count > 0 {
            self.selected_signal_action = (self.selected_signal_action + 1) % count;
        }
    }

    pub(in crate::tui) fn focus_previous_signal_action(&mut self) {
        let count = self.selected_signal_actions().len();
        if count > 0 {
            self.selected_signal_action = (self.selected_signal_action + count - 1) % count;
        }
    }

    pub(in crate::tui) fn select_next_operator_signal(&mut self) {
        self.operator_signal_scroll = 0;
        // Focus follows the selection rather than the list position, so moving to
        // another signal always starts from its first action.
        self.selected_signal_action = 0;
        if !self.operator_signals.is_empty() {
            self.selected_operator_signal =
                (self.selected_operator_signal + 1) % self.operator_signals.len();
        }
    }

    pub(in crate::tui) fn select_previous_operator_signal(&mut self) {
        self.operator_signal_scroll = 0;
        self.selected_signal_action = 0;
        if !self.operator_signals.is_empty() {
            self.selected_operator_signal =
                (self.selected_operator_signal + self.operator_signals.len() - 1)
                    % self.operator_signals.len();
        }
    }
}

impl App {
    /// Close the keys, returning to what they covered.
    pub(in crate::tui) fn dismiss_keys(&mut self) {
        if let Foreground::Keys(behind) = mem::take(&mut self.foreground) {
            self.foreground = *behind;
        }
    }

    /// Take the answer to a reply, and act on it.
    pub(in crate::tui) fn reply_key<E>(&mut self, key: KeyEvent, event_loop: &TaskEventLoop<E>)
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
    pub(in crate::tui) fn shell_key<E>(&mut self, key: KeyEvent, event_loop: &TaskEventLoop<E>)
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
    pub(in crate::tui) fn new_task_key<E>(&mut self, key: KeyEvent, event_loop: &TaskEventLoop<E>)
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
    pub(in crate::tui) async fn panel_keys(&mut self, key: KeyEvent) {
        if self.panel.form.is_some() {
            self.panel_form_key(key).await;
        } else {
            self.panel_key(key).await;
        }
    }

    /// The keys of the signals pop-up.
    pub(in crate::tui) async fn signals_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
    ) where
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
    pub(in crate::tui) async fn list_key<E>(
        &mut self,
        key: KeyEvent,
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
            KeyCode::Char('i') => self.open_signals(task_service).await,
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
            // Enter reads what the task is asking. Taking over has a key of its
            // own, and asks first, because it ends the agent's autonomy.
            KeyCode::Enter => self.open_signals(task_service).await,
            KeyCode::Char('o') => {
                if let Some(task) = self.selected_task() {
                    let question = format!(
                        "Take over \"{}\"? The agent stops and the terminal is yours; \
                         ctrl-c hands it back.",
                        task.facts.intent
                    );
                    self.foreground =
                        Foreground::Confirm(PendingConfirm::taking_over(task.id, question));
                }
            }
            _ => {}
        }
        Ok(false)
    }

    /// Stop the agent and give the operator its terminal.
    ///
    /// This is the one action never taken by accident: it ends the run's autonomy,
    /// so it has a key of its own and it asks first. A task hosted by a remote is
    /// also brought home by it, because a terminal has to be beside the capsule —
    /// and handing control back sends it there again, which is the other half.
    pub(in crate::tui) fn take_over<E>(
        &mut self,
        id: &str,
        terminal: &DefaultTerminal,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(task) = self.tasks.iter().find(|task| task.id == id).cloned() else {
            return;
        };
        if matches!(
            task.facts.execution_status,
            TaskExecutionStatus::Paused | TaskExecutionStatus::Failed
        ) {
            event_loop.start_operator_control(task);
            self.notice = Some("restoring the agent in the terminal…".into());
            return;
        }
        if !matches!(task.facts.execution_status, TaskExecutionStatus::Running) {
            self.notice = Some("this task is not running".into());
            return;
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
            return;
        }
        // Already the operator's: put its terminal back in front.
        if self.shells.contains_key(&task.id) {
            let Some((rows, cols)) = self.shell_size(terminal) else {
                return;
            };
            event_loop.resize_terminal(&task.id, rows, cols);
            if let Some(shell) = self.shells.get_mut(&task.id) {
                shell.resize(rows, cols);
            }
            self.foreground = Foreground::Shell(task.id);
            return;
        }
        if !event_loop.has_active_run(&task.id) {
            self.notice = Some("this task has no run to attach to".into());
            return;
        }
        let Some((rows, cols)) = self.shell_size(terminal) else {
            return;
        };
        let history = event_loop.cached_terminal_output(&task.id);
        event_loop.resize_terminal(&task.id, rows, cols);
        self.shells
            .insert(task.id.clone(), Shell::new(rows, cols, history));
        self.foreground = Foreground::Shell(task.id);
    }

    /// The size a terminal for a task should be, saying so when it cannot be read
    /// rather than taking the whole interface down with it.
    fn shell_size(&mut self, terminal: &DefaultTerminal) -> Option<(u16, u16)> {
        match terminal.size() {
            Ok(size) => Some(screens::shell::size(size.into())),
            Err(error) => {
                self.notice = Some(error.to_string());
                None
            }
        }
    }

    /// Read what a task is asking, and show it.
    pub(in crate::tui) async fn open_signals<E>(&mut self, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(task) = self.selected_task() else {
            return;
        };
        let signals = match task_service.operator_signals(&task.id).await {
            Ok(signals) => signals,
            Err(error) => {
                self.notice = Some(error.to_string());
                return;
            }
        };
        let read = match task_service.read_operator_signals(&task.id).await {
            Ok(task) => task,
            Err(error) => {
                self.notice = Some(error.to_string());
                return;
            }
        };
        self.operator_signals = signals;
        self.replace_task(read);
        for signal in &mut self.operator_signals {
            signal.read = true;
        }
        self.selected_operator_signal = 0;
        self.selected_signal_action = 0;
        self.operator_signal_scroll = 0;
        self.foreground = Foreground::Signals;
    }

    /// Take one change from a task's run, and make the interface match it.
    pub(in crate::tui) async fn apply<E>(
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

    pub(in crate::tui) fn shell_has_active_notice(&self) -> bool {
        self.open_shell()
            .and_then(|id| self.shells.get(id))
            .is_some_and(Shell::has_active_notice)
    }

    /// Whether any task can currently be waiting on an environment build.
    pub(in crate::tui) fn is_preparing(&self) -> bool {
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
