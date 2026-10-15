//! The application: what it is holding, and what it does with it.
//!
//! One value holds everything a running client knows — the tasks, the settings,
//! the shells, and what the keyboard is answering to — and the methods here are
//! what it does in response. Nothing in this file talks to a terminal except to
//! draw, and nothing here reads an event.

use std::{
    collections::{HashMap, HashSet},
    env, fs, io, mem,
    path::PathBuf,
    process,
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use iroh::EndpointAddr;
use ratatui::{DefaultTerminal, layout::Rect, text::Line};
use tokio::{process::Command as HostCommand, sync::mpsc, task::JoinHandle};

use crate::{
    agent::AgentKind,
    config::{Config, DEFAULT_PORT, SettingsError, SettingsFile},
    isolation::{
        Environment, EnvironmentDescription, EnvironmentProgress, IsolationEngine, ProfileSummary,
    },
    remote::{
        Announcement, ConfigBundle, LinkClient, RemoteKey, RemoteKeyStatus, RemoteKeys,
        RemoteState, TaskOrigin, TaskRequest, TaskSummary, TrackerConnection, bring_home, meet,
        send::send_to_remote,
    },
    task,
    task::{
        CredentialShell, CredentialShellCancel, CredentialShellEvent, Task, TaskEventLoop,
        TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus, TaskExecutionUpdate,
        TaskOperatorSignal, TaskService, TaskServiceError,
    },
    tracker::{IssueReference, IssueSnapshot, IssueSummary},
    tui::keys::{is_dismiss, is_remove},
    tui::popups,
    tui::screens,
    tui::widgets::chrome::Hint,
};

use popups::confirm::{CredentialTarget, PendingConfirm};
use popups::new_task::{Answer, Host, NewTask, SearchToken};
use popups::open_link::{self, Link, OpenLink};
use popups::reply::{Prompt, PromptAction, Reply};
use popups::send_task::{Decision as SendDecision, SendTask};
use popups::setup::{Setup, SetupKind};
use screens::settings::{
    CredentialKind, Depth, Focus, Form, GitIdentity, Panel, RowKind, Section, Setting, Target,
};
use screens::shell::{GuestRequest, Shell};

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
    /// Choosing a machine to continue a task that is already here.
    SendTask(Box<SendTask>),
    /// The creation question, holding the prompt until it is answered.
    NewTask(Box<NewTask>),
    /// The settings panel: environments, remotes, the interface, the identity.
    Panel,
    Setup(Setup),
    /// A task's signals, over the list it was opened from.
    Signals,
    /// A terminal, taking every key there is.
    Shell(ShellId),
    /// A sandbox asking to open a link, over the terminal that asked.
    OpenLink(OpenLink),
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
    /// Whether something is happening, in which case the dot travels like every
    /// other wait in the interface.
    pub(in crate::tui) fn is_in_progress(&self) -> bool {
        matches!(self, Self::Asking | Self::Syncing)
    }
}

/// What a task's terminal is called on its frame, as opposed to the shell opened to
/// set up credentials, which names the profile it is for.
const TASK_SHELL: &str = "shell";

/// What a profile's own check last said about its login.
///
/// Asked rather than known, and asked the way a remote's address is reached: one at a
/// time, off the loop, with a mark in front of the row that says which it is doing.
#[derive(Clone, Default, PartialEq, Eq)]
pub(in crate::tui) enum CredentialStatus {
    /// Not asked yet.
    #[default]
    Unknown,
    /// Asked, and nothing has come back.
    Checking,
    /// Answered, with what the profile's own command printed.
    Checked { signed_in: bool, output: String },
}

impl CredentialStatus {
    fn is_in_progress(&self) -> bool {
        *self == Self::Checking
    }
}

pub(in crate::tui) struct CredentialCheckReply {
    pub profile: &'static str,
    pub status: CredentialStatus,
    pub generation: u64,
}

/// What is known about the Linear tracker's live session.
///
/// The provider's own `is_connected` is the only truth here: a sign-in profile
/// kept on disk is not a connection, so it is never shown as one. Signing in
/// opens a browser and waits on an OAuth callback, which is minutes rather than
/// frames, so it runs off the loop and reports back as one of these.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::tui) enum TrackerState {
    /// No live session: nothing is signed in, or a session ended.
    #[default]
    NotConnected,
    /// A sign-in is out, waiting on the browser and the callback.
    Connecting,
    /// The provider reports a live session.
    Connected,
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

/// What came back from a Linear search. The token says which popup and which
/// query it answers, so a late one is dropped rather than shown under a query it
/// was not about.
pub(in crate::tui) struct IssueSearchReply {
    pub token: SearchToken,
    pub result: Result<Vec<IssueSummary>, String>,
}

/// What came back from reading one issue's snapshot. The token says which
/// submission it belongs to, so a snapshot that lands after the question was
/// dismissed cannot still found a task.
pub(in crate::tui) struct IssueReadReply {
    pub token: u64,
    pub result: Result<IssueSnapshot, String>,
}

/// What came back from finishing a Linear sign-in.
///
/// The bool is the provider's own answer to `is_connected` after connecting,
/// because a profile on disk is not a connection and only a live session makes
/// the row say Connected.
pub(in crate::tui) struct TrackerConnectReply {
    /// Which attempt this answers, so a reply for one already given up on
    /// changes nothing.
    pub attempt: u64,
    pub result: Result<bool, String>,
}

/// The notice the sign-in sets, kept as one value so a reply can clear its own
/// without erasing whatever else has happened since.
const SIGN_IN_NOTICE: &str = "Finish Linear sign-in in your browser";

/// What it says when the build carries no tracker provider at all.
pub(in crate::tui) const NO_LINEAR: &str = "Linear support is not available in this build.";

/// How long the list is shown before its addresses are dialled, so moving through
/// the sections does not reach anything.
const ASK_AFTER: Duration = Duration::from_millis(400);

/// Which terminal is in front.
///
/// One shell with two owners: everything about drawing it, sizing it, pasting into
/// it and copying out of it is the same either way, and only where its keys go
/// differs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::tui) enum ShellId {
    /// A task's run, whose keys go to the task's event loop.
    Task(String),
    /// A shell opened to set up credentials, whose keys go to its own sandbox.
    Credentials(String),
}

/// Where a credential shell is.
#[derive(Default)]
pub(in crate::tui) enum CredentialShellState {
    /// No shell, and none being made.
    #[default]
    Idle,
    /// The sandbox is being made, which is a wait rather than a frame.
    Opening {
        id: String,
        display: String,
        progress: String,
        cancel: CredentialShellCancel,
    },
    /// A terminal to draw, and the channel its keys go down.
    Open(CredentialShell),
}

/// A task another machine owns, and which machine that is.
///
/// Which machine matters, because a task belongs to the machine running it: asking for
/// one is a question put to a particular machine, not to remotes in general.
#[derive(Clone)]
pub(in crate::tui) struct HeldTask {
    pub(in crate::tui) address: String,
    pub(in crate::tui) summary: TaskSummary,
}

/// The remote task the signals pop-up is about.
///
/// The attempt says which opening this is, so a reply carrying another attempt
/// belongs to a pop-up the operator has already left and changes nothing.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::tui) struct RemoteSignals {
    pub(in crate::tui) address: String,
    pub(in crate::tui) task_id: String,
    pub(in crate::tui) attempt: u64,
}

/// What the operator did with a remote signal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum RemoteSignalAction {
    Answer,
    Acknowledge,
    Confirm,
}

/// What came back from a remote task's signals: the list when it opens, or the
/// outcome of acting on one.
pub(in crate::tui) enum RemoteSignalsReply {
    Opened {
        remote: RemoteSignals,
        signals: Result<Vec<TaskOperatorSignal>, String>,
    },
    Resolved {
        remote: RemoteSignals,
        signal_id: String,
        action: RemoteSignalAction,
        result: Result<(), String>,
    },
}

/// What came back from asking a remote to run a task. The address names the
/// machine that answered, so the notice does.
pub(in crate::tui) struct TaskDelegationReply {
    pub(in crate::tui) address: String,
    pub(in crate::tui) result: Result<(), String>,
}

/// What came back from sending an existing task to a machine. The result is the
/// id that machine gave the task, once it has booted it and ownership moved.
pub(in crate::tui) struct SendTaskReply {
    pub(in crate::tui) address: String,
    pub(in crate::tui) result: Result<String, String>,
}

/// What came back from sending this client's configuration to a remote.
///
/// The stamp is the one the bundle was built from, so a settings edit made while
/// the send was in flight is not mistaken for one the remote already holds.
pub(in crate::tui) struct ConfigPushReply {
    pub(in crate::tui) address: String,
    pub(in crate::tui) stamp: i64,
    pub(in crate::tui) result: Result<(), String>,
}

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
    pub(in crate::tui) help_scroll: u16,
    /// What the host will run at once, so a wait can say whether there is room.
    /// Absent until the first refresh, and never stale by more than one interval.
    pub(in crate::tui) capacity: Option<usize>,
    pub(in crate::tui) notice: Option<String>,
    pub(in crate::tui) removing: HashSet<String>,
    pub(in crate::tui) selected: usize,
    pub(in crate::tui) first_visible_task: usize,
    pub(in crate::tui) shells: HashMap<ShellId, Shell>,
    /// The shell opened to set up credentials, if there is one.
    ///
    /// One at a time: a profile is a shared directory, and two shells writing it
    /// is a way to confuse yourself.
    pub(in crate::tui) credential_shell: CredentialShellState,
    git_identity: GitIdentity,
    /// The selected task's signals, and where you are among them. Read when the
    /// signals open; the pop-up is a view over what the task already recorded.
    pub(in crate::tui) operator_signals: Vec<TaskOperatorSignal>,
    pub(in crate::tui) selected_operator_signal: usize,
    pub(in crate::tui) selected_signal_action: usize,
    pub(in crate::tui) operator_signal_scroll: u16,
    /// The remote task the signals pop-up is about, when it is showing one from
    /// another machine.
    pub(in crate::tui) remote_signals: Option<RemoteSignals>,
    /// Whether that remote's signals are still being read.
    pub(in crate::tui) remote_signals_loading: bool,
    /// Which opening of the pop-up this is, so a reply from an earlier one is
    /// dropped rather than replacing what is on screen.
    remote_signals_attempt: u64,
    pairing_replies: mpsc::Sender<Result<(String, String), String>>,
    pub(in crate::tui) pairings_replied: mpsc::Receiver<Result<(String, String), String>>,
    /// Where `config.toml` lives, so the manage view can persist changes.
    pub(in crate::tui) config: Config,
    pub(in crate::tui) settings_file: Option<SettingsFile>,
    /// The one link to remotes, shared by every exchange rather than bound per dial.
    pub(in crate::tui) link: Arc<LinkClient>,
    /// The settings panel: environments, remotes, the interface, and the identity.
    pub(in crate::tui) panel: Panel,
    /// What the keyboard is answering to.
    pub(in crate::tui) foreground: Foreground,
    /// What each address last answered, for the list to show.
    pub(in crate::tui) remote_status: HashMap<String, RemoteStatus>,
    /// What each address's last announcement said about the trackers it runs.
    /// Runtime only: it is a fact about the remote, not something to persist.
    pub(in crate::tui) remote_trackers: HashMap<String, Vec<TrackerConnection>>,
    /// What each profile's own check last said about its login.
    pub(in crate::tui) credential_status: HashMap<&'static str, CredentialStatus>,
    /// When the credentials list started showing, which the wait before asking is
    /// measured from. Absent while it is not showing.
    credentials_visible_since: Option<Instant>,
    credential_check_generation: u64,
    /// When the remotes list started showing, which the wait before dialling is
    /// measured from. Absent while it is not showing.
    remotes_visible_since: Option<Instant>,
    /// What the remotes are holding, as of the last time each answered. Read from
    /// them rather than copied: a task belongs to the machine running it, so this is
    /// only ever as good as the last answer.
    pub(in crate::tui) remote_tasks: Vec<HeldTask>,
    /// The remote card to select once the next read of a remote's tasks lands,
    /// so a task that has just been sent there is the one in view.
    pending_remote: Option<(String, String)>,
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
    /// What the Linear tracker's session is doing, read when the panel opens
    /// and updated when a sign-in lands.
    pub(in crate::tui) tracker_state: TrackerState,
    /// The sign-in in flight, so a second enter can give up on it and leaving
    /// the interface can take it down with it. The provider owns the browser and
    /// the OAuth callback; dropping this future is what cancels them.
    tracker_connect: Option<JoinHandle<()>>,
    tracker_connect_attempt: u64,
}

impl Drop for App {
    fn drop(&mut self) {
        // The provider's future cleans up its broker when it is dropped, so the
        // sign-in must not outlive the interface that asked for it.
        if let Some(handle) = self.tracker_connect.take() {
            handle.abort();
        }
    }
}

impl App {
    pub fn new(
        tasks: Vec<Task>,
        config: Config,
        settings: Result<SettingsFile, SettingsError>,
        remote_keys: RemoteKeys,
        link: LinkClient,
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
        let (pairing_replies, pairings_replied) = mpsc::channel(8);
        Self {
            tasks,
            grid_columns: 1,
            notice: settings_error,
            removing: HashSet::new(),
            selected: 0,
            first_visible_task: 0,
            shells: HashMap::new(),
            credential_shell: CredentialShellState::Idle,
            git_identity: GitIdentity::default(),
            operator_signals: Vec::new(),
            selected_operator_signal: 0,
            selected_signal_action: 0,
            operator_signal_scroll: 0,
            remote_signals: None,
            remote_signals_loading: false,
            remote_signals_attempt: 0,
            pairing_replies,
            pairings_replied,
            config,
            link: Arc::new(link),
            settings_file,
            panel: Panel::new(),
            foreground: Foreground::default(),
            remote_status: HashMap::new(),
            remote_trackers: HashMap::new(),
            credential_status: HashMap::new(),
            credentials_visible_since: None,
            credential_check_generation: 0,
            remotes_visible_since: None,
            remote_synced: HashMap::new(),
            remote_tasks: Vec::new(),
            pending_remote: None,

            remote_keys,
            remembered_keys: Vec::new(),
            base_description: None,
            profiles: Vec::new(),
            tracker_state: TrackerState::NotConnected,
            tracker_connect: None,
            tracker_connect_attempt: 0,
            environment_builds: HashMap::new(),
            shimmer: 0,
            help_scroll: 0,
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
        let count = self.tasks.len() + self.remote_tasks.len();
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

    /// The name of the environment a new task would get.
    pub(in crate::tui) fn default_environment_name(&self) -> &str {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().default_environment.as_str())
            .unwrap_or_default()
    }

    /// The environment a new task would get.
    ///
    /// A credential shell is opened in it, because the credentials being set up are
    /// the ones tasks read.
    pub(in crate::tui) fn default_environment_value(&self) -> Environment {
        let default = self.default_environment_name();
        self.environments()
            .into_iter()
            .find(|environment| environment.name.eq_ignore_ascii_case(default))
            .unwrap_or_else(Environment::base)
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

    /// The few actions worth keeping visible below the current screen.
    pub(in crate::tui) fn hints(&self) -> Vec<Hint> {
        let mut hints = self.hints_for(&self.foreground);
        hints.retain(|hint| {
            !matches!(hint.keys, "tab" | "j/k" | "h/j/k/l" | "pgup/pgdn")
                && !matches!((hint.keys, hint.what), ("esc", "close") | ("q", "quit"))
        });
        for hint in &mut hints {
            if hint.keys == "enter / tab" {
                hint.keys = "enter";
            }
        }
        if !self.is_typing()
            && self.open_shell().is_none()
            && !hints.iter().any(|hint| hint.keys == "?")
        {
            hints.push(Hint::new("?", "all shortcuts"));
        }
        hints
    }

    /// Candidate footer actions before common navigation is removed.
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
            Foreground::SendTask(sending) => sending.hints(),
            Foreground::NewTask(new_task) => new_task.hints(),
            Foreground::Panel => {
                if matches!(self.credential_shell, CredentialShellState::Opening { .. }) {
                    return vec![Hint::new("esc / q", "cancel")];
                }
                if self.panel.depth == Depth::Rail {
                    return vec![
                        Hint::new("j/k", "section"),
                        Hint::new("enter / tab", "open"),
                        dismiss(),
                    ];
                }
                if self.panel.form.is_some() {
                    return vec![
                        Hint::new("tab", "next"),
                        Hint::new("enter", "next, or add a line"),
                        Hint::new("esc", "leave the box, then the page"),
                    ];
                }
                let mut hints = vec![Hint::new("tab", "change column"), Hint::new("j/k", "move")];
                if self.panel.section == Section::Remotes {
                    if matches!(
                        self.selected_row().map(|row| row.kind),
                        Some(RowKind::Remote { .. })
                    ) {
                        hints.push(Hint::new("enter", "connect"));
                        hints.push(Hint::new("p", "send config"));
                        hints.push(Hint::new("t", "task trackers"));
                        hints.push(Hint::new("b", "pair Linear"));
                        hints.push(Hint::new("e", "edit"));
                    } else {
                        hints.push(Hint::new("enter", "remote setup"));
                    }
                } else if self.panel.section.grows() {
                    hints.push(Hint::new("enter", "edit"));
                } else if self.panel.section == Section::Agents {
                    if matches!(
                        self.selected_row().map(|row| row.kind),
                        Some(RowKind::Agent { .. })
                    ) {
                        hints.push(Hint::new("enter", "sign in"));
                        hints.push(Hint::new("d", "use by default"));
                    }
                } else if self.panel.section == Section::Interface {
                    hints.push(Hint::new("enter", "toggle"));
                } else if self.panel.section == Section::VersionControl {
                    hints.push(Hint::new("enter", "open setup shell"));
                } else if self.panel.section == Section::Trackers {
                    hints.push(Hint::new(
                        "enter",
                        if self.is_connecting_tracker() {
                            "cancel sign-in"
                        } else {
                            "sign in"
                        },
                    ));
                }
                if self.selected_has_credentials() {
                    hints.push(Hint::new("delete", "sign out"));
                }
                if self.panel.section.grows() {
                    hints.push(Hint::new("n", "new"));
                    if self.panel.section != Section::Remotes
                        || matches!(
                            self.selected_row().map(|row| row.kind),
                            Some(RowKind::Remote { .. })
                        )
                    {
                        hints.push(Hint::new("d", "delete"));
                    }
                }
                hints.push(Hint::new("pgup/pgdn", "details"));
                hints.push(dismiss());
                hints
            }
            Foreground::Setup(setup) => match &setup.kind {
                SetupKind::RemoteTrackers => vec![
                    Hint::new("enter", "setup guide"),
                    Hint::new("esc / q", "close"),
                ],
                SetupKind::RemoteTrackerGuide(_) => vec![Hint::new("esc / q", "back")],
                _ => vec![Hint::new("esc / q", "close")],
            },
            Foreground::Signals => vec![
                Hint::new("tab", "action"),
                Hint::new("enter", "validate"),
                Hint::new("j/k", "signal"),
                Hint::new("pgup/pgdn", "scroll"),
                dismiss(),
            ],
            // A shell takes every key there is, so it has no `?` to offer.
            Foreground::Shell(ShellId::Task(_)) => {
                vec![Hint::new("ctrl-a", "hand back to autopilot")]
            }
            // Nothing to hand back to: the sandbox goes with the shell.
            Foreground::Shell(ShellId::Credentials(_)) => vec![Hint::new("ctrl-a", "close")],
            Foreground::OpenLink(asking) => asking.hints(),
            Foreground::Tasks => {
                // Only what there is something to do it to. A remote's tasks sit
                // beside this machine's, so a list holding only theirs still has
                // something to select and act on.
                let mut hints = vec![Hint::new("c", "create"), Hint::new("s", "settings")];
                if !self.tasks.is_empty() || !self.remote_tasks.is_empty() {
                    hints.extend([
                        Hint::new("enter", "signals"),
                        Hint::new("o", "take over"),
                        Hint::new("h/j/k/l", "select"),
                    ]);
                }
                // Removing a task is this machine's act on its own capsule; a
                // remote's task is not here to remove, and neither is it this
                // machine's to send anywhere.
                if self.selected_is_remote() {
                    if self
                        .selected_held()
                        .is_some_and(|held| held.summary.tracker_issue.is_some())
                    {
                        hints.push(Hint::new("b", "open issue"));
                    }
                } else {
                    if !self.tasks.is_empty() {
                        hints.push(Hint::new("d", "remove"));
                        hints.push(Hint::new("r", "send to remote"));
                    }
                    if self
                        .selected_task()
                        .is_some_and(|task| task.facts.tracker_issue.is_some())
                    {
                        hints.push(Hint::new("b", "open issue"));
                    }
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

    /// The terminal in front, if one is. A question a sandbox asked is asked over
    /// the terminal it asked from, which goes on running behind it.
    pub(in crate::tui) fn open_shell(&self) -> Option<&ShellId> {
        match &self.foreground {
            Foreground::Shell(id) => Some(id),
            Foreground::OpenLink(asking) => Some(asking.shell()),
            _ => None,
        }
    }

    /// Send bytes to whichever shell is in front, wherever its keys go.
    pub(in crate::tui) fn send_shell_input<E>(
        &self,
        id: &ShellId,
        bytes: Vec<u8>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        match id {
            ShellId::Task(task_id) => event_loop.send_terminal_input(task_id, bytes),
            ShellId::Credentials(_) => {
                if let CredentialShellState::Open(shell) = &self.credential_shell {
                    shell.input(bytes);
                }
            }
        }
    }

    /// Tell whichever shell is in front how big its terminal is.
    pub(in crate::tui) fn resize_shell<E>(
        &mut self,
        rows: u16,
        cols: u16,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(id) = self.open_shell().cloned() else {
            return;
        };
        if let Some(shell) = self.shells.get_mut(&id) {
            shell.resize(rows, cols);
        }
        match &id {
            ShellId::Task(task_id) => event_loop.resize_terminal(task_id, rows, cols),
            ShellId::Credentials(_) => {
                if let CredentialShellState::Open(shell) = &self.credential_shell {
                    shell.resize(rows, cols);
                }
            }
        }
    }

    /// Open a terminal in a throwaway sandbox, for one profile's login.
    ///
    /// Off the loop, because the sandbox may have to be built first and a build is
    /// minutes rather than frames. Altitude is not in the way of the credential
    /// either: the CLI writes it where the profile is mounted, and the interface
    /// only ever reads names and places.
    pub(in crate::tui) fn open_credential_shell<E>(
        &mut self,
        display: &str,
        profile: &'static str,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if !matches!(self.credential_shell, CredentialShellState::Idle) {
            self.notice = Some("a shell is already open".to_owned());
            return;
        }
        let cancel = CredentialShellCancel::new();
        let id = task::credential_shell_id();
        self.credential_shell = CredentialShellState::Opening {
            id: id.clone(),
            display: display.to_owned(),
            progress: "Preparing your sign-in environment…".to_owned(),
            cancel: cancel.clone(),
        };
        self.notice = None;
        let environment = self.default_environment_value();
        let chosen = self.agent();
        let service = task_service.clone();
        let events = events.clone();
        tokio::spawn(async move {
            match service
                .open_credential_shell(id.clone(), environment, profile, chosen, cancel, &events)
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    let _ = events.send(CredentialShellEvent::Unavailable {
                        id,
                        reason: error.to_string(),
                    });
                }
            }
        });
    }

    /// Take what came back from a credential shell.
    pub(in crate::tui) async fn credential_shell_event<E>(
        &mut self,
        event: CredentialShellEvent,
        terminal: &DefaultTerminal,
        task_service: &TaskService<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let expected = match &self.credential_shell {
            CredentialShellState::Opening { id, .. } => Some(id.as_str()),
            CredentialShellState::Open(shell) => Some(shell.id()),
            CredentialShellState::Idle => None,
        };
        if expected != Some(event.id()) {
            if let CredentialShellEvent::Opened(shell) = event {
                let service = task_service.clone();
                tokio::spawn(async move {
                    let _ = service.close_credential_shell(shell.id()).await;
                });
            }
            return;
        }
        match event {
            CredentialShellEvent::Preparing { label, .. } => {
                if let CredentialShellState::Opening { progress, .. } = &mut self.credential_shell {
                    *progress = label;
                }
            }
            CredentialShellEvent::Opened(shell) => {
                let CredentialShellState::Opening { display, .. } =
                    mem::replace(&mut self.credential_shell, CredentialShellState::Idle)
                else {
                    // Nothing is waiting for it any more, so it has no reason to run.
                    if let Err(error) = task_service.close_credential_shell(shell.id()).await {
                        tracing::warn!(%error, "could not remove an unwanted credential shell");
                    }
                    return;
                };
                let id = ShellId::Credentials(shell.id().to_owned());
                self.foreground = Foreground::Shell(id.clone());
                self.notice = None;
                let Some((rows, cols)) = self.shell_size(terminal) else {
                    let service = task_service.clone();
                    tokio::spawn(async move {
                        let _ = service.close_credential_shell(shell.id()).await;
                    });
                    self.foreground = Foreground::Panel;
                    return;
                };
                self.shells.insert(
                    id.clone(),
                    Shell::new(rows, cols, Vec::new(), format!("credentials — {display}")),
                );
                shell.resize(rows, cols);
                self.credential_shell = CredentialShellState::Open(shell);
                self.foreground = Foreground::Shell(id);
                self.notice = None;
            }
            CredentialShellEvent::Output { id, bytes } => {
                self.process_shell_output(&ShellId::Credentials(id), bytes)
            }
            CredentialShellEvent::Exited { code, .. } => {
                if code != 0 {
                    self.notice = Some(format!("the shell exited with {code}"));
                }
                self.close_credential_shell(task_service).await;
            }
            CredentialShellEvent::Failed { error, .. } => {
                self.notice = Some(format!("the shell ended: {error}"));
                self.close_credential_shell(task_service).await;
            }
            CredentialShellEvent::Unavailable { reason, .. } => {
                self.credential_shell = CredentialShellState::Idle;
                self.notice = Some(format!("no shell: {reason}"));
            }
        }
    }

    /// Throw a credential shell away, and go back to where it was opened from.
    ///
    /// The shell is not a task: what it wrote is in the profile, which is a host
    /// directory, and the sandbox only ever had to last as long as the terminal.
    pub(in crate::tui) async fn close_credential_shell<E>(&mut self, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        let state = mem::take(&mut self.credential_shell);
        let CredentialShellState::Open(shell) = state else {
            if let CredentialShellState::Opening { cancel, .. } = state {
                cancel.cancel();
            }
            self.credential_shell = CredentialShellState::Idle;
            return;
        };
        let id = ShellId::Credentials(shell.id().to_owned());
        self.shells.remove(&id);
        if self.open_shell() == Some(&id) {
            // Back to the credentials it was about, so the row can say what changed.
            self.foreground = Foreground::Panel;
        }
        let service = task_service.clone();
        tokio::spawn(async move {
            if let Err(error) = service.close_credential_shell(shell.id()).await {
                tracing::warn!(%error, "could not remove a credential shell's sandbox");
            }
        });
        self.credentials_visible_since = None;
        self.refresh_git_identity().await;
    }

    pub(in crate::tui) async fn panel_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
        config_pushes: &mpsc::Sender<ConfigPushReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if self.panel.depth == Depth::Rail && key.code == KeyCode::Enter {
            self.panel.depth = Depth::List;
            return;
        }
        let previous_section = self.panel.section;
        let rows = self.row_count();
        match key.code {
            KeyCode::PageDown => {
                self.panel.detail_scroll = self.panel.detail_scroll.saturating_add(5)
            }
            KeyCode::PageUp => {
                self.panel.detail_scroll = self.panel.detail_scroll.saturating_sub(5)
            }
            KeyCode::Home => self.panel.detail_scroll = 0,
            _ if is_dismiss(key) || key.code == KeyCode::Char('s') => {
                self.foreground = Foreground::Tasks;
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                // A row with nothing inside it has no deeper level to open.
                if self.panel.depth != Depth::List
                    || self.selected_row().is_some_and(|row| row.opens_form())
                {
                    self.panel.depth = self.panel.depth.deeper();
                }
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.panel.depth = self.panel.depth.shallower()
            }
            KeyCode::Down | KeyCode::Char('j') => self.panel.move_selection(true, rows),
            KeyCode::Up | KeyCode::Char('k') => self.panel.move_selection(false, rows),
            KeyCode::Delete | KeyCode::Backspace if self.selected_has_credentials() => {
                self.ask_to_forget_credentials()
            }
            KeyCode::Char('d') if self.panel.section == Section::Agents => self.edit_selected(),
            KeyCode::Char('e') if self.panel.section.grows() => self.edit_selected(),
            KeyCode::Char('p') if self.panel.section == Section::Remotes => {
                if let Some(RowKind::Remote { address, .. }) =
                    self.selected_row().map(|row| row.kind)
                {
                    self.push_config(&address, config_pushes);
                }
            }
            KeyCode::Char('b') if self.panel.section == Section::Remotes => {
                if let Some(RowKind::Remote { address, .. }) =
                    self.selected_row().map(|row| row.kind)
                {
                    self.begin_tracker_pairing(address, task_service);
                }
            }
            KeyCode::Char('t') if self.panel.section == Section::Remotes => {
                if let Some(RowKind::Remote {
                    address, trackers, ..
                }) = self.selected_row().map(|row| row.kind)
                {
                    self.foreground =
                        Foreground::Setup(Setup::tracker_list(address, trackers.as_deref()));
                }
            }
            KeyCode::Char('n') if self.panel.section.grows() => {
                self.panel.form = Some(match self.panel.section {
                    Section::Remotes => Form::new(Target::NewRemote, "", ""),
                    _ => Form::new(Target::NewEnvironment, "", ""),
                });
            }
            _ if is_remove(key) && self.panel.section.grows() => self.ask_to_delete(),
            KeyCode::Enter => {
                self.activate_selected(task_service, events, tracker_connects)
                    .await
            }
            _ => {}
        }
        if self.panel.section != previous_section {
            if self.panel.section == Section::VersionControl {
                self.refresh_git_identity().await;
            }
            self.credentials_visible_since = None;
            self.remotes_visible_since = None;
        }
    }

    fn begin_tracker_pairing<E>(&mut self, address: String, tasks: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(tracker) = tasks.tracker() else {
            self.notice = Some(NO_LINEAR.into());
            return;
        };
        let keys = self.remote_keys.clone();
        let link = self.link.clone();
        let replies = self.pairing_replies.clone();
        self.notice = Some("asking for a Linear pairing code…".into());
        tokio::spawn(async move {
            let result = async {
                let identity = tracker
                    .identity()
                    .await
                    .map_err(|error| error.to_string())?;
                let peer = Self::remote_peer(&address, &keys).await?;
                let code = link
                    .pair_tracker(peer, &identity.id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok((identity.label, format!("altitude pair {code}")))
            }
            .await;
            let _ = replies.send(result).await;
        });
    }

    pub(in crate::tui) fn pairing_resolved(&mut self, result: Result<(String, String), String>) {
        self.notice = Some(match result {
            Ok((label, command)) => match copy_to_clipboard(command.clone()) {
                Ok(()) => format!(
                    "Pairing command copied for {label}. Delegate an issue to Altitude and paste it in reply to its setup message (valid 10 minutes)."
                ),
                Err(_) => format!(
                    "Reply to Altitude's setup message in Linear: {command} (valid 10 minutes)"
                ),
            },
            Err(error) => format!("could not pair with the remote: {error}"),
        });
    }

    /// What `enter` does where the focus is: the row's own action, which for a
    /// remote is reaching it rather than editing the address it is reached at, for
    /// a credential is a terminal, and for a tracker is a browser sign-in, all
    /// rather than a page.
    pub(in crate::tui) async fn activate_selected<E>(
        &mut self,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        match self.selected_row().map(|row| row.kind) {
            Some(RowKind::Remote { address, .. }) => self.check_remote(&address).await,
            // A tracker is signed in to, which is a browser and a callback rather
            // than a page here.
            Some(RowKind::Tracker { .. }) => {
                let lines = if self.tracker_state == TrackerState::Connected {
                    vec![
                        Line::from("Linear is signed in."),
                        Line::from(""),
                        Line::from(
                            "To use another account, close this window and press Delete on the Linear row to remove this sign-in.",
                        ),
                    ]
                } else {
                    vec![
                        Line::from("Finish signing in in your browser."),
                        Line::from(""),
                        Line::from(
                            "Altitude uses Linear's official MCP server through mcp-remote. Node.js (npx) is required.",
                        ),
                        Line::from(""),
                        Line::from("Esc or q cancels this sign-in."),
                    ]
                };
                self.foreground = Foreground::Setup(Setup::new(SetupKind::Linear, lines));
                if self.tracker_state != TrackerState::Connected {
                    self.connect_tracker(task_service, tracker_connects);
                }
                self.notice = None;
            }
            Some(RowKind::Agent { credential, .. }) => {
                if let CredentialKind::Profile { profile, .. } = credential.kind {
                    self.open_credential_shell(&credential.label, profile, task_service, events);
                }
            }
            Some(RowKind::GitIdentity(_)) => {
                self.open_credential_shell("Git identity", "vcs/github", task_service, events);
            }
            Some(RowKind::Credential(credential)) => {
                if let CredentialKind::Profile { profile, .. } = credential.kind {
                    self.open_credential_shell(&credential.label, profile, task_service, events);
                } else {
                    self.foreground = Foreground::Setup(Setup::new(
                        SetupKind::Remote,
                        screens::settings::remote_setup_lines(),
                    ));
                }
            }
            _ => self.edit_selected(),
        }
    }

    /// The machines a task could run on: this one, then the remotes, each saying
    /// whether it can be given work.
    fn task_hosts(&self) -> Vec<Host> {
        let mut hosts = vec![Host {
            address: None,
            label: "this machine".to_owned(),
            ready: true,
        }];
        for address in self.configured_remotes() {
            let name = self
                .remembered_keys
                .iter()
                .find(|key| key.address.eq_ignore_ascii_case(&address))
                .map_or_else(|| address.clone(), |key| key.label.clone());
            // A remote that answers but does not hold this configuration cannot
            // build the environment a task names, so it is not a machine that can
            // be given work yet. Saying why points at the settings that fix it.
            let (label, ready) = match self.remote_status.get(&address) {
                Some(RemoteStatus::Reached) if self.remote_needs_config(&address) => {
                    (format!("{name} (needs config)"), false)
                }
                Some(RemoteStatus::Reached) => (name, true),
                Some(RemoteStatus::Syncing) => (format!("{name} (syncing)"), false),
                Some(RemoteStatus::Asking) => (format!("{name} (asking)"), false),
                Some(RemoteStatus::Changed) => (format!("{name} (key changed)"), false),
                Some(RemoteStatus::Unknown) | Some(RemoteStatus::Silent(_)) | None => {
                    (format!("{name} (unreachable)"), false)
                }
            };
            hosts.push(Host {
                address: Some(address),
                label,
                ready,
            });
        }
        hosts
    }

    /// The machines a task that is already here could be sent to: this machine is
    /// where it already is, so it is not one of them.
    fn send_hosts(&self) -> Vec<Host> {
        self.task_hosts()
            .into_iter()
            .filter(|host| host.address.is_some())
            .collect()
    }

    /// Put the machines the creation question and the send chooser offer back in
    /// step with what the remotes have answered, keeping the one the operator chose
    /// where it is still offered.
    pub(in crate::tui) fn refresh_task_hosts(&mut self) {
        let hosts = self.task_hosts();
        match &mut self.foreground {
            Foreground::NewTask(question) => question.refresh_hosts(hosts),
            Foreground::SendTask(sending) => {
                let remotes = hosts
                    .into_iter()
                    .filter(|host| host.address.is_some())
                    .collect();
                sending.refresh_hosts(remotes);
            }
            _ => {}
        }
    }

    /// Ask each remote that answered what it is holding, off the event loop: reading one
    /// is a dial, and a dial does not belong between two frames.
    ///
    /// A remote that does not answer contributes nothing. Its tasks are its own, and
    /// there is no copy here to fall back on.
    pub(in crate::tui) fn read_remote_tasks(&self, reads: &mpsc::UnboundedSender<Vec<HeldTask>>) {
        let addresses: Vec<String> = self
            .configured_remotes()
            .into_iter()
            .filter(|address| {
                matches!(self.remote_status.get(address), Some(RemoteStatus::Reached))
            })
            .collect();
        if addresses.is_empty() {
            return;
        }
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let reads = reads.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            for address in addresses {
                match Self::read_tasks(&address, &keys, &client).await {
                    Ok(tasks) => held.extend(tasks.into_iter().map(|summary| HeldTask {
                        address: address.clone(),
                        summary,
                    })),
                    Err(error) => {
                        tracing::debug!(%error, address, "could not read a remote's tasks")
                    }
                }
            }
            let _ = reads.send(held);
        });
    }

    /// Bring a task home from the machine that owns it.
    ///
    /// The remote stops running it, its capsule travels, and it is founded here with the
    /// history of what was decided about it — and only then is the remote told it is no
    /// longer the owner. Everything after that is the ordinary take-over of a task that
    /// is here, because by then it is.
    fn ask_for_task<E>(
        &mut self,
        held: &HeldTask,
        event_loop: &TaskEventLoop<E>,
        landings: &mpsc::UnboundedSender<Result<String, String>>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let address = held.address.clone();
        let summary = held.summary.clone();
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let event_loop = event_loop.clone();
        let landings = landings.clone();
        self.notice = Some(format!("asking {address} for the task…"));
        tokio::spawn(async move {
            let outcome = bring_home(&address, &summary, &keys, &client, &event_loop).await;
            // A take-over runs away from the interface, so where it stopped is a log
            // line rather than only a notice that has already been replaced.
            match &outcome {
                Ok(id) => tracing::info!(task = %id, address, "brought a task home"),
                Err(error) => tracing::warn!(%error, address, "could not bring a task home"),
            }
            let _ = landings.send(outcome);
        });
    }

    /// Ask a remote to run a task of its own, off the event loop.
    ///
    /// Nothing is copied and nothing is kept here: the remote founds the task, and it
    /// is the only machine that can say what the task is doing. The notice that it
    /// took the task arrives when the remote says so, not before.
    pub(in crate::tui) fn begin_delegation(
        &mut self,
        address: &str,
        origin: TaskOrigin,
        environment: &str,
        agent: AgentKind,
        replies: &mpsc::Sender<TaskDelegationReply>,
    ) {
        let identity = match self.config.identity() {
            Ok(identity) => identity,
            Err(error) => {
                self.notice = Some(error.to_string());
                return;
            }
        };
        let address = address.to_owned();
        let environment = environment.to_owned();
        self.notice = Some(format!("sending the task to {address}…"));
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result = Self::send_task(
                &address,
                origin,
                &environment,
                agent,
                identity.label,
                &keys,
                &client,
            )
            .await;
            let _ = replies.send(TaskDelegationReply { address, result }).await;
        });
    }

    /// The dial, which is what makes this delegation rather than a preference.
    async fn send_task(
        address: &str,
        origin: TaskOrigin,
        environment: &str,
        agent: AgentKind,
        identity_label: String,
        keys: &RemoteKeys,
        client: &LinkClient,
    ) -> Result<(), String> {
        let remembered = keys.remembered(address).await.ok().flatten();
        let sighting = meet(address, DEFAULT_PORT, remembered.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        if sighting.key_status == RemoteKeyStatus::Changed {
            return Err(format!(
                "{address} answered with a different key; approve its identity first"
            ));
        }
        // Whose work this is travels as a label. The machine it came from is the
        // key the handshake proved, so the id is not the client's to send.
        client
            .run_task(
                sighting.ticket,
                TaskRequest {
                    origin,
                    environment: environment.to_owned(),
                    agent,
                    identity_label,
                },
            )
            .await
            .map_err(|error| error.to_string())
    }

    /// Take what a remote answered when it was asked to run a task.
    pub(in crate::tui) fn task_delegation_resolved(
        &mut self,
        reply: TaskDelegationReply,
        reads: &mpsc::UnboundedSender<Vec<HeldTask>>,
    ) {
        self.notice = Some(match reply.result {
            Ok(()) => format!("{} took the task", reply.address),
            Err(error) => format!("{} did not take it: {error}", reply.address),
        });
        // The machine holds a task this client did not have, so its cards are read
        // again rather than guessed at.
        self.read_remote_tasks(reads);
    }

    /// Send a task that is already here to a machine that will continue it, off
    /// the event loop.
    ///
    /// The task stops here, its capsule travels, the remote boots it, and only
    /// then does this machine relinquish. Nothing is reported as done until the
    /// remote has acknowledged it.
    pub(in crate::tui) fn begin_send_to_remote<E>(
        &mut self,
        address: String,
        task_id: String,
        event_loop: &TaskEventLoop<E>,
        replies: &mpsc::Sender<SendTaskReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        self.notice = Some(format!("sending the task to {address}…"));
        let config = self.config.clone();
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let event_loop = event_loop.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result =
                send_to_remote(&address, &task_id, &config, &keys, &client, &event_loop).await;
            match &result {
                Ok(id) => tracing::info!(task = %id, %address, "sent a task to a remote"),
                Err(error) => tracing::warn!(%error, %address, "could not send a task"),
            }
            let _ = replies.send(SendTaskReply { address, result }).await;
        });
    }

    /// Take what a remote answered when an existing task was sent to it.
    ///
    /// The source stays in the list until it is really relinquished, so a failure
    /// leaves the stopped task visible rather than pretending it moved.
    pub(in crate::tui) fn send_task_resolved(
        &mut self,
        reply: SendTaskReply,
        reads: &mpsc::UnboundedSender<Vec<HeldTask>>,
    ) {
        match reply.result {
            Ok(task_id) => {
                self.notice = Some(format!("{} took the task", reply.address));
                // It just answered, so it is reachable; recording that lets the
                // read below dial it rather than skip an address the panel had
                // never checked.
                self.remote_status
                    .insert(reply.address.clone(), RemoteStatus::Reached);
                self.pending_remote = Some((reply.address, task_id));
            }
            Err(error) => {
                // Not "did not take it": the remote may hold the task while the
                // final acknowledgement was lost, in which case a restart finishes
                // the transfer. What is true here is that it did not finish.
                self.notice = Some(format!(
                    "sending to {} did not finish: {error}",
                    reply.address
                ));
            }
        }
        // The machine holds a task this client may not have, so its cards are read
        // again rather than guessed at.
        self.read_remote_tasks(reads);
    }

    /// Take a fresh read of what the remotes hold, and put the highlight on a task
    /// that has just been sent when it appears.
    pub(in crate::tui) fn remote_tasks_read(&mut self, tasks: Vec<HeldTask>) {
        self.remote_tasks = tasks;
        if let Some((address, task_id)) = self.pending_remote.take()
            && let Some(index) = self
                .remote_tasks
                .iter()
                .position(|held| held.address == address && held.summary.id == task_id)
        {
            self.selected = self.tasks.len() + index;
        }
    }

    /// The keys of the send chooser: choose a machine, or leave without sending.
    pub(in crate::tui) async fn send_task_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
        replies: &mpsc::Sender<SendTaskReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::SendTask(mut sending) = mem::take(&mut self.foreground) else {
            return;
        };
        match sending.handle_key(key) {
            None => self.foreground = Foreground::SendTask(sending),
            Some(SendDecision::Cancel) => {}
            Some(SendDecision::Settings) => {
                open_panel(self, task_service).await;
                self.panel.section = Section::Remotes;
                self.panel.selected = 0;
            }
            Some(SendDecision::Send { address }) => {
                self.begin_send_to_remote(
                    address,
                    sending.task_id().to_owned(),
                    event_loop,
                    replies,
                );
            }
        }
    }

    /// The held task the selection is on, when it is on one.
    ///
    /// The list shows them after this machine's own, so the index says which it is.
    fn selected_held(&self) -> Option<&HeldTask> {
        self.remote_tasks
            .get(self.selected.checked_sub(self.tasks.len())?)
    }

    /// Whether the selection is a task another machine owns.
    fn selected_is_remote(&self) -> bool {
        self.selected >= self.tasks.len()
            && self.selected < self.tasks.len() + self.remote_tasks.len()
    }

    /// Whether a remote holds the configuration this client has.
    pub(in crate::tui) fn remote_needs_config(&self, address: &str) -> bool {
        self.remote_synced
            .get(address)
            .is_none_or(|sent| *sent < self.config.stamp())
    }

    /// Send this installation's configuration to a remote, so it can build the
    /// environments this client runs tasks in.
    ///
    /// The dial is a network round trip and a bundle read, so it runs away from
    /// the interface and reports back. What is written down when it answers is the
    /// stamp the bundle was built from, not whatever the file says by then.
    pub(in crate::tui) fn push_config(
        &mut self,
        address: &str,
        replies: &mpsc::Sender<ConfigPushReply>,
    ) {
        self.notice = Some(format!("sending the configuration to {address}…"));
        self.remote_status
            .insert(address.to_owned(), RemoteStatus::Syncing);
        let stamp = self.config.stamp();
        let settings = match fs::read_to_string(self.config.settings_path()) {
            Ok(settings) => settings,
            Err(error) => {
                self.notice = Some(format!("could not read the configuration: {error}"));
                self.remote_status.remove(address);
                return;
            }
        };
        let address = address.to_owned();
        let data_dir = self.config.data_dir().to_path_buf();
        // Sending a configuration string again costs nothing; sending a directory
        // of logins again does, so the ones the remote already holds are left out.
        let profiles: Vec<&'static str> = self.profiles.iter().map(|held| held.profile).collect();
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result =
                deliver_config(&address, settings, data_dir, profiles, &keys, &client).await;
            let _ = replies
                .send(ConfigPushReply {
                    address,
                    stamp,
                    result,
                })
                .await;
        });
    }

    /// Take what came back from pushing a configuration. Only a real
    /// acknowledgement records the stamp; anything else leaves the settings alone
    /// and says what happened.
    pub(in crate::tui) fn config_push_resolved(&mut self, reply: ConfigPushReply) {
        match reply.result {
            Ok(()) => {
                self.remote_synced
                    .insert(reply.address.clone(), reply.stamp);
                self.notice = Some(format!("{} holds this configuration now", reply.address));
            }
            Err(error) => {
                self.notice = Some(format!("{} did not take it: {error}", reply.address));
            }
        }
        // Asked again either way, so the dot says what is true now rather than what
        // was hoped for.
        self.remote_status.remove(&reply.address);
        // A creation question may be open over this list; what it can offer has
        // just changed.
        self.refresh_task_hosts();
    }

    /// Note whether the remotes list is showing, and start its clock when it
    /// appears.
    pub(in crate::tui) fn sync_remotes(&mut self) {
        let showing = (matches!(self.foreground, Foreground::Panel)
            && self.panel.section == Section::Remotes)
            || matches!(self.foreground, Foreground::Tasks)
            || matches!(self.foreground, Foreground::NewTask(_))
            || matches!(self.foreground, Foreground::SendTask(_));
        match (showing, self.remotes_visible_since) {
            // A fresh look every time the list is opened, so what it says is about
            // now rather than about whenever it was last visited.
            (true, None) => {
                self.remotes_visible_since = Some(Instant::now());
                self.remote_status.clear();
                self.remote_trackers.clear();
            }
            (false, Some(_)) => self.remotes_visible_since = None,
            _ => {}
        }
    }

    /// Note whether the credentials list is showing, and start its clock when it
    /// appears.
    ///
    /// A fresh look every time it is opened, so a login made in a shell a moment ago
    /// is not still being reported as it was.
    pub(in crate::tui) fn sync_credentials(&mut self) {
        let showing = matches!(self.foreground, Foreground::Panel)
            && matches!(
                self.panel.section,
                Section::Agents | Section::VersionControl
            );
        match (showing, self.credentials_visible_since) {
            (true, None) => {
                self.credentials_visible_since = Some(Instant::now());
                self.credential_check_generation = self.credential_check_generation.wrapping_add(1);
                self.credential_status.clear();
            }
            (false, Some(_)) => self.credentials_visible_since = None,
            _ => {}
        }
    }

    /// Whether any profile is being asked about right now, which is what keeps the
    /// dot moving while it waits.
    pub(in crate::tui) fn is_asking_credentials(&self) -> bool {
        self.credential_status
            .values()
            .any(CredentialStatus::is_in_progress)
    }

    /// Whether it is time to ask about another profile: the list is showing, it has
    /// been showing long enough to mean it, and nothing is in flight.
    pub(in crate::tui) fn wants_credential_check(&self) -> bool {
        let Some(since) = self.credentials_visible_since else {
            return false;
        };
        since.elapsed() >= ASK_AFTER
            && !self.is_asking_credentials()
            && self.next_unasked_profile().is_some()
    }

    /// The first profile whose login has not been asked about.
    fn next_unasked_profile(&self) -> Option<&'static str> {
        self.profiles
            .iter()
            .map(|profile| profile.profile)
            .filter(|profile| match self.panel.section {
                Section::Agents => profile.starts_with("agents/"),
                Section::VersionControl => profile.starts_with("vcs/"),
                _ => false,
            })
            .find(|profile| !self.credential_status.contains_key(profile))
    }

    /// Ask a profile's own command about its login, and let it report back when it
    /// answers. Off the loop: the answer comes from a sandbox.
    pub(in crate::tui) fn ask_next_credential<E>(
        &mut self,
        task_service: &TaskService<E>,
        reports: &mpsc::UnboundedSender<CredentialCheckReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(profile) = self.next_unasked_profile() else {
            return;
        };
        self.credential_status
            .insert(profile, CredentialStatus::Checking);
        let environment = self.default_environment_value();
        let chosen = self.agent();
        let service = task_service.clone();
        let reports = reports.clone();
        let generation = self.credential_check_generation;
        tokio::spawn(async move {
            let status = match service
                .credential_report(environment, profile, chosen)
                .await
            {
                Ok(Some(report)) => CredentialStatus::Checked {
                    signed_in: report.signed_in,
                    output: report.output,
                },
                Ok(None) => CredentialStatus::Checked {
                    signed_in: false,
                    output: String::new(),
                },
                Err(error) => {
                    tracing::debug!(%error, profile, "credential check unavailable");
                    CredentialStatus::Checked {
                        signed_in: false,
                        output: String::new(),
                    }
                }
            };
            let _ = reports.send(CredentialCheckReply {
                profile,
                status,
                generation,
            });
        });
    }

    pub(in crate::tui) fn credential_checked(&mut self, reply: CredentialCheckReply) {
        if reply.generation == self.credential_check_generation {
            self.credential_status.insert(reply.profile, reply.status);
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
                        self.note_remote_trackers(&address, &announcement);
                        // It answered, but a machine still building what it needs
                        // cannot be given work, and that is worth knowing before you
                        // delegate to it.
                        match announcement.state {
                            RemoteState::Preparing => RemoteStatus::Syncing,
                            RemoteState::Ready => RemoteStatus::Reached,
                        }
                    }
                    // Reported, and not written: accepting a changed key means
                    // forgetting the address first, which is a second act. Nothing
                    // the unaccepted answer said is shown either.
                    RemoteKeyStatus::Changed => {
                        self.clear_remote_trackers(&address);
                        RemoteStatus::Changed
                    }
                };
                self.remote_status.insert(address, status);
                self.reload_remembered_keys().await;
            }
            RemoteAnswer::Silent { address, reason } => {
                self.clear_remote_trackers(&address);
                self.remote_status
                    .insert(address, RemoteStatus::Silent(reason));
            }
        }
        // A creation question may be open over the list, and a remote that just
        // answered may now be a machine it can offer.
        self.refresh_task_hosts();
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
                        self.note_remote_trackers(address, said);
                        Some(format!(
                            "{} answered, and its key is remembered",
                            said.label
                        ))
                    }
                    RemoteKeyStatus::Known => {
                        self.remember(address, said).await;
                        self.note_remote_trackers(address, said);
                        Some(format!(
                            "{} answered with the same key as before",
                            said.label
                        ))
                    }
                    RemoteKeyStatus::Changed => {
                        self.clear_remote_trackers(address);
                        Some(format!(
                            "{address} answered with a different key. d forgets the one you have, \
                             then enter accepts this one."
                        ))
                    }
                }
            }
            Err(error) => {
                self.clear_remote_trackers(address);
                Some(format!("{address} did not answer: {error}"))
            }
        };
        self.reload_remembered_keys().await;
    }

    /// Note what an announcement said about the trackers a remote runs. Called
    /// only for an answer whose key is accepted, so a changed key never lends its
    /// announcement any authority.
    fn note_remote_trackers(&mut self, address: &str, announcement: &Announcement) {
        self.remote_trackers
            .insert(address.to_owned(), announcement.trackers.clone());
        if let Foreground::Setup(setup) = &mut self.foreground {
            setup.refresh_trackers(address, Some(&announcement.trackers));
        }
    }

    fn clear_remote_trackers(&mut self, address: &str) {
        self.remote_trackers.remove(address);
        if let Foreground::Setup(setup) = &mut self.foreground {
            setup.refresh_trackers(address, None);
        }
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
            // The installation's key is made, not signed in to, so there is no
            // terminal to open for it.
            RowKind::GitIdentity(_) => {
                self.notice = Some("press enter to configure Git identity and signing".to_owned());
                return;
            }
            RowKind::Credential(_) => {
                self.notice = Some("the key names this machine and is not edited here".to_owned());
                return;
            }
            // A tracker is signed in to with enter, which starts a browser
            // sign-in rather than a form here.
            RowKind::Tracker { .. } => {
                self.notice = Some("press enter to sign in to Linear".to_owned());
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

    fn selected_has_credentials(&self) -> bool {
        self.panel.depth != Depth::Rail
            && matches!(
                self.selected_row().map(|row| row.kind),
                Some(RowKind::Agent { .. } | RowKind::Tracker { .. })
                    | Some(RowKind::Credential(screens::settings::Credential {
                        kind: CredentialKind::Profile { .. },
                        ..
                    }))
            )
    }

    fn ask_to_forget_credentials(&mut self) {
        let (target, question) = match self.selected_row().map(|row| row.kind) {
            Some(RowKind::Agent { credential, .. }) | Some(RowKind::Credential(credential)) => {
                let CredentialKind::Profile { profile, .. } = credential.kind else { return; };
                let scope = if profile == "agents/pi" { "all Pi provider sign-ins" } else if profile == "vcs/github" { "the GitHub CLI sign-in" } else { "the Claude Code sign-in" };
                (CredentialTarget::Profile(profile), format!("Delete {scope} saved on this machine? Tool settings and copies already sent to remotes are kept."))
            }
            Some(RowKind::Tracker { .. }) => (CredentialTarget::Linear, "Delete this machine's personal Linear sign-in? The remote's shared Linear app is kept.".to_owned()),
            _ => return,
        };
        self.foreground =
            Foreground::Confirm(PendingConfirm::forgetting_credentials(target, question));
    }

    pub(in crate::tui) async fn forget_credentials<E>(
        &mut self,
        target: CredentialTarget,
        service: &TaskService<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let result = match target {
            CredentialTarget::Profile(profile) => service
                .forget_credentials(profile)
                .await
                .map_err(|error| error.to_string()),
            CredentialTarget::Linear => {
                if let Some(handle) = self.tracker_connect.take() {
                    handle.abort();
                }
                self.tracker_connect_attempt = self.tracker_connect_attempt.wrapping_add(1);
                match service.tracker() {
                    Some(tracker) => tracker
                        .clear_credentials()
                        .await
                        .map_err(|error| error.to_string()),
                    None => Err("Linear is unavailable".to_owned()),
                }
            }
        };
        self.foreground = Foreground::Panel;
        match result {
            Ok(()) => {
                self.credential_check_generation = self.credential_check_generation.wrapping_add(1);
                self.credential_status.clear();
                self.credentials_visible_since = None;
                self.tracker_state = match service.tracker() {
                    Some(tracker) if tracker.is_connected() => TrackerState::Connected,
                    _ => TrackerState::NotConnected,
                };
                self.notice = Some("Saved sign-in deleted".to_owned());
            }
            Err(error) => {
                self.notice = Some(format!("Could not delete the saved sign-in: {error}"))
            }
        }
    }

    pub(in crate::tui) fn ask_to_delete(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        // The question is built first: it borrows the name the target then moves.
        let (question, target) = match row.kind {
            RowKind::Base
            | RowKind::Switch { .. }
            | RowKind::Agent { .. }
            | RowKind::GitIdentity(_)
            | RowKind::Credential(_)
            | RowKind::Tracker { .. } => {
                return;
            }
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

    pub(in crate::tui) async fn panel_form_key(
        &mut self,
        key: KeyEvent,
        config_pushes: &mpsc::Sender<ConfigPushReply>,
    ) {
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
                self.push_config(&address, config_pushes);
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
            self.selected = self
                .selected
                .min((self.tasks.len() + self.remote_tasks.len()).saturating_sub(1));
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
        tracing::info!(shell = ?self.open_shell(), "detaching terminal");
        self.foreground = Foreground::Tasks;
    }

    pub(in crate::tui) fn remove_shell(&mut self, task_id: &str) {
        let id = ShellId::Task(task_id.to_owned());
        self.shells.remove(&id);
        if self.open_shell() == Some(&id) {
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
    pub(in crate::tui) fn setup_key(&mut self, key: KeyEvent) {
        if is_dismiss(key) {
            if matches!(&self.foreground, Foreground::Setup(setup) if matches!(setup.kind, SetupKind::Linear))
                && let Some(handle) = self.tracker_connect.take()
            {
                handle.abort();
                self.tracker_connect_attempt = self.tracker_connect_attempt.wrapping_add(1);
                self.tracker_state = TrackerState::NotConnected;
            }
            // A provider's guide steps back to the list it came from; everything
            // else closes to the panel.
            let stays = match &mut self.foreground {
                Foreground::Setup(setup) => setup.back(),
                _ => false,
            };
            if !stays {
                self.foreground = Foreground::Panel;
                self.notice = None;
            }
        } else if let Foreground::Setup(setup) = &mut self.foreground {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') if setup.is_picker() => {
                    setup.move_selection(true)
                }
                KeyCode::Up | KeyCode::Char('k') if setup.is_picker() => {
                    setup.move_selection(false)
                }
                KeyCode::Enter if setup.is_picker() => setup.open_selected(),
                _ => setup.scroll(key),
            }
        }
    }

    pub(in crate::tui) fn help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | '?') => self.dismiss_keys(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = self.help_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(12),
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(12),
            KeyCode::Home => self.help_scroll = 0,
            KeyCode::End => self.help_scroll = u16::MAX,
            _ => {}
        }
    }

    pub(in crate::tui) fn dismiss_keys(&mut self) {
        if let Foreground::Keys(behind) = mem::take(&mut self.foreground) {
            self.foreground = *behind;
        }
    }

    /// Take the answer to a reply, and act on it.
    pub(in crate::tui) fn reply_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
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
                if self.remote_signals.is_some() {
                    self.act_on_remote_signal(
                        answering,
                        RemoteSignalAction::Answer,
                        Some(command),
                        remote_signals,
                    );
                } else {
                    event_loop.answer_operator_signal(answering, command);
                }
                if !was_last {
                    self.foreground = Foreground::Signals;
                }
            }
        }
    }

    /// Send a key to the terminal in front, or leave it.
    ///
    /// Leaving means different things: a task's shell goes back to autopilot, and a
    /// credential shell is thrown away with its sandbox.
    pub(in crate::tui) async fn shell_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(id) = self.open_shell().cloned() else {
            return;
        };
        if screens::shell::is_exit_key(key) {
            match &id {
                ShellId::Task(task_id) => {
                    if !event_loop.return_to_autopilot(task_id) {
                        self.notice = Some(
                            "this task is no longer running; control stayed with autopilot".into(),
                        );
                    }
                    self.hide_shell();
                }
                ShellId::Credentials(_) => self.close_credential_shell(task_service).await,
            }
            return;
        }
        let bytes = self
            .shells
            .get_mut(&id)
            .and_then(|shell| shell.key_bytes(key));
        if let Some(bytes) = bytes {
            self.send_shell_input(&id, bytes, event_loop);
        }
    }

    /// Take the answer to the creation question, and act on it.
    pub(in crate::tui) async fn new_task_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
        delegations: &mpsc::Sender<TaskDelegationReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::NewTask(mut question) = mem::take(&mut self.foreground) else {
            return;
        };
        match question.handle_key(key) {
            None => self.foreground = Foreground::NewTask(question),
            // The question is dropped, and with it any read that had not yet
            // founded a task.
            Some(Answer::Cancel) => {}
            Some(Answer::Create {
                task,
                environment,
                agent,
                host,
                remember,
                reopen,
            }) => {
                // The first answer becomes the default, because there was nothing
                // to default to and guessing is not Altitude's to do.
                if let Some(agent) = remember {
                    self.set_agent(agent);
                }
                match host {
                    // A task for a remote is founded there, off this loop. Nothing is
                    // copied: the machine that runs it is the machine that owns it.
                    Some(address) => self.begin_delegation(
                        &address,
                        TaskOrigin::Intent(task),
                        &environment.name,
                        agent,
                        delegations,
                    ),
                    // Founded here, so whose work it is needs no saying.
                    None => event_loop.create(task, environment, agent, None),
                }
                if reopen {
                    question.reset_for_next();
                    self.foreground = Foreground::NewTask(question);
                }
            }
            // The prompt stays in front, reading the issue: the operator sees the
            // wait, and can still dismiss it.
            Some(Answer::ReadIssue { token }) => {
                self.foreground = Foreground::NewTask(question);
                self.start_issue_read(token, task_service, issue_reads);
            }
        }
    }

    /// Where Linear keeps its sign-in profile. Shown in settings; the interface
    /// never reads its contents, and it is never mounted into a task.
    pub(in crate::tui) fn tracker_profile_dir(&self) -> PathBuf {
        self.config.data_dir().join("trackers").join("linear")
    }

    /// The search the issue page owes, if its query has rested long enough.
    pub(in crate::tui) fn due_issue_search(&mut self) -> Option<(SearchToken, String)> {
        let Foreground::NewTask(question) = &mut self.foreground else {
            return None;
        };
        question.due_search(Instant::now())
    }

    /// Tell the issue page why it cannot search, with the message the caller
    /// chose: no session is the ordinary case, no provider is the build's.
    pub(in crate::tui) fn issue_search_unavailable(
        &mut self,
        token: SearchToken,
        message: &'static str,
    ) {
        if let Foreground::NewTask(question) = &mut self.foreground {
            question.search_unavailable(token, message.to_owned());
        }
    }

    /// Take a search answer, if the page is still asking that question. A
    /// submission queued behind the search starts its read here, once.
    pub(in crate::tui) fn issue_search_resolved<E>(
        &mut self,
        reply: IssueSearchReply,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let token = {
            let Foreground::NewTask(question) = &mut self.foreground else {
                return;
            };
            question.apply_search(reply.token, reply.result)
        };
        if let Some(token) = token {
            self.start_issue_read(token, task_service, issue_reads);
        }
    }

    /// Read the chosen issue off the loop, so the interface keeps drawing while
    /// Linear is asked.
    fn start_issue_read<E>(
        &mut self,
        token: u64,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(reference) = self.issue_submission_reference(token) else {
            return;
        };
        let Some(tracker) = task_service.tracker() else {
            if let Foreground::NewTask(question) = &mut self.foreground {
                question.read_failed(token, NO_LINEAR.to_owned());
            }
            return;
        };
        let replies = issue_reads.clone();
        tokio::spawn(async move {
            let result = tracker
                .read(&reference)
                .await
                .map_err(|error| error.to_string());
            let _ = replies.send(IssueReadReply { token, result });
        });
    }

    fn issue_submission_reference(&self, token: u64) -> Option<IssueReference> {
        match &self.foreground {
            Foreground::NewTask(question) => question.submission_reference(token),
            _ => None,
        }
    }

    /// Take a snapshot that has arrived, if the submission that asked for it is
    /// still the one in front of the operator. This is the gate between the read
    /// and founding a task: a reply whose token no longer matches creates nothing.
    ///
    /// A bound issue runs here or on the chosen machine. For a remote, the whole
    /// snapshot travels as the task's origin, so the machine reads the issue itself
    /// rather than founding a task from a plain prompt.
    pub(in crate::tui) async fn issue_read_resolved<E>(
        &mut self,
        reply: IssueReadReply,
        event_loop: &TaskEventLoop<E>,
        delegations: &mpsc::Sender<TaskDelegationReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        match reply.result {
            Ok(snapshot) => {
                let submission = {
                    let Foreground::NewTask(question) = &mut self.foreground else {
                        return;
                    };
                    question.take_submission(reply.token)
                };
                let Some(submission) = submission else {
                    return;
                };
                if let Some(agent) = submission.remember {
                    self.set_agent(agent);
                }
                match submission.host {
                    Some(address) => self.begin_delegation(
                        &address,
                        TaskOrigin::Issue(Box::new(snapshot)),
                        &submission.environment.name,
                        submission.agent,
                        delegations,
                    ),
                    None => {
                        event_loop.create_from_issue(
                            snapshot,
                            submission.environment,
                            submission.agent,
                        );
                    }
                }
                if submission.reopen {
                    if let Foreground::NewTask(question) = &mut self.foreground {
                        question.reset_for_next();
                    }
                } else {
                    self.foreground = Foreground::Tasks;
                }
            }
            Err(error) => {
                if let Foreground::NewTask(question) = &mut self.foreground {
                    question.read_failed(reply.token, error);
                }
            }
        }
    }

    /// Start signing in to Linear, or give up on a sign-in already under way.
    ///
    /// The browser and the OAuth callback are the provider's; this asks for a
    /// connection and keeps the one handle, so the attempt can be cancelled here
    /// or taken down with the interface.
    pub(in crate::tui) fn connect_tracker<E>(
        &mut self,
        task_service: &TaskService<E>,
        replies: &mpsc::UnboundedSender<TrackerConnectReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if let Some(handle) = self.tracker_connect.take() {
            handle.abort();
            self.tracker_connect_attempt = self.tracker_connect_attempt.wrapping_add(1);
            self.tracker_state = TrackerState::NotConnected;
            self.clear_sign_in_notice();
            self.notice = Some("Linear sign-in cancelled".to_owned());
            return;
        }
        let Some(tracker) = task_service.tracker() else {
            self.notice = Some("Linear is not available in this build".to_owned());
            return;
        };
        if tracker.is_connected() {
            self.tracker_state = TrackerState::Connected;
            self.notice = Some("Linear is already connected".to_owned());
            return;
        }
        self.tracker_state = TrackerState::Connecting;
        self.tracker_connect_attempt = self.tracker_connect_attempt.wrapping_add(1);
        let attempt = self.tracker_connect_attempt;
        self.notice = Some(SIGN_IN_NOTICE.to_owned());
        let replies = replies.clone();
        self.tracker_connect = Some(tokio::spawn(async move {
            let result = match tracker.connect().await {
                Ok(()) => Ok(tracker.is_connected()),
                Err(error) => Err(error.to_string()),
            };
            let _ = replies.send(TrackerConnectReply { attempt, result });
        }));
    }

    /// Take what a sign-in ended in, and update the row without a restart.
    ///
    /// Only this attempt's own notice is cleared: an unrelated error that arrived
    /// while the browser was open is not this reply's to erase.
    pub(in crate::tui) fn tracker_connect_resolved(&mut self, reply: TrackerConnectReply) {
        // A reply that lands after this attempt was given up on is not the
        // attempt the operator is watching, so it changes nothing.
        if self.tracker_state != TrackerState::Connecting
            || reply.attempt != self.tracker_connect_attempt
        {
            return;
        }
        self.tracker_connect = None;
        let setup_open = matches!(&self.foreground, Foreground::Setup(setup) if matches!(setup.kind, SetupKind::Linear));
        if setup_open {
            self.foreground = Foreground::Panel;
        }
        // The result replaces the sign-in notice it set, and nothing else: an
        // unrelated error that arrived while the browser was open stays put.
        let sign_in = self.clear_sign_in_notice() || setup_open;
        match reply.result {
            Ok(true) => {
                self.tracker_state = TrackerState::Connected;
                if sign_in {
                    self.notice = Some("Linear is connected".to_owned());
                }
            }
            // The provider answered but reports no live session, which is not a
            // connection whatever else it may have kept.
            Ok(false) => {
                self.tracker_state = TrackerState::NotConnected;
                if sign_in {
                    self.notice = Some("Linear did not report a session".to_owned());
                }
            }
            Err(error) => {
                self.tracker_state = TrackerState::NotConnected;
                if sign_in {
                    self.notice = Some(format!("Linear sign-in did not finish: {error}"));
                }
            }
        }
    }

    /// Whether a sign-in is out, which is what keeps its dot moving.
    pub(in crate::tui) fn is_connecting_tracker(&self) -> bool {
        self.tracker_state == TrackerState::Connecting
    }

    /// Drop the sign-in notice, and only it, saying whether it was the one that
    /// was showing: anything else in the band belongs to another action and is
    /// not this reply's to clear.
    fn clear_sign_in_notice(&mut self) -> bool {
        if self.notice.as_deref() == Some(SIGN_IN_NOTICE) {
            self.notice = None;
            true
        } else {
            false
        }
    }

    /// Whether the issue page is waiting on a search or a read, which is what
    /// keeps its shimmer moving.
    pub(in crate::tui) fn is_awaiting_tracker(&self) -> bool {
        match &self.foreground {
            Foreground::NewTask(question) => question.is_awaiting(),
            _ => false,
        }
    }

    /// The panel's keys, which are its form's when a form is open.
    pub(in crate::tui) async fn panel_keys<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
        config_pushes: &mpsc::Sender<ConfigPushReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if matches!(self.credential_shell, CredentialShellState::Opening { .. }) {
            if is_dismiss(key) {
                self.close_credential_shell(task_service).await;
            }
            return;
        }
        if self.panel.form.is_some() {
            self.panel_form_key(key, config_pushes).await;
        } else {
            self.panel_key(key, task_service, events, tracker_connects, config_pushes)
                .await;
        }
    }

    /// The keys of the signals pop-up.
    pub(in crate::tui) async fn signals_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
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
            _ if is_dismiss(key) || key.code == KeyCode::Char('i') => self.leave_signals(),
            KeyCode::Tab => self.focus_next_signal_action(),
            KeyCode::BackTab => self.focus_previous_signal_action(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_operator_signal(),
            KeyCode::Up | KeyCode::Char('k' | 'h') => self.select_previous_operator_signal(),
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
                let remote = self.remote_signals.is_some();
                match action {
                    popups::signals::SignalAction::Reply => {
                        // A remote task is not in this list, so it is not looked
                        // for there; its reply travels to where it is.
                        if remote || self.tasks.iter().any(|task| task.id == signal.task_id) {
                            self.foreground = Foreground::Reply(Reply {
                                prompt: Prompt::new("answer operator signal"),
                                answering: signal.id,
                            });
                        } else {
                            self.notice = Some("this task is no longer in the list".into());
                        }
                    }
                    popups::signals::SignalAction::Acknowledge => {
                        if remote {
                            self.act_on_remote_signal(
                                signal.id,
                                RemoteSignalAction::Acknowledge,
                                None,
                                remote_signals,
                            );
                        } else {
                            match task_service.acknowledge_operator_signal(&signal.id).await {
                                Ok(task) => {
                                    self.replace_task(task);
                                    self.resolved_operator_signal(self.selected_operator_signal);
                                }
                                Err(error) => self.notice = Some(error.to_string()),
                            }
                        }
                    }
                    popups::signals::SignalAction::Confirm => {
                        if remote {
                            self.act_on_remote_signal(
                                signal.id,
                                RemoteSignalAction::Confirm,
                                None,
                                remote_signals,
                            );
                        } else {
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
            }
            _ => {}
        }
    }

    /// The keys of the task list.
    pub(in crate::tui) async fn list_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) -> io::Result<bool>
    where
        E: IsolationEngine + Clone + 'static,
    {
        // A task on another machine belongs to that machine. `o` asks for it, which is a
        // transfer rather than a keystroke, and removing is this machine's act on its
        // own capsule. Its signals are read and answered where it is, over the link.
        if self.selected_is_remote() {
            if key.code == KeyCode::Char('o')
                && let Some(held) = self.selected_held()
            {
                self.foreground = Foreground::Confirm(PendingConfirm::taking_over(
                    held.summary.id.clone(),
                    format!(
                        "Take over \"{}\" from {}? It stops running there, its capsule \
                         travels here, and it becomes this machine's.",
                        held.summary
                            .title
                            .as_deref()
                            .unwrap_or(&held.summary.intent),
                        held.address
                    ),
                ));
                return Ok(false);
            }
            // The same pop-up this machine's tasks use, over a task whose actions
            // travel to the machine that owns it.
            if key.code == KeyCode::Enter || key.code == KeyCode::Char('i') {
                self.open_signals(task_service, remote_signals).await;
                return Ok(false);
            }
            // The issue a task is bound to is a page, and it opens the same way
            // whether the task is here or on another machine: through this
            // machine's browser, by the operator's own act.
            if key.code == KeyCode::Char('b') {
                let issue = self
                    .selected_held()
                    .and_then(|held| held.summary.tracker_issue.clone());
                if let Some(issue) = issue {
                    match open_in_browser(&issue.url) {
                        Ok(()) => tracing::info!(%issue.url, "opened a remote task's issue"),
                        Err(error) => {
                            self.notice = Some(format!("could not open {}: {error}", issue.url))
                        }
                    }
                }
                return Ok(false);
            }
            if is_remove(key) {
                self.notice = Some("that task is on another machine".to_owned());
                return Ok(false);
            }
        }

        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Char('c') => {
                if self.reload_settings() {
                    self.foreground = Foreground::NewTask(Box::new(NewTask::new(
                        self.environments(),
                        self.default_environment_name(),
                        self.agent(),
                        self.task_hosts(),
                    )));
                }
            }
            KeyCode::Char('s') => {
                open_panel(self, task_service).await;
            }
            KeyCode::Char('i') => self.open_signals(task_service, remote_signals).await,
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
                    let question =
                        format!("Remove \"{}\" and dispose of its capsule?", task.title());
                    self.foreground =
                        Foreground::Confirm(PendingConfirm::removing(task.id, question));
                }
            }
            // Enter reads what the task is asking. Taking over has a key of its
            // own, and asks first, because it ends the agent's autonomy.
            KeyCode::Enter => self.open_signals(task_service, remote_signals).await,
            KeyCode::Char('o') => {
                if let Some(task) = self.selected_task() {
                    let question = format!(
                        "Take over \"{}\"? The agent stops and the terminal is yours; \
                         ctrl-a hands it back.",
                        task.title()
                    );
                    self.foreground =
                        Foreground::Confirm(PendingConfirm::taking_over(task.id, question));
                }
            }
            // Sending an existing task asks which machine first, and nothing
            // leaves until that answer is given.
            KeyCode::Char('r') => {
                if let Some(task) = self.selected_task() {
                    let hosts = self.send_hosts();
                    let title = task.title().to_owned();
                    let environment = task.facts.environment_name.clone();
                    let agent = task.agent_kind;
                    self.foreground = Foreground::SendTask(Box::new(SendTask::new(
                        task.id,
                        title,
                        environment,
                        agent,
                        hosts,
                    )));
                }
            }
            // The issue a task is bound to is a page, so it opens the same way a
            // sandbox's link does: through this machine's browser, by the
            // operator's own act.
            KeyCode::Char('b') => {
                if let Some(issue) = self
                    .selected_task()
                    .and_then(|task| task.facts.tracker_issue)
                {
                    match open_in_browser(&issue.url) {
                        Ok(()) => tracing::info!(%issue.url, "opened a task's issue"),
                        Err(error) => {
                            self.notice = Some(format!("could not open {}: {error}", issue.url))
                        }
                    }
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
        landings: &mpsc::UnboundedSender<Result<String, String>>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        // A task on another machine has to travel before it can be taken over, and
        // travelling is a wait rather than a keystroke.
        if let Some(held) = self
            .remote_tasks
            .iter()
            .find(|held| held.summary.id == id)
            .cloned()
        {
            self.ask_for_task(&held, event_loop, landings);
            return;
        }
        let Some(task) = self.tasks.iter().find(|task| task.id == id).cloned() else {
            return;
        };
        if task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer) {
            self.notice = Some("this task is awaiting transfer confirmation".into());
            return;
        }
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
        let shell_id = ShellId::Task(task.id.clone());
        if !self.shells.contains_key(&shell_id) && !event_loop.has_active_run(&task.id) {
            self.notice = Some("this task has no run to attach to".into());
            return;
        }
        self.foreground = Foreground::Shell(shell_id.clone());
        self.notice = None;
        if self.shells.contains_key(&shell_id) {
            let Some((rows, cols)) = self.shell_size(terminal) else {
                return;
            };
            event_loop.resize_terminal(&task.id, rows, cols);
            let id = shell_id;
            if let Some(shell) = self.shells.get_mut(&id) {
                shell.resize(rows, cols);
            }
            self.foreground = Foreground::Shell(id);
            return;
        }
        let Some((rows, cols)) = self.shell_size(terminal) else {
            return;
        };
        let history = event_loop.cached_terminal_output(&task.id);
        event_loop.resize_terminal(&task.id, rows, cols);
        let id = ShellId::Task(task.id.clone());
        self.shells.insert(
            id.clone(),
            Shell::new(rows, cols, history, TASK_SHELL.to_owned()),
        );
        self.foreground = Foreground::Shell(id);
    }

    /// The size a terminal for a task should be, saying so when it cannot be read
    /// rather than taking the whole interface down with it.
    pub(in crate::tui) fn shell_size(&mut self, terminal: &DefaultTerminal) -> Option<(u16, u16)> {
        match terminal.size() {
            Ok(size) => Some(screens::shell::size(self.shell_area(size.into()))),
            Err(error) => {
                self.notice = Some(error.to_string());
                None
            }
        }
    }

    pub(in crate::tui) fn git_identity(&self) -> GitIdentity {
        self.git_identity.clone()
    }

    async fn refresh_git_identity(&mut self) {
        let path = self.config.profile_dir("vcs/github").join("git/config");
        let output = HostCommand::new("git")
            .args(["config", "--no-includes", "--file"])
            .arg(path)
            .args([
                "--get-regexp",
                "^(user\\.(name|email|signingkey)|commit\\.gpgsign)$",
            ])
            .output()
            .await;
        let mut identity = GitIdentity::default();
        if let Ok(output) = output {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let Some((key, value)) = line.split_once(' ') else {
                    continue;
                };
                match key {
                    "user.name" => identity.name = Some(value.to_owned()),
                    "user.email" => identity.email = Some(value.to_owned()),
                    "user.signingkey" => identity.signing_key = Some(value.to_owned()),
                    "commit.gpgsign" => {
                        identity.signs_commits = matches!(value, "true" | "yes" | "on" | "1")
                    }
                    _ => {}
                }
            }
        }
        self.git_identity = identity;
    }

    pub(in crate::tui) fn shell_area(&self, area: Rect) -> Rect {
        super::widgets::chrome::content_area(
            area,
            &self.hints(),
            self.keyboard_hints(),
            self.notice.as_deref(),
        )
    }

    /// Asks one machine what it is holding, which is where those tasks live.
    async fn read_tasks(
        address: &str,
        keys: &RemoteKeys,
        client: &LinkClient,
    ) -> Result<Vec<TaskSummary>, String> {
        let remembered = keys.remembered(address).await.ok().flatten();
        let sighting = meet(address, DEFAULT_PORT, remembered.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        client
            .list_tasks(sighting.ticket)
            .await
            .map_err(|error| error.to_string())
    }

    /// Read what a task is asking, and show it.
    ///
    /// A task on another machine is asked over the link, off this loop; this
    /// machine's own is read from the store here, because that is a file rather
    /// than a wait.
    pub(in crate::tui) async fn open_signals<E>(
        &mut self,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if let Some(held) = self.selected_held().cloned() {
            self.open_remote_signals(held, remote_signals);
            return;
        }
        self.open_local_signals(task_service).await;
    }

    async fn open_local_signals<E>(&mut self, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(task) = self.selected_task() else {
            return;
        };
        // The list is this machine's now, whatever remote was open before.
        self.remote_signals = None;
        self.remote_signals_loading = false;
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

    /// Open the signals of a task another machine owns, and read them over the
    /// link.
    ///
    /// The pop-up is shown before the answer arrives, so the operator sees the
    /// wait rather than a frozen list. The attempt in the reply says which opening
    /// it belongs to, so a reply from one already left behind cannot replace what
    /// is on screen.
    fn open_remote_signals(&mut self, held: HeldTask, replies: &mpsc::Sender<RemoteSignalsReply>) {
        self.remote_signals_attempt = self.remote_signals_attempt.wrapping_add(1);
        let remote = RemoteSignals {
            address: held.address,
            task_id: held.summary.id,
            attempt: self.remote_signals_attempt,
        };
        self.remote_signals = Some(remote.clone());
        self.remote_signals_loading = true;
        self.operator_signals.clear();
        self.selected_operator_signal = 0;
        self.selected_signal_action = 0;
        self.operator_signal_scroll = 0;
        self.foreground = Foreground::Signals;
        let client = self.link.clone();
        let keys = self.remote_keys.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let signals =
                Self::read_remote_signals(&remote.address, &remote.task_id, &keys, &client).await;
            let _ = replies
                .send(RemoteSignalsReply::Opened { remote, signals })
                .await;
        });
    }

    /// Leave the signals pop-up, forgetting which remote task it was about so a
    /// reply that lands after this changes nothing.
    fn leave_signals(&mut self) {
        self.remote_signals = None;
        self.remote_signals_loading = false;
        self.foreground = Foreground::Tasks;
    }

    /// What the signals pop-up says above the list: the wait while a remote is
    /// being asked, or the selected task's own last words.
    pub(in crate::tui) fn signals_summary(&self) -> Option<String> {
        // A remote's own last words, read from what it last answered with. The
        // task is not in this machine's list, so it is not looked for there.
        if let Some(remote) = &self.remote_signals {
            if self.remote_signals_loading {
                return Some(format!("asking {}…", remote.address));
            }
            return self
                .remote_tasks
                .iter()
                .find(|held| held.address == remote.address && held.summary.id == remote.task_id)
                .and_then(|held| held.summary.latest_turn_summary.clone());
        }
        self.selected_task()
            .and_then(|task| task.facts.latest_turn_summary)
    }

    /// The address to reach a remote at, once its key is checked against the one
    /// remembered for it. A different key is refused: signals are not something to
    /// send to a stranger.
    async fn remote_peer(address: &str, keys: &RemoteKeys) -> Result<EndpointAddr, String> {
        let remembered = keys
            .remembered(address)
            .await
            .map_err(|error| error.to_string())?;
        let sighting = meet(address, DEFAULT_PORT, remembered.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        if sighting.key_status == RemoteKeyStatus::Changed {
            return Err(format!(
                "{address} answered with a different key; approve its identity first"
            ));
        }
        Ok(sighting.ticket.into())
    }

    /// Read one remote task's unresolved signals, and mark them read there.
    async fn read_remote_signals(
        address: &str,
        task_id: &str,
        keys: &RemoteKeys,
        client: &LinkClient,
    ) -> Result<Vec<TaskOperatorSignal>, String> {
        let peer = Self::remote_peer(address, keys).await?;
        let mut signals = client
            .operator_signals(peer.clone(), task_id)
            .await
            .map_err(|error| error.to_string())?;
        // Marking them read is best effort: the questions are what the operator
        // came for, and a read that fails should not hide them.
        if let Err(error) = client.read_operator_signals(peer, task_id).await {
            tracing::debug!(%error, address, task_id, "could not mark a remote's signals read");
        }
        for signal in &mut signals {
            signal.read = true;
        }
        Ok(signals)
    }

    /// Send one action to the machine that owns the open signals.
    fn act_on_remote_signal(
        &self,
        signal_id: String,
        action: RemoteSignalAction,
        answer: Option<String>,
        replies: &mpsc::Sender<RemoteSignalsReply>,
    ) {
        let Some(remote) = self.remote_signals.clone() else {
            return;
        };
        let client = self.link.clone();
        let keys = self.remote_keys.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result = Self::send_remote_signal_action(
                &remote,
                &signal_id,
                action,
                answer.as_deref(),
                &keys,
                &client,
            )
            .await;
            let _ = replies
                .send(RemoteSignalsReply::Resolved {
                    remote,
                    signal_id,
                    action,
                    result,
                })
                .await;
        });
    }

    async fn send_remote_signal_action(
        remote: &RemoteSignals,
        signal_id: &str,
        action: RemoteSignalAction,
        answer: Option<&str>,
        keys: &RemoteKeys,
        client: &LinkClient,
    ) -> Result<(), String> {
        let peer = Self::remote_peer(&remote.address, keys).await?;
        match action {
            RemoteSignalAction::Answer => client
                .answer_operator_signal(peer, signal_id, answer.unwrap_or_default())
                .await
                .map_err(|error| error.to_string()),
            RemoteSignalAction::Acknowledge => client
                .acknowledge_operator_signal(peer, signal_id)
                .await
                .map_err(|error| error.to_string()),
            RemoteSignalAction::Confirm => client
                .confirm_operator_signal(peer, signal_id)
                .await
                .map_err(|error| error.to_string()),
        }
    }

    /// Take what came back from a remote task's signals. A reply that belongs to
    /// a pop-up the operator has left changes nothing; one that succeeds drops the
    /// resolved signal and re-reads the machine's cards.
    pub(in crate::tui) fn remote_signals_resolved(
        &mut self,
        reply: RemoteSignalsReply,
        reads: &mpsc::UnboundedSender<Vec<HeldTask>>,
    ) {
        match reply {
            RemoteSignalsReply::Opened { remote, signals } => {
                if !self.is_current_remote_signals(&remote) {
                    return;
                }
                self.remote_signals_loading = false;
                match signals {
                    Ok(signals) => {
                        self.operator_signals = signals;
                        self.selected_operator_signal = 0;
                        self.selected_signal_action = 0;
                        self.operator_signal_scroll = 0;
                    }
                    // The pop-up stays open with what it has, which is nothing
                    // new: the operator can retry by opening it again.
                    Err(error) => self.notice = Some(error),
                }
            }
            RemoteSignalsReply::Resolved {
                remote,
                signal_id,
                action,
                result,
            } => {
                if !self.is_current_remote_signals(&remote) {
                    return;
                }
                match result {
                    Ok(()) => {
                        if let Some(index) = self
                            .operator_signals
                            .iter()
                            .position(|signal| signal.id == signal_id)
                        {
                            self.resolved_operator_signal(index);
                        }
                        if action == RemoteSignalAction::Confirm {
                            self.notice = Some("task closed; its capsule was disposed of".into());
                        }
                        // The card's unread count comes from the machine, so it
                        // is read again rather than guessed at.
                        self.read_remote_tasks(reads);
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
        }
    }

    /// Whether a reply is about the pop-up that is open right now.
    fn is_current_remote_signals(&self, remote: &RemoteSignals) -> bool {
        self.remote_signals.as_ref() == Some(remote)
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
            TaskExecutionUpdate::Created(Ok(task)) => {
                // A task that was just asked for is the one in view, whether it
                // was founded or an existing binding was reused for it.
                let id = task.id.clone();
                self.replace_task(task);
                if let Some(index) = self.tasks.iter().position(|task| task.id == id) {
                    self.selected = index;
                }
            }
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
                self.process_shell_output(&ShellId::Task(task_id), bytes);
            }
            TaskExecutionUpdate::OperatorReady(task) => {
                let id = task.id.clone();
                self.replace_task(task);
                let shell_id = ShellId::Task(id.clone());
                self.foreground = Foreground::Shell(shell_id.clone());
                self.notice = None;
                let (rows, cols) = screens::shell::size(self.shell_area(terminal.size()?.into()));
                let history = event_loop.cached_terminal_output(&id);
                event_loop.resize_terminal(&id, rows, cols);
                self.shells.insert(
                    shell_id.clone(),
                    Shell::new(rows, cols, history, TASK_SHELL.to_owned()),
                );
                self.foreground = Foreground::Shell(shell_id);
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

    /// Feed one terminal's bytes to the shell that draws it, and do what its program
    /// asked for along the way.
    ///
    /// The same for a task's shell and a credential shell, because the terminal is
    /// the same: only where the bytes came from differs.
    fn process_shell_output(&mut self, id: &ShellId, bytes: Vec<u8>) {
        let mut asked_to_open = None;
        {
            let Some(shell) = self.shells.get_mut(id) else {
                return;
            };
            for request in shell.process(bytes) {
                match request {
                    GuestRequest::Copy(text) => match copy_to_clipboard(text) {
                        Ok(()) => {
                            shell.show_clipboard_confirmation();
                            tracing::info!("copied terminal text to clipboard");
                        }
                        Err(error) => tracing::error!(%error, "could not copy terminal text"),
                    },
                    GuestRequest::Open(url) => asked_to_open = Some(url),
                }
            }
        }
        let Some(url) = asked_to_open else {
            return;
        };
        match Link::parse(&url) {
            // A question rather than an act: a sandbox runs what it likes, and this
            // machine opens what its operator says it opens.
            Some(link) => self.foreground = Foreground::OpenLink(OpenLink::new(id.clone(), link)),
            None => tracing::debug!(%url, "a sandbox asked to open something that is not a page"),
        }
    }

    /// Take the answer to a sandbox's link.
    pub(in crate::tui) fn open_link_key(&mut self, key: KeyEvent) {
        let Foreground::OpenLink(mut asking) = mem::take(&mut self.foreground) else {
            return;
        };
        let shell = asking.shell().clone();
        match asking.handle_key(key) {
            None => self.foreground = Foreground::OpenLink(asking),
            Some(open_link::Decision::No) => self.foreground = Foreground::Shell(shell),
            Some(open_link::Decision::Open) => {
                let url = asking.url().to_owned();
                self.foreground = Foreground::Shell(shell);
                match open_in_browser(&url) {
                    Ok(()) => tracing::info!(%url, "opened a sandbox's link"),
                    Err(error) => self.notice = Some(format!("could not open {url}: {error}")),
                }
            }
        }
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

/// Open an address in this machine's browser.
///
/// One argument and never a shell word: the sandbox chooses the address, and a
/// sandbox is not to be trusted with this machine's command line.
fn open_in_browser(url: &str) -> Result<(), String> {
    let opener = match env::consts::OS {
        "macos" => "open",
        _ => "xdg-open",
    };
    let status = process::Command::new(opener)
        .arg(url)
        .status()
        .map_err(|error| error.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{opener} exited with {status}"))
}

/// Send a configuration bundle to a remote, and return only once it has
/// acknowledged holding it. The dial is what makes this a push rather than a
/// preference.
async fn deliver_config(
    address: &str,
    settings: String,
    data_dir: PathBuf,
    profiles: Vec<&'static str>,
    keys: &RemoteKeys,
    client: &LinkClient,
) -> Result<(), String> {
    let remembered = keys.remembered(address).await.ok().flatten();
    let sighting = meet(address, DEFAULT_PORT, remembered.as_deref())
        .await
        .map_err(|error| error.to_string())?;
    if sighting.key_status == RemoteKeyStatus::Changed {
        return Err(format!(
            "{address} answered with a different key; approve its identity first"
        ));
    }
    let peer: EndpointAddr = sighting.ticket.into();
    // What it already holds is asked over the authenticated link, not read off the
    // unauthenticated announcement: a login is not something to send again because
    // a stranger said it was missing.
    let held = client
        .held_profiles(peer.clone())
        .await
        .map_err(|error| error.to_string())?;
    let bundle = ConfigBundle::of(settings, &data_dir, &profiles, &held)
        .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(&bundle).map_err(|error| error.to_string())?;
    client
        .put_config(peer, &bytes)
        .await
        .map_err(|error| error.to_string())
}

/// Open the settings panel, reading back the two things it cannot see for
/// itself: what each address last answered with, and what the built base
/// environment is made of.
async fn open_panel<E>(app: &mut App, task_service: &TaskService<E>)
where
    E: IsolationEngine + Clone + 'static,
{
    app.foreground = Foreground::Panel;
    app.panel = Panel::new();
    if !app.reload_settings() {
        return;
    }
    app.base_description = task_service.base_description();
    app.profiles = task_service.profiles();
    app.tracker_state = match task_service.tracker() {
        Some(tracker) if tracker.is_connected() => TrackerState::Connected,
        _ => TrackerState::NotConnected,
    };
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
        .map_or(id, |task| task.title());
    app.notice = Some(format!("{title}: {error}"));
}
