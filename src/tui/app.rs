//! The application: what it is holding, and what it does with it.
//!
//! One value holds everything a running client knows — the tasks, the settings,
//! the shells, and what the keyboard is answering to — and the methods here are
//! what it does in response. Nothing in this file talks to a terminal except to
//! draw, and nothing here reads an event.

use std::{
    collections::{HashMap, HashSet},
    env, io, mem,
    path::PathBuf,
    process,
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use iroh::{EndpointAddr, EndpointId};
use ratatui::{DefaultTerminal, layout::Rect, text::Line};
use tokio::{process::Command as HostCommand, sync::mpsc, task::JoinHandle, time::sleep};

use crate::{
    agent::AgentKind,
    config::{Config, DEFAULT_PORT, SettingsError, SettingsFile},
    credentials::{
        CredentialAnswer, CredentialShell, CredentialShellCancel, CredentialShellEvent,
        credential_shell_id,
    },
    isolation::{EnvironmentDescription, EnvironmentProgress, IsolationEngine, ProfileSummary},
    project::{Project, TrackerRoute, normalize_repository},
    remote::{
        Announcement, ConfigBundle, ConfigPush, DiscoveredRemote, DiscoveryEvent, LinkClient,
        RemoteKey, RemoteKeyStatus, RemoteKeys, RemoteState, TaskOrigin, TaskRequest, TaskSummary,
        TrackerConnection, WatchEvent, bring_home, meet, send::send_to_remote, transfer,
        transfer::TransferUpdate,
    },
    task::{
        Task, TaskEventLoop, TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
        TaskExecutionUpdate, TaskOperatorSignalAuthor, TaskOperatorSignalKind,
        TaskOperatorSignalMessage, TaskOperatorSignalThread, TaskService, TaskServiceError,
    },
    tracker::{IssueReference, IssueSnapshot, IssueSummary, TrackerKind},
    tui::keys::{is_dismiss, is_remove, is_secondary},
    tui::popups,
    tui::screens,
    tui::widgets::{
        chrome::Hint,
        pointer::Targets,
        select::{Offer, Select},
    },
};

use super::background::terrain::Terrain;
use super::widgets::{alert::Alert, list};
use popups::confirm::{CredentialTarget, PendingConfirm};
use popups::new_task::{Answer, Hit as NewTaskHit, Host, NewTask, SearchToken};
use popups::open_link::{self, Link, OpenLink};
use popups::send_task::{Decision as SendDecision, SendTask};
use popups::setup::{Setup, SetupKind};
use popups::sign_in::{Decision as SignInDecision, SignIn};
use screens::settings::{
    CredentialKind, Depth, Focus, Form, GitIdentity, Panel, RowKind, Section, Setting, SettingsHit,
    Target,
};
use screens::shell::{GuestRequest, Shell};
use screens::signals::{
    Composer, SignalAction, SignalFilter, SignalFocus, SignalHit, SignalSort, actions,
    is_lingering, primary_action, secondary_action,
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
pub(in crate::tui) enum Foreground {
    /// The keys for wherever you are, over whatever is there. Help is stacked
    /// rather than exclusive: dismissing it returns to what it covered.
    Keys(Box<Foreground>),
    /// A question waiting on an answer, and what the answer would do.
    Confirm(PendingConfirm),
    /// Choosing a machine to continue a task that is already here.
    SendTask(Box<SendTask>),
    /// The creation question, holding the prompt until it is answered.
    NewTask(Box<NewTask>),
    /// The settings panel: environments, remotes, the interface, the identity.
    Panel,
    Setup(Setup),
    /// What a tool's own sign-in asks, before its terminal opens.
    SignIn(Box<SignIn>),
    /// A task's signal threads: what is waiting, and the conversations about it.
    Signals,
    /// A terminal, taking every key there is.
    Shell(ShellId),
    /// A sandbox asking to open a link, over the terminal that asked.
    OpenLink(OpenLink),
    #[default]
    Tasks,
}

impl Foreground {
    /// Whether this is a fullscreen screen rather than a pop-up over the work.
    /// Screens arrive and leave the same way, whatever they show.
    pub(in crate::tui) fn is_screen(&self) -> bool {
        match self {
            Self::Panel | Self::Signals => true,
            Self::Keys(behind) => behind.is_screen(),
            _ => false,
        }
    }

    /// Whether the signals screen is what is being read, possibly with a
    /// question about one of its threads laid over it.
    pub(in crate::tui) fn shows_signals(&self) -> bool {
        match self {
            Self::Signals => true,
            Self::Confirm(confirm) => confirm.is_signal(),
            Self::Keys(behind) => behind.shows_signals(),
            _ => false,
        }
    }

    pub(in crate::tui) fn shows_settings(&self) -> bool {
        match self {
            Self::Panel | Self::Setup(_) | Self::SignIn(_) => true,
            // A credential shell is opened from the settings list it is about, and
            // goes back to them, so the panel is the screen it sits on.
            Self::Shell(ShellId::Credentials(_)) => true,
            Self::Confirm(confirm) => confirm.is_settings(),
            Self::Keys(behind) => behind.shows_settings(),
            _ => false,
        }
    }
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
    /// A login file is on disk, but no CLI has been asked whether it still works.
    Saved,
    /// Nothing could answer: no check ran, so nothing is known either way.
    Unanswered,
}

impl CredentialStatus {
    fn is_in_progress(&self) -> bool {
        *self == Self::Checking
    }

    /// The list's word for this status, so the row and the page say the same
    /// thing rather than keeping two vocabularies in step.
    pub(in crate::tui) fn summary(&self) -> &'static str {
        match self {
            Self::Checking => "checking…",
            Self::Checked {
                signed_in: true, ..
            } => "signed in",
            Self::Checked {
                signed_in: false, ..
            } => "not signed in",
            Self::Saved => "saved login",
            Self::Unknown | Self::Unanswered => "not checked",
        }
    }
}

pub(in crate::tui) struct CredentialCheckReply {
    pub generation: u64,
    pub answers: Vec<(&'static str, CredentialStatus)>,
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

impl TrackerState {
    /// The list's word for this state, shared with the page below it.
    pub(in crate::tui) fn summary(self) -> &'static str {
        match self {
            Self::NotConnected => "not signed in",
            Self::Connecting => "connecting…",
            Self::Connected => "signed in",
        }
    }
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

/// What it says when the build carries no tracker provider at all.
pub(in crate::tui) const NO_LINEAR: &str = "Linear support is not available in this build.";

/// How long the list is shown before its addresses are dialled, so moving through
/// the sections does not reach anything.
const ASK_AFTER: Duration = Duration::from_millis(400);

/// How long after nothing could check a login the section tries again, so a sandbox
/// that will not start is not asked on every tick.
const CHECK_RETRY_AFTER: Duration = Duration::from_secs(15);
/// How long a task watch waits before dialling again, and how far that grows.
const WATCH_RETRY: Duration = Duration::from_secs(1);
const WATCH_RETRY_MAX: Duration = Duration::from_secs(30);
/// A watch that lasted less than this never connected, so its retry backs off.
const WATCH_SETTLED: Duration = Duration::from_secs(5);

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
        /// What there is to say about the wait. Absent when the environment is
        /// already built, because then there is no wait to show.
        progress: Option<String>,
        cancel: CredentialShellCancel,
    },
    /// A terminal to draw, and the channel its keys go down.
    Open(CredentialShell),
}

/// A transfer the interface is watching, and what it can say about it.
///
/// Bytes are the only thing a transfer has to report, so the numbers the bar
/// draws are kept here rather than in the link: the link counts, and this turns
/// counting into a speed over the last moment.
pub(in crate::tui) struct TransferView {
    title: String,
    label: String,
    watching: transfer::Watching,
    update: TransferUpdate,
    /// The last count and when it was read, which is one half of the next rate.
    measured: Option<(Instant, u64)>,
    /// Bytes per second, smoothed: one piece is far too small a sample to show.
    rate: Option<f64>,
    /// Whether the operator has asked it to stop, which the bar says while the
    /// transfer finishes the piece it is on.
    is_stopping: bool,
    /// Whether the terminal opens when it arrives, or the task is only moved here.
    opens_terminal: bool,
}

impl TransferView {
    /// Reads what the transfer has to say, and says whether anything changed.
    fn observe(&mut self) -> bool {
        if !self.watching.updates.has_changed().unwrap_or(false) {
            return false;
        }
        self.update = self.watching.updates.borrow_and_update().clone();
        let now = Instant::now();
        if let Some((then, was)) = self.measured {
            let elapsed = now.duration_since(then).as_secs_f64();
            if elapsed > 0.15 && self.update.done >= was {
                let sample = (self.update.done - was) as f64 / elapsed;
                self.rate = Some(match self.rate {
                    Some(previous) => previous * 0.5 + sample * 0.5,
                    None => sample,
                });
                self.measured = Some((now, self.update.done));
            }
        } else {
            self.measured = Some((now, self.update.done));
        }
        true
    }

    fn progress(&self) -> super::widgets::progress::Progress<'_> {
        super::widgets::progress::Progress {
            title: &self.title,
            label: &self.label,
            stage: if self.is_stopping {
                "stopping…"
            } else {
                &self.update.stage
            },
            done: self.update.done,
            total: self.update.total,
            rate: self.rate,
        }
    }
}

/// What a machine's watch reported, named by the machine so one channel can carry
/// every remote.
pub enum WatchedTasks {
    /// The watch is dialling, so this machine is being asked about.
    Dialling {
        address: String,
    },
    /// It answered. What it said about itself is the only place an address becomes
    /// a machine with a name, a key and a state.
    Reached {
        address: String,
        announcement: Box<Announcement>,
        key_status: RemoteKeyStatus,
    },
    /// It can be given work now, which is not a change to any task.
    Ready {
        address: String,
    },
    /// Everything it holds for this client, as of the moment the watch opened.
    Snapshot {
        address: String,
        summaries: Vec<TaskSummary>,
    },
    Changed {
        address: String,
        summary: Box<TaskSummary>,
    },
    Removed {
        address: String,
        id: String,
    },
    /// The watch ended: the machine is not answering any more, and its tasks are
    /// its own.
    Ended {
        address: String,
        reason: String,
    },
}

/// Why a watch stopped dialling, which is what decides whether it dials again.
enum WatchEnd {
    /// The interface is gone.
    Closed,
    /// It answered with a key that is not the one remembered, so there is nothing
    /// to listen to until the operator decides which key this machine is.
    UnapprovedKey,
    /// It did not answer, or the stream ended.
    Lost,
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

/// The remote task the signals screen is about.
///
/// The attempt says which opening this is, so a reply carrying another attempt
/// belongs to a screen the operator has already left and changes nothing.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::tui) struct RemoteSignals {
    pub(in crate::tui) address: String,
    pub(in crate::tui) task_id: String,
    pub(in crate::tui) attempt: u64,
}

/// What the operator did with a remote signal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum RemoteSignalAction {
    Reply,
    Resolve(bool),
    Accept,
}

/// What came back from a remote task's signals: the list when it opens, or the
/// outcome of acting on one.
pub(in crate::tui) enum RemoteSignalsReply {
    Opened {
        remote: RemoteSignals,
        signals: Result<Vec<TaskOperatorSignalThread>, String>,
    },
    Resolved {
        remote: RemoteSignals,
        signal_id: String,
        action: RemoteSignalAction,
        result: Result<(), String>,
    },
}

/// What came back from asking a remote to run a task. The address names the
/// machine that answered, so the message does.
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

/// What came back from asking a machine to remove a task of its own. The address
/// names the machine, so the message can.
pub(in crate::tui) struct RemoveTaskReply {
    pub(in crate::tui) address: String,
    pub(in crate::tui) task_id: String,
    pub(in crate::tui) result: Result<(), String>,
}

/// What came back from sending this client's configuration to a remote.
///
/// The digest names the bytes sent, so edits during a sync remain pending.
pub(in crate::tui) struct ConfigPushReply {
    pub(in crate::tui) address: String,
    pub(in crate::tui) digest: String,
    pub(in crate::tui) result: Result<ConfigPush, String>,
}

/// A task tracker account this machine pushed that another client already claims
/// on the same remote.
///
/// Nothing was written until this is answered: an account says whose work work is,
/// and a machine that quietly reassigned one would attribute somebody else's.
pub(in crate::tui) struct TrackerConflict {
    address: String,
    provider: TrackerKind,
    display_name: String,
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
    /// The backdrop, on its own clock and with the glyphs it last found.
    pub(in crate::tui) terrain: Terrain,
    pub(in crate::tui) help_scroll: u16,
    /// What the host will run at once, so a wait can say whether there is room.
    /// Absent until the first refresh, and never stale by more than one interval.
    pub(in crate::tui) capacity: Option<usize>,
    /// What the interface has to say, in front of the work until it is read.
    pub(in crate::tui) alert: Option<Alert>,
    pub(in crate::tui) removing: HashSet<String>,
    pub(in crate::tui) selected: usize,
    pub(in crate::tui) first_visible_task: usize,
    pub(in crate::tui) shells: HashMap<ShellId, Shell>,
    /// Where the creation question's controls were drawn last, so a click can
    /// land on what is on screen.
    pub(in crate::tui) new_task_targets: Targets<NewTaskHit>,
    /// The shell opened to set up credentials, if there is one.
    ///
    /// One at a time: a profile is a shared directory, and two shells writing it
    /// is a way to confuse yourself.
    pub(in crate::tui) credential_shell: CredentialShellState,
    git_identity: GitIdentity,
    /// The task the signals screen is about. Held rather than read from the
    /// grid, so the screen stays about the same task while the grid is behind it.
    pub(in crate::tui) signal_task_id: Option<String>,
    /// Every thread of that task, as read when the screen opened and refreshed
    /// as the task moves. The screen shows the filtered view below.
    pub(in crate::tui) signal_all_threads: Vec<TaskOperatorSignalThread>,
    /// The threads after the filter and sort the operator chose.
    pub(in crate::tui) signal_threads: Vec<TaskOperatorSignalThread>,
    pub(in crate::tui) signal_filter: Select<SignalFilter>,
    pub(in crate::tui) signal_sort: Select<SignalSort>,
    pub(in crate::tui) signals_cursor: list::Cursor,
    /// The first message of the selected thread on screen.
    pub(in crate::tui) signal_scroll: u16,
    /// The reply being written, if one is.
    pub(in crate::tui) signal_composer: Option<Composer>,
    /// What the keys are on inside the signals screen, and which message of the
    /// selected thread they are on.
    pub(in crate::tui) signal_focus: SignalFocus,
    pub(in crate::tui) signal_message_cursor: usize,
    /// What the last signals frame drew, so a click lands on what was seen.
    pub(in crate::tui) signal_targets: Targets<SignalHit>,
    /// What the last task-grid frame drew, by card.
    pub(in crate::tui) task_targets: Targets<usize>,
    /// What the last settings frame drew, by section or row.
    pub(in crate::tui) settings_targets: Targets<SettingsHit>,
    /// Threads whose conversation changed since the last frame, with when it was
    /// noticed, so a new arrival moves rather than appearing already settled.
    pub(in crate::tui) signal_arrivals: HashMap<String, Instant>,
    /// The remote task the signals screen is about, when it is showing one from
    /// another machine.
    pub(in crate::tui) remote_signals: Option<RemoteSignals>,
    /// Whether that remote's signals are still being read.
    pub(in crate::tui) remote_signals_loading: bool,
    /// Which opening of the screen this is, so a reply from an earlier one is
    /// dropped rather than replacing what is on screen.
    remote_signals_attempt: u64,
    /// Where `config.toml` lives, so the manage view can persist changes.
    pub(in crate::tui) config: Config,
    pub(in crate::tui) settings_file: Option<SettingsFile>,
    /// The one link to remotes, shared by every exchange rather than bound per dial.
    pub(in crate::tui) link: Arc<LinkClient>,
    /// The settings panel: environments, remotes, the interface, and the identity.
    pub(in crate::tui) panel: Panel,
    /// What the keyboard is answering to.
    pub(in crate::tui) foreground: Foreground,
    /// A task travelling here from a remote, while it travels.
    pub(in crate::tui) transfer: Option<TransferView>,
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
    /// When a section's checks may be tried again after nothing could run them.
    credential_retry_at: Option<Instant>,
    /// What the remotes are holding, as of the last time each answered. Read from
    /// them rather than copied: a task belongs to the machine running it, so this is
    /// only ever as good as the last answer.
    pub(in crate::tui) remote_tasks: Vec<HeldTask>,
    /// The remote card to select once the next read of a remote's tasks lands,
    /// so a task that has just been sent there is the one in view.
    pending_remote: Option<(String, String)>,
    /// A tracker account another client already claims, waiting to be answered.
    pub(in crate::tui) tracker_conflict: Option<TrackerConflict>,
    /// Which answer the question is on: leave it, or take it over.
    pub(in crate::tui) conflict_choice: usize,
    /// Addresses whose account the operator said to take over, so the next push
    /// carries the consent.
    taking_over: HashSet<String>,
    /// Addresses whose account the operator chose to leave alone, until the
    /// configuration changes.
    conflict_declined: HashSet<String>,
    /// Machines whose tasks are being watched, one watch each, and where a watch
    /// reports what it finds.
    watched: HashMap<String, JoinHandle<()>>,
    watch_events: mpsc::UnboundedSender<WatchedTasks>,
    /// Acknowledged digests stay in memory; reconnecting rechecks the remote.
    remote_synced: HashMap<String, String>,
    configuration_digest: Option<String>,
    remote_config_inflight: HashSet<String>,
    pub(in crate::tui) discovered_remotes: HashMap<EndpointId, DiscoveredRemote>,

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

/// Whether a task is one the grid lists.
///
/// Completed work is history; the grid is for what there is still something to do.
fn is_listed(task: &Task) -> bool {
    task.facts.execution_status != TaskExecutionStatus::Completed
}

fn listed(tasks: Vec<Task>) -> Vec<Task> {
    tasks.into_iter().filter(is_listed).collect()
}

/// The same rule for a task another machine reports.
fn is_listed_summary(summary: &TaskSummary) -> bool {
    summary.status != TaskExecutionStatus::Completed
}

impl App {
    pub fn new(
        tasks: Vec<Task>,
        config: Config,
        settings: Result<SettingsFile, SettingsError>,
        remote_keys: RemoteKeys,
        link: LinkClient,
        watch_events: mpsc::UnboundedSender<WatchedTasks>,
    ) -> Self {
        let (settings_file, settings_error) = match settings {
            Ok(file) => (Some(file), None),
            Err(error) => (
                None,
                Some(format!(
                    "Altitude could not read {}.\n\n{error}",
                    config.settings_path().display()
                )),
            ),
        };
        Self {
            tasks: listed(tasks),
            grid_columns: 1,
            alert: settings_error.map(Alert::problem),
            removing: HashSet::new(),
            selected: 0,
            first_visible_task: 0,
            shells: HashMap::new(),
            new_task_targets: Targets::default(),
            credential_shell: CredentialShellState::Idle,
            git_identity: GitIdentity::default(),
            signal_task_id: None,
            signal_all_threads: Vec::new(),
            signal_threads: Vec::new(),
            signal_filter: Select::new(
                "Filter",
                vec![
                    Offer::new(SignalFilter::Unresolved, "Unresolved"),
                    Offer::new(SignalFilter::All, "All"),
                    Offer::new(SignalFilter::NeedsYou, "Needs you"),
                    Offer::new(SignalFilter::Review, "Ready to review"),
                    Offer::new(SignalFilter::Decisions, "Decisions"),
                    Offer::new(SignalFilter::Info, "Info log"),
                    Offer::new(SignalFilter::Resolved, "Resolved"),
                ],
                0,
            )
            .without_search(),
            signal_sort: Select::new(
                "Sort",
                vec![
                    Offer::new(SignalSort::Oldest, "Oldest"),
                    Offer::new(SignalSort::Newest, "Newest"),
                ],
                0,
            ),
            signals_cursor: list::Cursor::default(),
            signal_scroll: 0,
            signal_composer: None,
            signal_focus: SignalFocus::List,
            signal_message_cursor: 0,
            signal_targets: Targets::default(),
            task_targets: Targets::default(),
            settings_targets: Targets::default(),
            signal_arrivals: HashMap::new(),
            remote_signals: None,
            remote_signals_loading: false,
            remote_signals_attempt: 0,
            config,
            link: Arc::new(link),
            settings_file,
            panel: Panel::new(),
            foreground: Foreground::default(),
            transfer: None,
            remote_status: HashMap::new(),
            remote_trackers: HashMap::new(),
            credential_status: HashMap::new(),
            credentials_visible_since: None,
            credential_check_generation: 0,
            credential_retry_at: None,
            tracker_conflict: None,
            conflict_choice: 0,
            taking_over: HashSet::new(),
            conflict_declined: HashSet::new(),
            watched: HashMap::new(),
            watch_events,
            remote_synced: HashMap::new(),
            configuration_digest: None,
            remote_config_inflight: HashSet::new(),
            discovered_remotes: HashMap::new(),
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
            terrain: Terrain::default(),
            help_scroll: 0,
            capacity: None,
        }
    }
    /// Says something the operator has to read: the answer to what they pressed.
    pub(in crate::tui) fn say(&mut self, message: impl Into<String>) {
        self.alert = Some(Alert::note(message));
    }

    /// Says that what the operator pressed did not work.
    pub(in crate::tui) fn problem(&mut self, message: impl Into<String>) {
        self.alert = Some(Alert::problem(message));
    }

    pub(in crate::tui) fn dismiss_alert(&mut self) {
        self.alert = None;
    }

    pub(in crate::tui) fn replace_task(&mut self, task: Task) {
        let Some(current) = self.tasks.iter_mut().find(|current| current.id == task.id) else {
            self.tasks.insert(0, task);
            return;
        };
        // A task that has just been completed leaves the grid the moment the
        // record says so, rather than waiting for the next refresh.
        let stays = is_listed(&task);
        *current = task;
        if !stays {
            self.tasks.retain(is_listed);
            self.clamp_selection();
        }
    }

    /// Take the task list the event loop reports, leaving out what the grid does
    /// not show.
    pub(in crate::tui) fn list_tasks(&mut self, tasks: Vec<Task>) {
        self.tasks = listed(tasks);
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        self.selected = self
            .selected
            .min((self.tasks.len() + self.remote_tasks.len()).saturating_sub(1));
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

    /// Read a mouse event against the last task-grid frame.
    pub(in crate::tui) fn task_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self.task_targets.at(mouse.column, mouse.row).copied() {
                    self.selected = index;
                }
            }
            MouseEventKind::ScrollUp => {
                let columns = self.grid_columns();
                self.select_adjacent(-(columns as isize), columns);
            }
            MouseEventKind::ScrollDown => {
                let columns = self.grid_columns();
                self.select_adjacent(columns as isize, columns);
            }
            _ => {}
        }
    }

    /// Read a mouse event against the last settings frame.
    pub(in crate::tui) fn panel_mouse(&mut self, mouse: MouseEvent) {
        if self.panel.form.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(target) = self.settings_targets.at(mouse.column, mouse.row).copied()
                else {
                    return;
                };
                match target {
                    SettingsHit::Section(index) => {
                        if let Some(section) = Section::ALL.get(index) {
                            self.panel.section = *section;
                            self.panel.cursor.reset();
                            self.panel.depth = Depth::Rail;
                        }
                    }
                    SettingsHit::Row(index) => {
                        self.panel.cursor.select(index);
                        self.panel.depth = Depth::List;
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => self.scroll_mouse(mouse),
            _ => {}
        }
    }

    /// How many cards fit across the body, which is what a grid step needs.
    pub(in crate::tui) fn grid_columns(&self) -> usize {
        self.grid_columns.max(1)
    }

    /// The project a new task starts in: the first, until tabs choose one.
    pub(in crate::tui) fn active_project_id(&self) -> String {
        self.projects()
            .first()
            .map(|project| project.id.clone())
            .unwrap_or_default()
    }

    /// The projects a task can be founded in.
    ///
    /// Built from the settings rather than stored on the app: the list is small,
    /// and a copy kept here would be one more thing to keep in step with the file.
    pub(in crate::tui) fn projects(&self) -> Vec<Project> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().projects().to_vec())
            .unwrap_or_default()
    }

    pub(in crate::tui) fn reload_settings(&mut self) -> bool {
        match self.config.load_or_create_settings() {
            Ok(file) => {
                self.settings_file = Some(file);
                self.alert = None;
                true
            }
            Err(error) => {
                self.settings_file = None;
                self.problem(format!(
                    "Altitude could not read {}.\n\n{error}",
                    self.config.settings_path().display()
                ));
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
            self.problem("Settings are unavailable; fix the file and reopen");
            return;
        };
        let mut proposed = file.settings().clone();
        proposed.set_agent(agent);
        match file.save(proposed) {
            Ok(()) => self.alert = None,
            Err(error) => self.problem(error.to_string()),
        }
    }

    /// Where the last task was founded, when it was not this machine: the answer
    /// the next creation question opens on.
    pub(in crate::tui) fn default_task_host(&self) -> Option<String> {
        self.settings_file
            .as_ref()?
            .settings()
            .default_task_host()
            .map(str::to_owned)
    }

    /// Remember where a task was founded, so the next one starts there.
    fn set_default_task_host(&mut self, address: Option<&str>) {
        let Some(file) = self.settings_file.as_mut() else {
            return;
        };
        if file.settings().default_task_host() == address {
            return;
        }
        let mut proposed = file.settings().clone();
        proposed.set_default_task_host(address);
        if let Err(error) = file.save(proposed) {
            self.problem(error.to_string());
        }
    }

    /// Whether the keys that work are listed at the bottom of the screen.
    pub(in crate::tui) fn keyboard_hints(&self) -> bool {
        self.settings_file
            .as_ref()
            .is_none_or(|file| file.settings().interface().keyboard_hints)
    }

    /// Whether the interface animates: the transitions between what is in
    /// front, and the drifting backdrop behind it.
    pub(in crate::tui) fn animations(&self) -> bool {
        self.settings_file
            .as_ref()
            .is_none_or(|file| file.settings().interface().animations)
    }

    /// The frame counter that drives animated drawing, absent while animations
    /// are off. What is animated reads this rather than the counter itself, so
    /// there is one place that decides whether anything moves at all.
    pub(in crate::tui) fn animation_tick(&self) -> Option<usize> {
        self.animations().then_some(self.shimmer)
    }

    /// Whether the faster foreground clock has anything to move.
    ///
    /// The backdrop is not included: it has its own clock. This is the question
    /// that decides whether an idle list wakes up twelve times a second for text
    /// nobody is waiting on.
    pub(in crate::tui) fn needs_shimmer(&self) -> bool {
        self.has_modal()
            || self.is_awaiting_progress()
            || !self.environment_builds.is_empty()
            || self
                .remote_tasks
                .iter()
                .any(|held| held.summary.status == TaskExecutionStatus::Pending)
    }

    /// The few actions worth keeping visible below the current screen.
    pub(in crate::tui) fn hints(&self) -> Vec<Hint> {
        // A message in front of the work is what the keys are about until it is
        // read, and a transfer under it keeps the one key it has.
        if self.alert.is_some() || self.transfer.is_some() {
            let mut hints = Vec::new();
            if self.alert.is_some() {
                hints.push(Hint::new("enter", "dismiss"));
            }
            if self.transfer.is_some() {
                hints.push(Hint::new("esc", "stop the transfer"));
            }
            return hints;
        }
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
            // Help owns the screen while it is open, so the line under it shows
            // how to move through and leave the help itself.
            Foreground::Keys(_) => vec![
                Hint::new("↑/↓ · j/k · pgup/pgdn", "scroll"),
                Hint::new("esc / q", "close"),
            ],
            Foreground::Confirm(pending) => pending.hints(),
            Foreground::SendTask(sending) => sending.hints(),
            Foreground::NewTask(new_task) => new_task.hints(),
            Foreground::Panel => {
                // Only a wait with something to show offers leaving it; a shell that
                // opens at once would otherwise flash a key that means nothing yet.
                if let CredentialShellState::Opening {
                    progress: Some(_), ..
                } = &self.credential_shell
                {
                    return vec![Hint::new("esc / q", "cancel")];
                }
                if self.panel.form.is_some() {
                    return vec![
                        Hint::new("tab", "next"),
                        Hint::new("ctrl-enter", "save"),
                        Hint::new("enter", "next, or add a line"),
                        Hint::new("esc", "leave the box, then the page"),
                    ];
                }
                if self.panel.depth == Depth::Rail {
                    return vec![
                        Hint::new("j/k", "section"),
                        Hint::new("enter / tab", "open"),
                        dismiss(),
                    ];
                }
                let mut hints = vec![Hint::new("tab", "change column"), Hint::new("j/k", "move")];
                // One row, asked once: what it offers, and whether it can be
                // signed out of or removed.
                let row = self.selected_row();
                if let Some(row) = &row {
                    hints.extend(row.hints(self.is_connecting_tracker()));
                }
                if row.as_ref().is_some_and(|row| row.forgets_credentials()) {
                    hints.push(Hint::new("delete", "sign out"));
                }
                if self.panel.section.grows() {
                    hints.push(Hint::new("n", "new"));
                }
                if row.as_ref().is_some_and(|row| row.is_removable()) {
                    hints.push(Hint::new("d", "delete"));
                }
                hints.push(Hint::new("pgup/pgdn", "details"));
                hints.push(dismiss());
                hints
            }
            Foreground::SignIn(sign_in) => sign_in.hints(),
            Foreground::Setup(setup) => match &setup.kind {
                SetupKind::RemoteTrackers => vec![
                    Hint::new("j/k", "choose"),
                    Hint::new("enter", "setup guide"),
                    Hint::new("esc / q", "close"),
                ],
                SetupKind::RemoteTrackerGuide(_) => vec![
                    Hint::new("j/k · pgup/pgdn", "scroll"),
                    Hint::new("esc / q", "back"),
                ],
                _ => vec![
                    Hint::new("j/k · pgup/pgdn", "scroll"),
                    Hint::new("esc / q", "close"),
                ],
            },
            Foreground::Signals => {
                if self.signal_composer.is_some() {
                    return vec![
                        Hint::new("enter", "send"),
                        Hint::new("shift-enter", "new line"),
                        Hint::new("esc", "cancel"),
                    ];
                }
                if self.signal_filter.is_open() || self.signal_sort.is_open() {
                    return vec![
                        Hint::new("j/k", "choose"),
                        Hint::new("enter", "take"),
                        Hint::new("esc", "close"),
                    ];
                }
                let mut hints = vec![
                    Hint::new("tab", "focus"),
                    Hint::new(
                        "j/k",
                        match self.signal_focus {
                            SignalFocus::List => "thread",
                            SignalFocus::Detail => "message",
                            SignalFocus::Action(_) => "button",
                            SignalFocus::Filter | SignalFocus::Sort => "control",
                        },
                    ),
                    Hint::new("f", "filter"),
                    Hint::new("s", "sort"),
                ];
                if matches!(self.signal_focus, SignalFocus::Filter | SignalFocus::Sort) {
                    hints.push(Hint::new("enter", "open"));
                } else if let Some(thread) = self.signal_thread() {
                    if let SignalFocus::Action(index) = self.signal_focus
                        && let Some(action) = self.signal_actions().get(index).copied()
                    {
                        hints.push(Hint::new(
                            "enter",
                            action.label(thread.kind, thread.is_resolved()),
                        ));
                    } else if let Some(action) = primary_action(thread) {
                        hints.push(Hint::new(
                            "enter",
                            action.label(thread.kind, thread.is_resolved()),
                        ));
                    }
                    if let Some(action) = secondary_action(thread) {
                        hints.push(Hint::new(
                            "ctrl-enter",
                            action.label(thread.kind, thread.is_resolved()),
                        ));
                    }
                }
                hints.push(Hint::new("pgup/pgdn", "scroll"));
                hints.push(dismiss());
                hints
            }
            // A shell takes every key there is, so it has no `?` to offer.
            Foreground::Shell(ShellId::Task(_)) => {
                vec![Hint::new("ctrl-a", "hand it back")]
            }
            // Nothing to hand back to: the sandbox goes with the shell.
            Foreground::Shell(ShellId::Credentials(_)) => vec![Hint::new("ctrl-a", "close")],
            Foreground::OpenLink(asking) => asking.hints(),
            Foreground::Tasks => {
                // Only what there is something to do it to. A remote's tasks sit
                // beside this machine's, so a list holding only theirs still has
                // something to select and act on.
                let mut hints = vec![
                    Hint::new("c", "create"),
                    Hint::new("ctrl-l", "link issue"),
                    Hint::new("s", "settings"),
                ];
                if !self.tasks.is_empty() || !self.remote_tasks.is_empty() {
                    hints.extend([
                        Hint::new("enter", "signals"),
                        Hint::new("ctrl-enter", "take over"),
                    ]);
                }
                // Drawn from different places, but acted on the same way: a task is
                // moved across with one key, and deleted by the machine holding it.
                if self.selected_is_remote() {
                    hints.push(Hint::new("p", "pull here"));
                    hints.push(Hint::new("d", "delete"));
                    if self
                        .selected_held()
                        .is_some_and(|held| held.summary.tracker_issue.is_some())
                    {
                        hints.push(Hint::new("b", "open issue"));
                    }
                } else {
                    if !self.tasks.is_empty() {
                        hints.push(Hint::new("d", "delete"));
                        hints.push(Hint::new("p", "push to remote"));
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
            Foreground::NewTask(new_task) => new_task.is_typing(),
            _ => {
                self.signal_composer.is_some()
                    || self
                        .panel
                        .form
                        .as_ref()
                        .is_some_and(|form| form.focus.takes_text())
            }
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

    /// Whether a list is still catching up with its cursor.
    pub(in crate::tui) fn is_gliding(&self) -> bool {
        self.signals_cursor.is_gliding() || self.panel.cursor.is_gliding()
    }

    /// Move the lists that are still catching up with their cursors, which is
    /// what makes a jump read as the list moving. Answers whether anything
    /// moved, so the loop knows to draw.
    pub(in crate::tui) fn glide_lists(&mut self, elapsed: Duration) -> bool {
        self.signals_cursor.glide(elapsed) | self.panel.cursor.glide(elapsed)
    }

    /// Whether a modal is in front: a pop-up or the panel, rather than the work
    /// itself or a terminal. What the transition animates.
    pub(in crate::tui) fn has_modal(&self) -> bool {
        // A transfer on its way, or a message, is a modal over the list and
        // leaves like one.
        self.transfer.is_some()
            || self.alert.is_some()
            || matches!(
                self.foreground,
                Foreground::Keys(_)
                    | Foreground::Confirm(_)
                    | Foreground::SendTask(_)
                    | Foreground::NewTask(_)
                    | Foreground::Panel
                    | Foreground::Setup(_)
                    | Foreground::SignIn(_)
                    | Foreground::Signals
                    | Foreground::OpenLink(_)
            )
    }

    /// Whether the app wants the terminal's mouse reports.
    ///
    /// A shell needs them to hand to its program, and a modal that scrolls needs
    /// the wheel. Everywhere else they are left to the terminal, so selecting
    /// text still works.
    pub(in crate::tui) fn wants_mouse(&self) -> bool {
        self.open_shell().is_some()
            || matches!(
                self.foreground,
                Foreground::Keys(_)
                    | Foreground::Setup(_)
                    | Foreground::Signals
                    | Foreground::Panel
                    | Foreground::NewTask(_)
                    | Foreground::Tasks
            )
    }

    /// Move whatever is in front by one wheel notch, for the modals that scroll.
    pub(in crate::tui) fn scroll_mouse(&mut self, mouse: MouseEvent) {
        let up = match mouse.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
        // A wheel notch is three rows.
        const ROWS: u16 = 3;
        let step = |scroll: u16| {
            if up {
                scroll.saturating_sub(ROWS)
            } else {
                scroll.saturating_add(ROWS)
            }
        };
        match &mut self.foreground {
            Foreground::Keys(_) => self.help_scroll = step(self.help_scroll),
            Foreground::Setup(setup) => setup.wheel(up, ROWS),
            Foreground::Panel => self.panel.detail_scroll = step(self.panel.detail_scroll),
            _ => {}
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

    /// Start a profile's sign-in, with what it asks said first when the tool's own
    /// interface is where it happens.
    ///
    /// A command that signs in and exits needs no page: the terminal is the
    /// sign-in. A tool that has to be driven gets its instructions before its
    /// screen, so the operator arrives knowing what to do there.
    fn begin_sign_in<E>(
        &mut self,
        display: &str,
        profile: &'static str,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        match self
            .profiles
            .iter()
            .find(|summary| summary.profile == profile)
        {
            Some(summary) if summary.sign_in_command.is_interactive() => {
                self.foreground = Foreground::SignIn(Box::new(SignIn::new(
                    summary.display,
                    profile,
                    summary.sign_in,
                )));
                self.alert = None;
            }
            _ => self.open_credential_shell(display, profile, task_service, events),
        }
    }

    /// The keys of the page that says what a sign-in asks.
    pub(in crate::tui) async fn sign_in_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::SignIn(sign_in) = mem::take(&mut self.foreground) else {
            return;
        };
        match sign_in.handle_key(key) {
            None => self.foreground = Foreground::SignIn(sign_in),
            Some(SignInDecision::Cancel) => self.foreground = Foreground::Panel,
            Some(SignInDecision::Open) => {
                self.foreground = Foreground::Panel;
                self.open_credential_shell(&sign_in.display, sign_in.profile, task_service, events);
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
            self.say("A shell is already open");
            return;
        }
        let cancel = CredentialShellCancel::new();
        let id = credential_shell_id();
        // A built environment is a wait of a moment, so it gets no wait to look at:
        // the box appears only when there is an install to watch.
        let progress = (!task_service.credentials().environment_built(profile))
            .then(|| "Preparing your sign-in…".to_owned());
        self.credential_shell = CredentialShellState::Opening {
            id: id.clone(),
            display: display.to_owned(),
            progress,
            cancel: cancel.clone(),
        };
        self.alert = None;
        let service = task_service.clone();
        let events = events.clone();
        tokio::spawn(async move {
            match service
                .credentials()
                .open_shell(id.clone(), profile, cancel, &events)
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
                    let _ = service.credentials().close_shell(shell.id()).await;
                });
            }
            return;
        }
        match event {
            CredentialShellEvent::Preparing { label, .. } => {
                if let CredentialShellState::Opening { progress, .. } = &mut self.credential_shell {
                    *progress = Some(label);
                }
            }
            CredentialShellEvent::Opened(shell) => {
                let CredentialShellState::Opening { display, .. } =
                    mem::replace(&mut self.credential_shell, CredentialShellState::Idle)
                else {
                    // Nothing is waiting for it any more, so it has no reason to run.
                    if let Err(error) = task_service.credentials().close_shell(shell.id()).await {
                        tracing::warn!(%error, "could not remove an unwanted credential shell");
                    }
                    return;
                };
                let id = ShellId::Credentials(shell.id().to_owned());
                self.foreground = Foreground::Shell(id.clone());
                self.alert = None;
                let Some((rows, cols)) = self.shell_size(terminal) else {
                    let service = task_service.clone();
                    tokio::spawn(async move {
                        let _ = service.credentials().close_shell(shell.id()).await;
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
                self.alert = None;
            }
            CredentialShellEvent::Output { id, bytes } => {
                self.process_shell_output(&ShellId::Credentials(id), bytes)
            }
            CredentialShellEvent::Exited { code, .. } => {
                // Leaving the terminal is not an error: a cancelled sign-in and a
                // finished one look the same from here, and the row's next check says
                // what came of it.
                tracing::debug!(code, "a credential shell exited");
                self.close_credential_shell(task_service).await;
            }
            CredentialShellEvent::Failed { error, .. } => {
                self.problem(format!("The shell ended: {error}"));
                self.close_credential_shell(task_service).await;
            }
            CredentialShellEvent::Unavailable { reason, .. } => {
                self.credential_shell = CredentialShellState::Idle;
                self.problem(format!("No shell: {reason}"));
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
            if let Err(error) = service.credentials().close_shell(shell.id()).await {
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
            KeyCode::Tab | KeyCode::BackTab => {
                self.panel.depth = match self.panel.depth {
                    Depth::Rail => Depth::List,
                    Depth::List => Depth::Rail,
                };
            }
            KeyCode::Right | KeyCode::Char('l') => self.panel.depth = Depth::List,
            KeyCode::Left | KeyCode::Char('h') => self.panel.depth = Depth::Rail,
            KeyCode::Down | KeyCode::Char('j') => self.panel.move_selection(true, rows),
            KeyCode::Up | KeyCode::Char('k') => self.panel.move_selection(false, rows),
            KeyCode::Delete | KeyCode::Backspace if self.selected_has_credentials() => {
                self.ask_to_forget_credentials()
            }
            KeyCode::Char(' ') if self.selected_row().is_some_and(|row| row.is_agent()) => {
                self.edit_selected()
            }
            KeyCode::Char('e') if self.panel.section.grows() => self.edit_selected(),
            KeyCode::Char('t')
                if self
                    .selected_row()
                    .is_some_and(|row| row.is_configured_remote()) =>
            {
                if let Some(RowKind::Remote { address, .. }) =
                    self.selected_row().map(|row| row.kind)
                {
                    self.foreground = Foreground::Setup(Setup::tracker_list(address));
                }
            }
            KeyCode::Char('n') if self.panel.section.grows() => {
                self.panel.form = Some(match self.panel.section {
                    Section::Remotes => Form::for_remote(Target::NewRemote, ""),
                    _ => Form::new_project(),
                });
            }
            _ if is_remove(key) && self.selected_row().is_some_and(|row| row.is_removable()) => {
                self.ask_to_delete()
            }
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
        }
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
            // A machine that answered with another identity is asking to be told
            // which machine it is: connecting again would get the same answer.
            Some(RowKind::Remote {
                address, status, ..
            }) => {
                if status == RemoteStatus::Changed {
                    self.foreground = Foreground::Confirm(PendingConfirm::replacing_key(
                        address.clone(),
                        format!(
                            "The machine at {} is not the one this client knew: it answered \
                             with a different identity, as one that was reinstalled or replaced \
                             would. Accept it?",
                            self.remote_name(&address)
                        ),
                    ));
                    return;
                }
                self.check_remote(&address).await
            }
            Some(RowKind::Discovered(remote)) => {
                if let Some(file) = self.settings_file.as_mut() {
                    let mut proposed = file.settings().clone();
                    proposed.add_remote(&remote.address);
                    match file.save(proposed) {
                        Ok(()) => {
                            self.remote_status.remove(&remote.address);
                            self.check_remote(&remote.address).await;
                        }
                        Err(error) => self.problem(error.to_string()),
                    }
                }
            }
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
                        Line::from("Esc or q cancels this sign-in."),
                    ]
                };
                self.foreground = Foreground::Setup(Setup::new(SetupKind::Linear, lines));
                if self.tracker_state != TrackerState::Connected {
                    self.connect_tracker(task_service, tracker_connects);
                }
                self.alert = None;
            }
            Some(RowKind::Agent { credential, .. }) => {
                if let CredentialKind::Profile { profile, .. } = credential.kind {
                    self.begin_sign_in(&credential.label, profile, task_service, events);
                }
            }
            Some(RowKind::GitIdentity(_)) => {
                self.begin_sign_in("Git identity", "vcs/github", task_service, events);
            }
            Some(RowKind::Credential(credential)) => {
                if let CredentialKind::Profile { profile, .. } = credential.kind {
                    self.begin_sign_in(&credential.label, profile, task_service, events);
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

    /// How a remote reads to a human: the name it gave itself, or its address
    /// until it has said one. The same name its picker shows, so a message about
    /// a machine and the list of machines agree.
    fn remote_name(&self, address: &str) -> String {
        self.remembered_keys
            .iter()
            .find(|key| key.address.eq_ignore_ascii_case(address))
            .map_or_else(|| address.to_owned(), |key| key.label.clone())
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
            let name = self.remote_name(&address);
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
                Some(RemoteStatus::Changed) => (format!("{name} (identity changed)"), false),
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

    /// Watch a machine's tasks, for as long as the interface runs.
    ///
    /// The one thing that dials a remote. What the watch finds on the way in is
    /// what the interface shows about the machine, so nothing else has to ask it
    /// the same question on a clock of its own.
    pub(in crate::tui) fn watch_remote(&mut self, address: &str) {
        if self.watched.contains_key(address) {
            return;
        }
        let address = address.to_owned();
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let events = self.watch_events.clone();
        let watching = address.clone();
        let handle = tokio::spawn(async move {
            let mut wait = WATCH_RETRY;
            loop {
                let began = Instant::now();
                match Self::watch_once(&watching, &keys, &client, &events).await {
                    // Nothing is reading any more, which is the interface ending.
                    WatchEnd::Closed => break,
                    // Dialling again will get the same answer until the operator
                    // says which key this machine is.
                    WatchEnd::UnapprovedKey => break,
                    WatchEnd::Lost => {
                        // A watch that ran for a while was working, and its retry
                        // should be quick; one that failed at once backs off.
                        if began.elapsed() > WATCH_SETTLED {
                            wait = WATCH_RETRY;
                        }
                        sleep(wait).await;
                        wait = (wait * 2).min(WATCH_RETRY_MAX);
                    }
                }
            }
        });
        self.watched.insert(address, handle);
    }

    /// Stop watching a machine, which is what forgetting one means.
    pub(in crate::tui) fn unwatch_remote(&mut self, address: &str) {
        if let Some(handle) = self.watched.remove(address) {
            handle.abort();
        }
    }

    /// Watch every machine this client is configured to use.
    pub(in crate::tui) fn watch_remotes(&mut self) {
        for address in self.configured_remotes() {
            self.watch_remote(&address);
        }
    }

    /// Dial one machine, say what it answered, and carry its changes until the
    /// stream ends.
    async fn watch_once(
        address: &str,
        keys: &RemoteKeys,
        client: &LinkClient,
        events: &mpsc::UnboundedSender<WatchedTasks>,
    ) -> WatchEnd {
        let _ = events.send(WatchedTasks::Dialling {
            address: address.to_owned(),
        });
        let remembered = keys.remembered(address).await.ok().flatten();
        let sighting = match meet(address, DEFAULT_PORT, remembered.as_deref()).await {
            Ok(sighting) => sighting,
            Err(error) => {
                let _ = events.send(WatchedTasks::Ended {
                    address: address.to_owned(),
                    reason: error.to_string(),
                });
                return WatchEnd::Lost;
            }
        };
        // What it said is reported before the key is judged, so a machine that
        // answered with a different one can be seen and decided about.
        let key_status = sighting.key_status;
        let _ = events.send(WatchedTasks::Reached {
            address: address.to_owned(),
            announcement: Box::new(sighting.announcement.clone()),
            key_status: key_status.clone(),
        });
        if key_status == RemoteKeyStatus::Changed {
            return WatchEnd::UnapprovedKey;
        }
        let (said, mut receiving) = mpsc::unbounded_channel();
        let watching = client.watch_tasks(sighting.ticket, said);
        tokio::pin!(watching);
        loop {
            tokio::select! {
                ended = &mut watching => match ended {
                    Ok(()) => return WatchEnd::Closed,
                    Err(error) => {
                        let _ = events.send(WatchedTasks::Ended {
                            address: address.to_owned(),
                            reason: error.to_string(),
                        });
                        return WatchEnd::Lost;
                    }
                },
                said = receiving.recv() => {
                    let Some(said) = said else { return WatchEnd::Lost };
                    let address = address.to_owned();
                    let event = match said {
                        WatchEvent::Snapshot(summaries) => WatchedTasks::Snapshot { address, summaries },
                        WatchEvent::Changed(summary) => WatchedTasks::Changed { address, summary },
                        WatchEvent::Removed(id) => WatchedTasks::Removed { address, id },
                        WatchEvent::Ready => WatchedTasks::Ready { address },
                    };
                    if events.send(event).is_err() {
                        return WatchEnd::Closed;
                    }
                }
            }
        }
    }

    /// Take what a watch reported about a machine.
    pub(in crate::tui) async fn tasks_watched(&mut self, event: WatchedTasks) {
        let address = match &event {
            WatchedTasks::Dialling { address }
            | WatchedTasks::Reached { address, .. }
            | WatchedTasks::Ready { address }
            | WatchedTasks::Snapshot { address, .. }
            | WatchedTasks::Changed { address, .. }
            | WatchedTasks::Removed { address, .. }
            | WatchedTasks::Ended { address, .. } => address.clone(),
        };
        // A machine that is no longer configured keeps a watch alive but not a
        // card: forgetting it is what makes its tasks go away.
        if !self.configured_remotes().contains(&address) {
            return;
        }
        match event {
            WatchedTasks::Dialling { address } => {
                self.remote_status.insert(address, RemoteStatus::Asking);
            }
            // It answered, so this is what the interface knows about it: the key,
            // the name it gave, and whether it can be given work.
            WatchedTasks::Reached {
                address,
                announcement,
                key_status,
            } => match key_status {
                RemoteKeyStatus::New | RemoteKeyStatus::Known => {
                    self.remember(&address, &announcement).await;
                    self.note_remote_trackers(&address, &announcement);
                    let status = match announcement.state {
                        RemoteState::Preparing => RemoteStatus::Syncing,
                        RemoteState::Ready => RemoteStatus::Reached,
                    };
                    self.remote_status.insert(address, status);
                    self.reload_remembered_keys().await;
                }
                // Reported, and not written: accepting a changed key means
                // forgetting the address first, which is a second act. Nothing the
                // unaccepted answer said is shown either, and nothing it holds is
                // listened to.
                RemoteKeyStatus::Changed => {
                    self.clear_remote_trackers(&address);
                    self.remote_tasks.retain(|held| held.address != address);
                    self.remote_status.insert(address, RemoteStatus::Changed);
                }
            },
            WatchedTasks::Ready { address } => {
                self.remote_status.insert(address, RemoteStatus::Reached);
            }
            WatchedTasks::Snapshot { address, summaries } => self.holding(address, summaries),
            WatchedTasks::Changed { address, summary } => {
                let held = HeldTask {
                    address: address.clone(),
                    summary: *summary,
                };
                let at = self.remote_tasks.iter().position(|existing| {
                    existing.address == address && existing.summary.id == held.summary.id
                });
                match (at, is_listed_summary(&held.summary)) {
                    (Some(at), true) => self.remote_tasks[at] = held,
                    (Some(at), false) => {
                        self.remote_tasks.remove(at);
                    }
                    (None, true) => self.remote_tasks.push(held),
                    (None, false) => {}
                }
                self.reveal_pending();
            }
            WatchedTasks::Removed { address, id } => {
                self.remote_tasks
                    .retain(|held| !(held.address == address && held.summary.id == id));
                self.settle_selection();
            }
            // Its tasks are its own, and there is no copy here to fall back on.
            WatchedTasks::Ended { address, reason } => {
                self.clear_remote_trackers(&address);
                self.remote_synced.remove(&address);
                self.remote_tasks.retain(|held| held.address != address);
                self.remote_status
                    .insert(address, RemoteStatus::Silent(reason));
                self.settle_selection();
            }
        }
        // A creation question may be open over the list, and a machine that just
        // answered may now be one it can offer.
        self.refresh_task_hosts();
    }

    /// Put one machine's tasks where the others are not, in the order it sent them.
    fn holding(&mut self, address: String, summaries: Vec<TaskSummary>) {
        let at = self
            .remote_tasks
            .iter()
            .position(|held| held.address == address)
            .unwrap_or(self.remote_tasks.len());
        self.remote_tasks.retain(|held| held.address != address);
        let arrived: Vec<HeldTask> = summaries
            .into_iter()
            .filter(is_listed_summary)
            .map(|summary| HeldTask {
                address: address.clone(),
                summary,
            })
            .collect();
        self.remote_tasks.splice(at..at, arrived);
        self.reveal_pending();
        self.settle_selection();
    }

    /// Select the card a machine just said it holds, when one was asked for.
    fn reveal_pending(&mut self) {
        if let Some((address, id)) = self.pending_remote.take()
            && let Some(index) = self
                .remote_tasks
                .iter()
                .position(|held| held.address == address && held.summary.id == id)
        {
            self.selected = self.tasks.len() + index;
        }
    }

    fn settle_selection(&mut self) {
        self.selected = self
            .selected
            .min((self.tasks.len() + self.remote_tasks.len()).saturating_sub(1));
    }

    /// Ask before taking a task over, naming where it is now when it is not here.
    fn ask_to_take_over(&mut self, id: &str, title: &str, from: Option<String>) {
        let question = match from {
            Some(name) => {
                format!("Take over \"{title}\"? It stops on {name} and moves here first.")
            }
            None => format!(
                "Take over \"{title}\"? The agent stops and the terminal is yours; \
                 ctrl-a hands it back."
            ),
        };
        self.foreground = Foreground::Confirm(PendingConfirm::taking_over(id.to_owned(), question));
    }

    /// Move a task here without taking its terminal over.
    ///
    /// The same transfer as taking it over, with the one difference an operator
    /// chooses: the task keeps running where it lands rather than handing the
    /// terminal over with it.
    pub(in crate::tui) fn pull_task<E>(
        &mut self,
        id: &str,
        event_loop: &TaskEventLoop<E>,
        landings: &mpsc::UnboundedSender<Result<String, String>>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if let Some(held) = self
            .remote_tasks
            .iter()
            .find(|held| held.summary.id == id)
            .cloned()
        {
            self.ask_for_task(&held, event_loop, landings, false);
        }
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
        opens_terminal: bool,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let address = held.address.clone();
        let summary = held.summary.clone();
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let event_loop = event_loop.clone();
        let landings = landings.clone();
        self.alert = None;
        let transfer = self.begin_transfer(
            &format!("Pulling task from {}", self.remote_name(&address)),
            &summary.title,
            "packaging…",
            opens_terminal,
        );
        tokio::spawn(async move {
            let outcome =
                bring_home(&address, &summary, &keys, &client, &event_loop, &transfer).await;
            // A take-over runs away from the interface, so where it stopped is a log
            // line rather than only a message that has already been replaced.
            match &outcome {
                Ok(id) => tracing::info!(task = %id, address, "brought a task home"),
                Err(error) => tracing::warn!(%error, address, "could not bring a task home"),
            }
            let _ = landings.send(outcome);
        });
    }

    /// Start watching a transfer, and answer with the moving end of it.
    fn begin_transfer(
        &mut self,
        title: &str,
        label: &str,
        stage: &str,
        opens_terminal: bool,
    ) -> transfer::Transfer {
        let (moving, watching) = transfer::Transfer::start(stage);
        let update = watching.updates.borrow().clone();
        self.transfer = Some(TransferView {
            title: title.to_owned(),
            label: label.to_owned(),
            watching,
            update,
            measured: None,
            rate: None,
            is_stopping: false,
            opens_terminal,
        });
        moving
    }

    /// Read what the transfer in front has to say, saying whether anything moved.
    pub(in crate::tui) fn observe_transfer(&mut self) -> bool {
        self.transfer.as_mut().is_some_and(TransferView::observe)
    }

    /// Ask the transfer in front to stop. It stops between pieces, and what it
    /// has already moved stays where it is.
    pub(in crate::tui) fn stop_transfer(&mut self) {
        if let Some(transfer) = &mut self.transfer {
            transfer.watching.stop();
            transfer.is_stopping = true;
        }
    }

    pub(in crate::tui) fn transfer_progress(
        &self,
    ) -> Option<super::widgets::progress::Progress<'_>> {
        self.transfer.as_ref().map(TransferView::progress)
    }

    /// A task that has arrived is the operator's. Taking it over ends in its
    /// terminal; pulling it here only moves it.
    pub(in crate::tui) fn task_landed<E>(
        &mut self,
        id: &str,
        terminal: &DefaultTerminal,
        event_loop: &TaskEventLoop<E>,
        landings: &mpsc::UnboundedSender<Result<String, String>>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let opens_terminal = self
            .transfer
            .take()
            .is_none_or(|transfer| transfer.opens_terminal);
        if opens_terminal {
            self.take_over(id, terminal, event_loop, landings);
        } else {
            self.say("The task is here");
        }
    }

    /// Ask a remote to run a task of its own, off the event loop.
    ///
    /// Nothing is copied and nothing is kept here: the remote founds the task, and it
    /// is the only machine that can say what the task is doing. The message that it
    /// took the task arrives when the remote says so, not before.
    pub(in crate::tui) fn begin_delegation(
        &mut self,
        address: &str,
        origin: TaskOrigin,
        project_id: &str,
        agent: AgentKind,
        replies: &mpsc::Sender<TaskDelegationReply>,
    ) {
        let identity = match self.config.identity() {
            Ok(identity) => identity,
            Err(error) => {
                self.problem(error.to_string());
                return;
            }
        };
        let address = address.to_owned();
        let project_id = project_id.to_owned();
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result = Self::send_task(
                &address,
                origin,
                &project_id,
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
        project_id: &str,
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
                "{address} is not the machine this client knew; accept its identity \
                 in Settings → Remotes first"
            ));
        }
        // Whose work this is travels as a label. The machine it came from is the
        // key the handshake proved, so the id is not the client's to send.
        client
            .run_task(
                sighting.ticket,
                TaskRequest {
                    origin,
                    project_id: project_id.to_owned(),
                    agent,
                    identity_label,
                },
            )
            .await
            .map_err(|error| error.to_string())
    }

    /// Take what a remote answered when it was asked to run a task.
    pub(in crate::tui) fn task_delegation_resolved(&mut self, reply: TaskDelegationReply) {
        match reply.result {
            Ok(()) => self.say(format!(
                "{} took the task",
                self.remote_name(&reply.address)
            )),
            Err(error) => self.problem(format!(
                "{} did not take it: {error}",
                self.remote_name(&reply.address)
            )),
        }
    }

    /// Ask the machine that holds a task to remove it and dispose of its capsule.
    ///
    /// The same act as removing a task here, done by the machine it belongs to,
    /// so there is nothing to say about it being elsewhere.
    pub(in crate::tui) fn begin_remove_remote(
        &mut self,
        address: &str,
        task_id: &str,
        replies: &mpsc::Sender<RemoveTaskReply>,
    ) {
        let address = address.to_owned();
        let task_id = task_id.to_owned();
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result = Self::remove_remote_task(&address, &task_id, &keys, &client).await;
            let _ = replies
                .send(RemoveTaskReply {
                    address,
                    task_id,
                    result,
                })
                .await;
        });
    }

    async fn remove_remote_task(
        address: &str,
        task_id: &str,
        keys: &RemoteKeys,
        client: &LinkClient,
    ) -> Result<(), String> {
        let peer = Self::remote_peer(address, keys).await?;
        client
            .remove_task(peer, task_id)
            .await
            .map_err(|error| error.to_string())
    }

    /// Take what a machine answered when asked to remove a task of its own.
    pub(in crate::tui) fn remove_remote_resolved(&mut self, reply: RemoveTaskReply) {
        match reply.result {
            Ok(()) => {
                self.remote_tasks.retain(|held| {
                    !(held.address == reply.address && held.summary.id == reply.task_id)
                });
                self.selected = self
                    .selected
                    .min((self.tasks.len() + self.remote_tasks.len()).saturating_sub(1));
                self.say(format!(
                    "Task dropped from {}",
                    self.remote_name(&reply.address)
                ));
            }
            Err(error) => self.problem(format!("Could not drop the task: {error}")),
        }
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
        title: String,
        event_loop: &TaskEventLoop<E>,
        replies: &mpsc::Sender<SendTaskReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        self.alert = None;
        let transfer = self.begin_transfer(
            &format!("Sending task to {}", self.remote_name(&address)),
            &title,
            "packaging…",
            false,
        );
        let config = self.config.clone();
        let keys = self.remote_keys.clone();
        let client = self.link.clone();
        let event_loop = event_loop.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result = send_to_remote(
                &address,
                &task_id,
                &config,
                &keys,
                &client,
                &event_loop,
                &transfer,
            )
            .await;
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
    pub(in crate::tui) fn send_task_resolved(&mut self, reply: SendTaskReply) {
        self.transfer = None;
        match reply.result {
            Ok(task_id) => {
                self.say(format!(
                    "{} took the task",
                    self.remote_name(&reply.address)
                ));
                // It just answered, so it is reachable; recording that lets the
                // read below dial it rather than skip an address the panel had
                // never checked.
                self.remote_status
                    .insert(reply.address.clone(), RemoteStatus::Reached);
                self.pending_remote = Some((reply.address, task_id));
            }
            Err(error) if error == transfer::STOPPED => {
                self.say("Transfer stopped; the task is still here")
            }
            Err(error) => {
                // Not "did not take it": the remote may hold the task while the
                // final acknowledgement was lost, in which case a restart finishes
                // the transfer. What is true here is that it did not finish.
                self.problem(format!("Could not send the task: {error}"));
            }
        }
    }

    /// Take a fresh read of what the remotes hold, and put the highlight on a task
    /// that has just been sent when it appears.
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
                self.panel.select_row(0);
            }
            Some(SendDecision::Send { address }) => {
                self.begin_send_to_remote(
                    address,
                    sending.task_id().to_owned(),
                    sending.title().to_owned(),
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

    pub(in crate::tui) fn discovery_observed(&mut self, event: DiscoveryEvent) {
        let selected = if self.panel.section == Section::Remotes {
            match self.selected_row().map(|row| row.kind) {
                Some(RowKind::Discovered(remote)) => Some(remote.id),
                _ => None,
            }
        } else {
            None
        };
        match event {
            DiscoveryEvent::Found(remote) => {
                self.discovered_remotes.insert(remote.id, remote);
            }
            DiscoveryEvent::Removed(id) => {
                self.discovered_remotes.remove(&id);
            }
        }
        if let Some(id) = selected {
            let index = self
                .settings_view()
                .rows
                .iter()
                .position(|row| matches!(&row.kind, RowKind::Discovered(remote) if remote.id == id))
                .unwrap_or(0);
            self.panel.select_row(index);
        }
    }

    pub(in crate::tui) fn remote_needs_config(&self, address: &str) -> bool {
        // An account someone chose to leave alone, or one being decided about, is
        // not something to keep pushing at.
        let waiting = self.conflict_declined.contains(address)
            || self
                .tracker_conflict
                .as_ref()
                .is_some_and(|conflict| conflict.address == address);
        !waiting
            && self
                .configuration_digest
                .as_ref()
                .is_none_or(|digest| self.remote_synced.get(address) != Some(digest))
    }

    pub(in crate::tui) fn configuration_read(
        &mut self,
        result: Result<ConfigBundle, String>,
        replies: &mpsc::Sender<ConfigPushReply>,
    ) {
        let bundle = match result {
            Ok(bundle) => bundle,
            Err(error) => {
                tracing::warn!(%error, "could not read configuration for remote sync");
                return;
            }
        };
        let digest = match bundle.digest() {
            Ok(digest) => digest,
            Err(error) => {
                tracing::warn!(%error, "could not fingerprint remote configuration");
                return;
            }
        };
        // A configuration that changed may carry a different login, so a question
        // already answered about an account is worth asking again.
        if self.configuration_digest.as_deref() != Some(digest.as_str()) {
            self.conflict_declined.clear();
        }
        self.configuration_digest = Some(digest.clone());
        let bundle = Arc::new(bundle);
        for address in self.configured_remotes() {
            if self.remote_needs_config(&address)
                && !self.remote_config_inflight.contains(&address)
                && matches!(
                    self.remote_status.get(&address),
                    Some(RemoteStatus::Reached)
                )
            {
                self.remote_config_inflight.insert(address.clone());
                self.remote_status
                    .insert(address.clone(), RemoteStatus::Syncing);
                let bundle = bundle.clone();
                let digest = digest.clone();
                let keys = self.remote_keys.clone();
                let client = self.link.clone();
                let replies = replies.clone();
                let take_tracker = self.taking_over.contains(&address);
                tokio::spawn(async move {
                    let result =
                        deliver_config(&address, (*bundle).clone(), &keys, &client, take_tracker)
                            .await;
                    let _ = replies
                        .send(ConfigPushReply {
                            address,
                            digest,
                            result,
                        })
                        .await;
                });
            }
        }
    }

    pub(in crate::tui) fn config_push_resolved(&mut self, reply: ConfigPushReply) {
        if !self.remote_config_inflight.remove(&reply.address)
            || matches!(
                self.remote_status.get(&reply.address),
                Some(RemoteStatus::Changed)
            )
            || !self.configured_remotes().contains(&reply.address)
        {
            return;
        }
        match reply.result {
            Ok(ConfigPush::Stored) => {
                self.taking_over.remove(&reply.address);
                self.conflict_declined.remove(&reply.address);
                self.remote_synced
                    .insert(reply.address.clone(), reply.digest);
                self.remote_status
                    .insert(reply.address, RemoteStatus::Reached);
            }
            // Nothing was written, and the remote is waiting to be told whose the
            // account is.
            Ok(ConfigPush::TrackerLinkedElsewhere {
                provider,
                display_name,
            }) => {
                self.taking_over.remove(&reply.address);
                self.remote_status
                    .insert(reply.address.clone(), RemoteStatus::Reached);
                self.tracker_conflict = Some(TrackerConflict {
                    address: reply.address,
                    provider,
                    display_name,
                });
                self.conflict_choice = 0;
            }
            Err(error) => {
                tracing::warn!(address=%reply.address, %error, "remote configuration sync failed");
                self.remote_status
                    .insert(reply.address, RemoteStatus::Silent(error));
            }
        }
        self.refresh_task_hosts();
    }

    /// Note whether the credentials list is showing, and start its clock when it
    /// appears.
    ///
    /// A fresh look every time it is opened, so a login made in a shell a moment ago
    /// is not still being reported as it was. Says whether anything changed, so a
    /// poll that found nothing does not ask for a frame.
    pub(in crate::tui) fn sync_credentials(&mut self) -> bool {
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
                self.credential_retry_at = None;
                true
            }
            (false, Some(_)) => {
                self.credentials_visible_since = None;
                true
            }
            _ => false,
        }
    }

    /// Whether any profile is being asked about right now, which is what keeps the
    /// dot moving while it waits.
    pub(in crate::tui) fn is_asking_credentials(&self) -> bool {
        self.credential_status
            .values()
            .any(CredentialStatus::is_in_progress)
    }

    /// Whether it is time to ask about this page's logins: the list is showing, it
    /// has been showing long enough to mean it, and nothing is in flight.
    pub(in crate::tui) fn wants_credential_check(&self) -> bool {
        let Some(since) = self.credentials_visible_since else {
            return false;
        };
        since.elapsed() >= ASK_AFTER
            && !self.is_asking_credentials()
            && !self.unasked_profiles().is_empty()
    }

    /// The profiles on this page still to be asked about.
    ///
    /// An answer that came back is not asked again, and one that could not run waits
    /// out a backoff rather than being retried on the next tick.
    fn unasked_profiles(&self) -> Vec<&'static str> {
        let Some(prefix) = self.panel.section.profile_prefix() else {
            return Vec::new();
        };
        let retry_due = self
            .credential_retry_at
            .is_none_or(|at| Instant::now() >= at);
        self.profiles
            .iter()
            .map(|summary| summary.profile)
            .filter(|profile| profile.starts_with(prefix))
            .filter(|profile| match self.credential_status.get(*profile) {
                None => true,
                Some(CredentialStatus::Unanswered) => retry_due,
                Some(_) => false,
            })
            .collect()
    }

    /// Ask this page's profiles about their logins in one sandbox, off the loop.
    ///
    /// One sandbox for the page rather than one per profile: making it is the
    /// expensive part of asking, and a check is a command inside it.
    pub(in crate::tui) fn ask_credential_checks<E>(
        &mut self,
        task_service: &TaskService<E>,
        reports: &mpsc::UnboundedSender<CredentialCheckReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let profiles = self.unasked_profiles();
        if profiles.is_empty() {
            return;
        }
        for profile in &profiles {
            self.credential_status
                .insert(*profile, CredentialStatus::Checking);
        }
        self.credential_retry_at = None;
        let service = task_service.clone();
        let reports = reports.clone();
        let generation = self.credential_check_generation;
        tokio::spawn(async move {
            let answers = match service.credentials().answers(profiles.clone()).await {
                Ok(answers) => answers
                    .into_iter()
                    .map(|(profile, answer)| (profile, credential_status(answer)))
                    .collect(),
                Err(error) => {
                    tracing::debug!(%error, "credential checks unavailable");
                    profiles
                        .into_iter()
                        .map(|profile| (profile, CredentialStatus::Unanswered))
                        .collect()
                }
            };
            let _ = reports.send(CredentialCheckReply {
                generation,
                answers,
            });
        });
    }

    pub(in crate::tui) fn credential_checked(&mut self, reply: CredentialCheckReply) {
        if reply.generation != self.credential_check_generation {
            return;
        }
        let mut nothing_answered = false;
        for (profile, status) in reply.answers {
            nothing_answered |= matches!(status, CredentialStatus::Unanswered);
            self.credential_status.insert(profile, status);
        }
        if nothing_answered {
            self.credential_retry_at = Some(Instant::now() + CHECK_RETRY_AFTER);
        }
    }

    /// Whether any address is being dialled right now, which is what keeps the
    /// dot moving while it waits.
    pub(in crate::tui) fn is_asking_remotes(&self) -> bool {
        self.remote_status
            .values()
            .any(RemoteStatus::is_in_progress)
    }

    /// The keys of the question about a tracker account that is already claimed.
    ///
    /// It holds every key while it is up: nothing behind it acts on a keystroke
    /// meant for it, and there are two answers rather than a way to slip past.
    pub(in crate::tui) fn tracker_conflict_key(&mut self, key: KeyEvent) -> bool {
        let Some(conflict) = &self.tracker_conflict else {
            return false;
        };
        let address = conflict.address.clone();
        match key.code {
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab => self.conflict_choice = 0,
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => self.conflict_choice = 1,
            KeyCode::Enter => {
                if self.conflict_choice == 1 {
                    self.take_over_tracker(&address);
                } else {
                    self.leave_tracker_conflict(&address);
                }
            }
            _ if is_dismiss(key) => self.leave_tracker_conflict(&address),
            _ => {}
        }
        true
    }

    /// Say yes: the next push carries the consent, and the remote stops attributing
    /// the account to whoever had it.
    fn take_over_tracker(&mut self, address: &str) {
        self.tracker_conflict = None;
        self.conflict_declined.remove(address);
        self.taking_over.insert(address.to_owned());
    }

    /// Say no: the account stays with the client that had it, and this machine
    /// stops asking until its configuration changes.
    fn leave_tracker_conflict(&mut self, address: &str) {
        self.tracker_conflict = None;
        self.taking_over.remove(address);
        self.conflict_declined.insert(address.to_owned());
    }

    /// What the question says, in the words of the thing that happened.
    pub(in crate::tui) fn tracker_conflict_question(
        &self,
    ) -> Option<(String, String, Vec<&'static str>)> {
        let conflict = self.tracker_conflict.as_ref()?;
        Some((
            "Tracker account in use".to_owned(),
            format!(
                "Your {} account {} is already linked for another client on {}. Taking it \
                 over makes that machine attribute its work to your access from now on; \
                 leaving it alone sends nothing.",
                conflict.provider.label(),
                conflict.display_name,
                self.remote_name(&conflict.address)
            ),
            vec!["leave it", "take it over"],
        ))
    }

    /// Forget the key remembered for an address, so the key that answered is the
    /// one it is, and start watching it again.
    pub(in crate::tui) async fn accept_remote_key(&mut self, address: &str) {
        if let Err(error) = self.remote_keys.forget(address).await {
            self.problem(format!("Could not accept the new identity: {error}"));
            return;
        }
        self.unwatch_remote(address);
        self.remote_status
            .insert(address.to_owned(), RemoteStatus::Asking);
        self.watch_remote(address);
    }

    /// Ask a machine about itself again, now.
    ///
    /// The watch is what asks a machine anything, so asking again is starting a
    /// fresh one rather than waiting out the backoff. This is also how a changed
    /// key is accepted: forget the one remembered, then ask.
    pub(in crate::tui) async fn check_remote(&mut self, address: &str) {
        self.unwatch_remote(address);
        self.remote_status
            .insert(address.to_owned(), RemoteStatus::Asking);
        self.watch_remote(address);
        self.refresh_task_hosts();
    }

    /// Note what an announcement said about the trackers a remote runs. Called
    /// only for an answer whose key is accepted, so a changed key never lends its
    /// announcement any authority.
    fn note_remote_trackers(&mut self, address: &str, announcement: &Announcement) {
        self.remote_trackers
            .insert(address.to_owned(), announcement.trackers.clone());
    }

    fn clear_remote_trackers(&mut self, address: &str) {
        self.remote_trackers.remove(address);
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

    /// Read back what each address has answered with, which is what the panel and
    /// every host picker show in place of the address.
    pub(in crate::tui) async fn reload_remembered_keys(&mut self) {
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
                self.say("Press enter to configure Git identity and signing");
                return;
            }
            RowKind::Credential(_) => {
                self.say("This is how remotes recognise this machine, and it is not edited here");
                return;
            }
            // A tracker is signed in to with enter, which starts a browser
            // sign-in rather than a form here.
            RowKind::Tracker { .. } => {
                self.say("Press enter to sign in to Linear");
                return;
            }
            RowKind::Project { project } => Form::for_project(
                Target::Project {
                    id: Some(project.id.clone()),
                    name: project.name.clone(),
                },
                &project,
            ),
            RowKind::Discovered(_) => return,
            RowKind::Remote { address, .. } => Form::for_remote(
                Target::Remote {
                    original: address.clone(),
                },
                &address,
            ),
        });
    }

    fn selected_has_credentials(&self) -> bool {
        self.panel.depth != Depth::Rail
            && self
                .selected_row()
                .is_some_and(|row| row.forgets_credentials())
    }

    fn ask_to_forget_credentials(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let (target, question) = match &row.kind {
            RowKind::Agent { .. } | RowKind::Credential(_) => {
                let Some(profile) = row.profile() else { return };
                let scope = if profile == "agents/pi" {
                    "all Pi provider sign-ins"
                } else if profile == "vcs/github" {
                    "the GitHub CLI sign-in"
                } else {
                    "the Claude Code sign-in"
                };
                (
                    CredentialTarget::Profile(profile),
                    format!("Delete {scope} saved on this machine? Tool settings and copies already sent to remotes are kept."),
                )
            }
            RowKind::Tracker { .. } => (CredentialTarget::Linear, "Delete this machine's personal Linear sign-in? The remote's shared Linear app is kept.".to_owned()),
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
                .credentials()
                .forget(profile)
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
                self.say("Saved sign-in deleted");
            }
            Err(error) => self.problem(format!("Could not delete the saved sign-in: {error}")),
        }
    }

    pub(in crate::tui) fn ask_to_delete(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        // The question is built first: it borrows the name the target then moves.
        let (question, target) = match row.kind {
            RowKind::Switch { .. }
            | RowKind::Agent { .. }
            | RowKind::GitIdentity(_)
            | RowKind::Credential(_)
            | RowKind::Tracker { .. }
            | RowKind::Discovered(_) => {
                return;
            }
            RowKind::Project { project } => (
                format!("Delete the {} project?", project.name),
                Target::Project {
                    id: Some(project.id.clone()),
                    name: project.name.clone(),
                },
            ),
            RowKind::Remote { address, .. } => (
                format!(
                    "Delete {address}? This machine forgets it, and the identity it \\
                         answered with."
                ),
                Target::Remote { original: address },
            ),
        };
        self.foreground = Foreground::Confirm(PendingConfirm::deleting(target, question));
    }

    /// Turn a preference on or off, and keep it.
    pub(in crate::tui) fn toggle(&mut self, setting: Setting, on: bool) {
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return;
        };
        let mut proposed = file.settings().clone();
        match setting {
            Setting::KeyboardHints => proposed.set_keyboard_hints(on),
            Setting::Animations => proposed.set_animations(on),
        }
        match file.save(proposed) {
            Ok(()) => self.alert = None,
            Err(error) => self.problem(error.to_string()),
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
            // The chord that starts work on the creation page saves here, so a
            // page with fields is finished the same way from wherever the caret
            // is.
            _ if is_secondary(key) => save = true,
            KeyCode::BackTab => form.move_focus(false),
            KeyCode::Tab => form.move_focus(true),
            KeyCode::Enter | KeyCode::Char(' ') if form.focus == Focus::Accept => {
                form.accepts_delegated_work = !form.accepts_delegated_work;
            }
            KeyCode::Enter if form.enters_newline() => form.handle_text(key),
            KeyCode::Enter => match form.focus {
                Focus::Cancel => keep = false,
                Focus::Save => save = true,
                _ => form.move_focus(true),
            },
            _ => form.handle_text(key),
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
                self.remote_status.remove(&address);
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
            self.say(refusal.to_owned());
            return false;
        }
        // Canonical before the file is borrowed, because a bad URL is the form's
        // answer to give rather than the file's.
        let repository = {
            let value = form.repository.value().trim();
            if value.is_empty() {
                None
            } else {
                match normalize_repository(value) {
                    Ok(repository) => Some(repository),
                    Err(error) => {
                        self.say(error.to_string());
                        return false;
                    }
                }
            }
        };
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return false;
        };
        let mut proposed = file.settings().clone();
        match &form.target {
            Target::NewProject | Target::Project { .. } => {
                let name = form.primary.value().trim().to_owned();
                // The page edits some fields; everything else on the project is
                // carried across so a rename is not a reset.
                let mut project = form
                    .target
                    .project_id()
                    .and_then(|id| proposed.project(id).cloned())
                    .unwrap_or_else(|| Project::new(name.clone()));
                project.name = name;
                project.repository = repository;
                project.setup_command = form.setup.value();
                project.caches = form.cache_paths();
                project.instructions = form.instruction_text();
                let team = form.team.value().trim();
                project.tracker = (!team.is_empty()).then(|| TrackerRoute {
                    team: team.to_owned(),
                    accepts_delegated_work: form.accepts_delegated_work,
                });
                proposed.add_project(project);
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
                self.alert = None;
                true
            }
            Err(error) => {
                self.problem(error.to_string());
                false
            }
        }
    }

    /// Destroy what a confirmation was about.
    pub(in crate::tui) async fn delete(&mut self, target: Target) {
        // Stopping the watch is not the settings file's business, so it happens
        // before the file is borrowed.
        if let Target::Remote { original } = &target {
            self.unwatch_remote(original);
        }
        let Some(file) = self.settings_file.as_mut() else {
            return;
        };
        let mut proposed = file.settings().clone();
        let mut forget_error = None;
        match &target {
            Target::Project { id: Some(id), .. } => proposed.remove_project(id),
            Target::Project { id: None, .. } => {}
            Target::Remote { original } => {
                proposed.remove_remote(original);
                self.remote_synced.remove(original);
                self.remote_status.remove(original);
                self.remote_config_inflight.remove(original);
                self.remote_trackers.remove(original);
                self.remote_tasks
                    .retain(|held| !held.address.eq_ignore_ascii_case(original));
                // A forgotten remote's key is forgotten with it: the next
                // sighting is a first sighting again, which is what forgetting
                // means.
                forget_error =
                    self.remote_keys.forget(original).await.err().map(|error| {
                        format!("could not forget the identity for {original}: {error}")
                    });
            }
            _ => {}
        }
        match file.save(proposed) {
            Ok(()) => {
                let index = self.panel.selected().saturating_sub(1);
                self.panel.select_row(index);
                self.alert = None;
            }
            Err(error) => self.problem(error.to_string()),
        }
        if let Some(message) = forget_error {
            self.problem(message);
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

    /// The thread the screen has selected.
    pub(in crate::tui) fn signal_thread(&self) -> Option<&TaskOperatorSignalThread> {
        self.signal_threads.get(self.signals_cursor.selected())
    }

    /// The task the signals screen is about, when this machine holds it.
    pub(in crate::tui) fn signal_task(&self) -> Option<&Task> {
        let id = self.signal_task_id.as_deref()?;
        self.tasks.iter().find(|task| task.id == id)
    }

    /// The task the signals screen is about, when another machine holds it.
    fn signal_held(&self) -> Option<&HeldTask> {
        let remote = self.remote_signals.as_ref()?;
        self.remote_tasks
            .iter()
            .find(|held| held.address == remote.address && held.summary.id == remote.task_id)
    }

    /// The name the agent is known by here, which is its harness rather than the
    /// word "agent": the operator is reading a conversation, not a category.
    pub(in crate::tui) fn signal_agent_label(&self) -> String {
        self.signal_task()
            .and_then(|task| task.agent_kind)
            .or_else(|| self.signal_held().and_then(|held| held.summary.agent))
            .map_or_else(|| "the agent".to_owned(), |kind| kind.to_string())
    }

    pub(in crate::tui) fn signal_task_title(&self) -> String {
        if let Some(task) = self.signal_task() {
            return task.title().to_owned();
        }
        self.signal_held()
            .map_or_else(String::new, |held| held.summary.title.clone())
    }

    /// The actions the selected thread offers, in footer order.
    fn signal_actions(&self) -> Vec<SignalAction> {
        self.signal_thread().map(actions).unwrap_or_default()
    }

    /// Move the keyboard between the parts of the signals screen.
    pub(in crate::tui) fn move_signal_focus(&mut self, forward: bool) {
        let actions = self.signal_actions().len();
        self.signal_focus = self.signal_focus.next(forward, actions);
        if self.signal_focus == SignalFocus::Detail {
            let len = self
                .signal_thread()
                .map_or(0, |thread| thread.messages.len());
            self.signal_message_cursor = self.signal_message_cursor.min(len.saturating_sub(1));
            self.signal_scroll = if self.signal_message_cursor + 1 == len {
                u16::MAX
            } else {
                u16::try_from(self.signal_message_cursor).unwrap_or(u16::MAX)
            };
        }
    }

    /// Put the selected thread back on its newest message.
    fn reset_signal_message(&mut self) {
        self.signal_message_cursor = self
            .signal_thread()
            .map_or(0, |thread| thread.messages.len().saturating_sub(1));
        self.signal_scroll = u16::MAX;
    }

    /// Move the focused message by one, scrolling the conversation to it.
    pub(in crate::tui) fn move_signal_message(&mut self, forward: bool) {
        let Some(thread) = self.signal_thread() else {
            return;
        };
        let len = thread.messages.len();
        if len == 0 {
            self.signal_message_cursor = 0;
            return;
        }
        self.signal_message_cursor = if forward {
            (self.signal_message_cursor + 1) % len
        } else {
            (self.signal_message_cursor + len - 1) % len
        };
        self.signal_scroll = if self.signal_message_cursor + 1 == len {
            u16::MAX
        } else {
            u16::try_from(self.signal_message_cursor).unwrap_or(u16::MAX)
        };
    }

    /// Keep a focus that a changed thread made impossible from sticking around.
    fn normalise_signal_focus(&mut self) {
        let actions = self.signal_actions().len();
        if matches!(self.signal_focus, SignalFocus::Action(index) if index >= actions) {
            self.signal_focus = SignalFocus::List;
        }
        if self.signal_focus == SignalFocus::Detail {
            let len = self
                .signal_thread()
                .map_or(0, |thread| thread.messages.len());
            self.signal_message_cursor = self.signal_message_cursor.min(len.saturating_sub(1));
            self.signal_scroll = if self.signal_message_cursor + 1 == len {
                u16::MAX
            } else {
                u16::try_from(self.signal_message_cursor).unwrap_or(u16::MAX)
            };
        }
    }

    /// Rebuild the visible threads from the chosen filter and sort.
    ///
    /// The groups keep their order: unresolved first, then the kind tier. The
    /// sort only chooses the direction inside a tier, so a new message can move a
    /// row but cannot move it past the group the operator is reading.
    pub(in crate::tui) fn apply_signal_view(&mut self) {
        let selected = self.signal_thread().map(|thread| thread.id.clone());
        let filter = self
            .signal_filter
            .chosen_value()
            .copied()
            .unwrap_or(SignalFilter::Unresolved);
        let sort = self
            .signal_sort
            .chosen_value()
            .copied()
            .unwrap_or(SignalSort::Oldest);
        let now = chrono::Utc::now().timestamp_millis();
        let mut threads: Vec<_> = self
            .signal_all_threads
            .iter()
            .filter(|thread| filter.matches(thread, now))
            .cloned()
            .collect();
        // A resolved thread that is still lingering in the unresolved view
        // keeps the place it had: the point of the five seconds is that it does
        // not move out from under the operator before they have seen it end.
        let keeping = filter == SignalFilter::Unresolved;
        threads.sort_by(|left, right| {
            let group = |thread: &TaskOperatorSignalThread| {
                let settled = thread.is_resolved() && !(keeping && is_lingering(thread, now));
                // Resolved threads share one RESOLVED heading, so their kind's
                // tier is not a visible group; ordering them by it would put a
                // newer update below an older decision for no reason.
                (
                    u8::from(settled),
                    if settled { 0 } else { thread.kind.tier() },
                )
            };
            group(left).cmp(&group(right)).then_with(|| {
                let when = |thread: &TaskOperatorSignalThread| thread.opened_at_ms;
                match sort {
                    SignalSort::Oldest => when(left).cmp(&when(right)),
                    SignalSort::Newest => when(right).cmp(&when(left)),
                }
            })
        });
        self.signal_threads = threads;
        match selected.as_ref().and_then(|id| {
            self.signal_threads
                .iter()
                .position(|thread| &thread.id == id)
        }) {
            Some(index) => self.signals_cursor.select(index),
            None => self.signals_cursor.select(0),
        }
        self.normalise_signal_focus();
    }

    /// Whether a resolved thread is still being shown in the unresolved view,
    /// either because its five seconds have not run out or because it has not yet
    /// been removed after they did.
    pub(in crate::tui) fn has_lingering_signals(&self) -> bool {
        self.signal_filter.chosen_value() == Some(&SignalFilter::Unresolved)
            && self
                .signal_threads
                .iter()
                .any(TaskOperatorSignalThread::is_resolved)
    }

    /// Rebuild the view once a linger has run out, so the row leaves on time
    /// instead of waiting for the next event.
    pub(in crate::tui) fn expire_signal_linger(&mut self) {
        if self.has_lingering_signals() {
            self.apply_signal_view();
        }
    }

    /// Mark the thread the operator is leaving behind as read, when it is one of
    /// the agent's logs. Reading is navigation, not appearance: the log stays
    /// unresolved until the operator has had the screen on it and moved on.
    async fn read_info_thread<E>(
        &mut self,
        thread: TaskOperatorSignalThread,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if thread.kind != TaskOperatorSignalKind::Update || thread.is_resolved() {
            return;
        }
        if self.remote_signals.is_some() {
            self.act_on_remote_signal(
                thread.id,
                RemoteSignalAction::Resolve(true),
                None,
                remote_signals,
            );
            return;
        }
        match task_service
            .resolve_signal(&thread.task_id, &thread.id, true)
            .await
        {
            Ok(task) => {
                self.resolve_signal_locally(&thread.id, true);
                self.replace_task(task);
            }
            Err(error) => self.problem(error.to_string()),
        }
    }

    async fn read_selected_info<E>(
        &mut self,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if let Some(thread) = self.signal_thread().cloned() {
            self.read_info_thread(thread, task_service, remote_signals)
                .await;
        }
    }

    /// Move between threads, and start the one now selected at its newest
    /// message.
    pub(in crate::tui) async fn move_signal_cursor<E>(
        &mut self,
        forward: bool,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        // Move first, then resolve what was left behind: resolving changes which
        // group the old row belongs to, and the cursor should not be following a
        // row that is moving under it.
        let previous = self.signal_thread().cloned();
        self.signals_cursor
            .move_by(forward, self.signal_threads.len());
        self.reset_signal_message();
        if let Some(thread) = previous {
            self.read_info_thread(thread, task_service, remote_signals)
                .await;
        }
    }

    /// Mark a thread resolved locally; the filter decides whether it stays on
    /// screen. The store keeps the thread either way.
    pub(in crate::tui) fn resolve_signal_locally(&mut self, signal_id: &str, resolved: bool) {
        if let Some(thread) = self
            .signal_all_threads
            .iter_mut()
            .find(|thread| thread.id == signal_id)
        {
            thread.resolved = resolved;
            thread.resolved_by = resolved.then_some(TaskOperatorSignalAuthor::Operator);
            thread.resolved_at_ms = resolved.then(|| chrono::Utc::now().timestamp_millis());
        }
        self.apply_signal_view();
    }

    /// Put the cursor on the next thread that is still unresolved.
    pub(in crate::tui) fn select_next_open_thread(&mut self) {
        let len = self.signal_threads.len();
        if len == 0 {
            return;
        }
        let start = self.signals_cursor.selected();
        for offset in 1..=len {
            let index = (start + offset) % len;
            if self.signal_threads[index].is_unresolved() {
                self.signals_cursor.select(index);
                self.reset_signal_message();
                return;
            }
        }
    }

    /// Read the threads again, keeping the selection on the same thread.
    pub(in crate::tui) async fn reload_signals<E>(&mut self, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        if self.remote_signals.is_some() {
            return;
        }
        let Some(task_id) = self.signal_task_id.clone() else {
            return;
        };
        let Ok(threads) = task_service.signal_threads(&task_id).await else {
            return;
        };
        self.signal_all_threads = threads;
        self.apply_signal_view();
    }

    /// The operator's answer, shown where it will be read: in the thread, at the
    /// bottom, before the agent has come back to it.
    pub(in crate::tui) fn remember_signal_reply(&mut self, signal_id: &str, answer: String) {
        if let Some(thread) = self
            .signal_all_threads
            .iter_mut()
            .find(|thread| thread.id == signal_id)
        {
            thread.messages.push(TaskOperatorSignalMessage {
                id: format!("m{}", thread.messages.len() + 1),
                author: TaskOperatorSignalAuthor::Operator,
                body: answer,
                at_ms: chrono::Utc::now().timestamp_millis(),
            });
            // A message is the whole conversation: it reopens a resolved thread.
            thread.resolved = false;
            thread.resolved_by = None;
            thread.resolved_at_ms = None;
        }
        self.apply_signal_view();
        self.signal_message_cursor = self
            .signal_thread()
            .map_or(0, |thread| thread.messages.len().saturating_sub(1));
        self.signal_scroll = u16::MAX;
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
                self.alert = None;
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

    /// The keys of the inline reply box.
    ///
    /// Enter sends, shift-enter (or alt-enter, where a terminal reports it as its
    /// own key) adds a line, and escape puts the box away without answering. The
    /// reply joins the thread as soon as it is sent, so the screen says what was
    /// said while the agent is still working out what to do about it.
    pub(in crate::tui) fn composer_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if key.code == KeyCode::Esc {
            self.signal_composer = None;
            return;
        }
        let newline = key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
        if key.code == KeyCode::Enter && !newline {
            let Some(composer) = self.signal_composer.take() else {
                return;
            };
            if composer.is_blank() {
                self.signal_composer = Some(composer);
                return;
            }
            let answer = composer.body.value().trim().to_owned();
            let task_id = composer.task_id;
            let signal_id = composer.signal_id;
            if self.remote_signals.is_some() {
                self.act_on_remote_signal(
                    signal_id.clone(),
                    RemoteSignalAction::Reply,
                    Some(answer.clone()),
                    remote_signals,
                );
            } else {
                event_loop.reply_signal(task_id, signal_id.clone(), answer.clone());
            }
            self.remember_signal_reply(&signal_id, answer);
            return;
        }
        if let Some(composer) = self.signal_composer.as_mut() {
            composer.body.handle_key(key);
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
                        self.say("This task is no longer running; control stayed with autopilot");
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
        let answer = question.handle_key(key);
        self.resolve_new_task(
            question,
            answer,
            event_loop,
            task_service,
            issue_reads,
            delegations,
        )
        .await;
    }

    /// What a click on the creation question means, read against the rectangles
    /// the last frame drew.
    pub(in crate::tui) async fn new_task_mouse<E>(
        &mut self,
        mouse: MouseEvent,
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
        let answer = question.mouse(mouse, &self.new_task_targets);
        self.resolve_new_task(
            question,
            answer,
            event_loop,
            task_service,
            issue_reads,
            delegations,
        )
        .await;
    }

    /// Take an answer to the creation question, whether it came from a key or a
    /// click: what a task being founded does is the same either way.
    async fn resolve_new_task<E>(
        &mut self,
        question: Box<NewTask>,
        answer: Option<Answer>,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
        delegations: &mpsc::Sender<TaskDelegationReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        match answer {
            None => self.foreground = Foreground::NewTask(question),
            // The question is dropped, and with it any read that had not yet
            // founded a task.
            Some(Answer::Cancel) => {}
            Some(Answer::Create {
                task,
                project,
                agent,
                host,
                remember,
            }) => {
                // The first answer becomes the default, because there was nothing
                // to default to and guessing is not Altitude's to do.
                if let Some(agent) = remember {
                    self.set_agent(agent);
                }
                // Where this one was founded is where the next one starts.
                self.set_default_task_host(host.as_deref());
                match host {
                    // A task for a remote is founded there, off this loop. Nothing is
                    // copied: the machine that runs it is the machine that owns it.
                    Some(address) => self.begin_delegation(
                        &address,
                        TaskOrigin::Intent(task),
                        &project.id,
                        agent,
                        delegations,
                    ),
                    // Founded here, so whose work it is needs no saying.
                    None => event_loop.create(task, *project, agent, None),
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
        self.config.profile_dir(TrackerKind::Linear.profile())
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
                self.set_default_task_host(submission.host.as_deref());
                match submission.host {
                    Some(address) => self.begin_delegation(
                        &address,
                        TaskOrigin::Issue(Box::new(snapshot)),
                        &submission.project.id,
                        submission.agent,
                        delegations,
                    ),
                    None => {
                        event_loop.create_from_issue(
                            snapshot,
                            submission.project,
                            submission.agent,
                        );
                    }
                }
                self.foreground = Foreground::Tasks;
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
            self.say("Linear sign-in cancelled");
            return;
        }
        let Some(tracker) = task_service.tracker() else {
            self.problem("Linear is not available in this build");
            return;
        };
        if tracker.is_connected() {
            self.tracker_state = TrackerState::Connected;
            self.say("Linear is already connected");
            return;
        }
        self.tracker_state = TrackerState::Connecting;
        self.tracker_connect_attempt = self.tracker_connect_attempt.wrapping_add(1);
        let attempt = self.tracker_connect_attempt;
        // Nothing is said while the browser is open: the row already reads as
        // connecting, and a message in front would have to be dismissed before
        // the operator could carry on.
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
        match reply.result {
            Ok(true) => {
                self.tracker_state = TrackerState::Connected;
                self.say("Linear is connected");
            }
            // The provider answered but reports no live session, which is not a
            // connection whatever else it may have kept.
            Ok(false) => {
                self.tracker_state = TrackerState::NotConnected;
                self.say("Linear did not report a session");
            }
            Err(error) => {
                self.tracker_state = TrackerState::NotConnected;
                self.problem(format!("Linear sign-in did not finish: {error}"));
            }
        }
    }

    /// Whether a sign-in is out, which is what keeps its dot moving.
    pub(in crate::tui) fn is_connecting_tracker(&self) -> bool {
        self.tracker_state == TrackerState::Connecting
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
            self.panel_form_key(key).await;
        } else {
            self.panel_key(key, task_service, events, tracker_connects)
                .await;
        }
    }

    /// Read a mouse event against the signals rectangles the last frame drew.
    pub(in crate::tui) async fn signals_mouse<E>(
        &mut self,
        mouse: MouseEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let target = self.signal_targets.at(mouse.column, mouse.row).copied();
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                if self.signal_filter.is_open() {
                    self.signal_filter.scroll(up);
                    return;
                }
                if self.signal_sort.is_open() {
                    self.signal_sort.scroll(up);
                    return;
                }
                match target {
                    Some(SignalHit::List | SignalHit::Thread(_)) => {
                        self.move_signal_cursor(!up, task_service, remote_signals)
                            .await;
                    }
                    Some(
                        SignalHit::Action(_)
                        | SignalHit::ComposerSend
                        | SignalHit::ComposerCancel
                        | SignalHit::Filter
                        | SignalHit::Sort,
                    ) => {}
                    _ => {
                        self.signal_focus = SignalFocus::Detail;
                        self.move_signal_message(!up);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if (self.signal_filter.is_open() || self.signal_sort.is_open())
                    && !matches!(
                        target,
                        Some(SignalHit::FilterOffer(_) | SignalHit::SortOffer(_))
                    )
                {
                    self.signal_filter.close();
                    self.signal_sort.close();
                    return;
                }
                let Some(target) = target else {
                    return;
                };
                match target {
                    SignalHit::List => self.signal_focus = SignalFocus::List,
                    SignalHit::Thread(index) => {
                        let previous = self.signal_thread().cloned();
                        self.signals_cursor.select(index);
                        self.signal_focus = SignalFocus::List;
                        self.reset_signal_message();
                        if let Some(thread) = previous {
                            self.read_info_thread(thread, task_service, remote_signals)
                                .await;
                        }
                    }
                    SignalHit::Detail => self.signal_focus = SignalFocus::Detail,
                    SignalHit::Message(index) => {
                        self.signal_focus = SignalFocus::Detail;
                        self.signal_message_cursor = index;
                        self.signal_scroll = if self
                            .signal_thread()
                            .is_some_and(|thread| index + 1 == thread.messages.len())
                        {
                            u16::MAX
                        } else {
                            u16::try_from(index).unwrap_or(u16::MAX)
                        };
                    }
                    SignalHit::Action(index) => {
                        self.signal_focus = SignalFocus::Action(index);
                        if let Some(action) = self.signal_actions().get(index).copied() {
                            self.act_on_signal(action, task_service, remote_signals)
                                .await;
                        }
                    }
                    SignalHit::ComposerSend => {
                        self.composer_key(
                            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                            event_loop,
                            remote_signals,
                        );
                    }
                    SignalHit::ComposerCancel => self.signal_composer = None,
                    SignalHit::Filter => {
                        self.signal_focus = SignalFocus::Filter;
                        self.signal_filter.open();
                    }
                    SignalHit::Sort => {
                        self.signal_focus = SignalFocus::Sort;
                        self.signal_sort.open();
                    }
                    SignalHit::FilterOffer(index) => {
                        let before = self.signal_filter.chosen_value().copied();
                        let previous = self.signal_thread().cloned();
                        self.signal_filter.choose(index);
                        self.signal_filter.close();
                        self.apply_signal_view();
                        if self.signal_filter.chosen_value().copied() != before
                            && let Some(thread) = previous
                        {
                            self.read_info_thread(thread, task_service, remote_signals)
                                .await;
                        }
                    }
                    SignalHit::SortOffer(index) => {
                        self.signal_sort.choose(index);
                        self.signal_sort.close();
                        self.apply_signal_view();
                    }
                }
            }
            _ => {}
        }
    }

    /// The keys of the signals screen.
    pub(in crate::tui) async fn signals_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if self.signal_composer.is_some() {
            self.composer_key(key, event_loop, remote_signals);
            return;
        }
        // A dropdown owns every key while it is open, escape included.
        if self.signal_filter.is_open() {
            let before = self.signal_filter.chosen_value().copied();
            let leaving = self.signal_thread().cloned();
            self.signal_filter.handle_key(key);
            self.apply_signal_view();
            if self.signal_filter.chosen_value().copied() != before
                && let Some(thread) = leaving
            {
                self.read_info_thread(thread, task_service, remote_signals)
                    .await;
            }
            return;
        }
        if self.signal_sort.is_open() {
            self.signal_sort.handle_key(key);
            self.apply_signal_view();
            return;
        }
        // Tab and the horizontal arrows move between the parts of the screen.
        // j/k and the vertical arrows move inside the part that has the keys.
        if matches!(
            key.code,
            KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Right
                | KeyCode::Left
                | KeyCode::Char('l')
                | KeyCode::Char('h')
        ) {
            let forward = matches!(key.code, KeyCode::Tab | KeyCode::Right | KeyCode::Char('l'));
            self.move_signal_focus(forward);
            return;
        }
        match key.code {
            KeyCode::PageDown => self.signal_scroll = self.signal_scroll.saturating_add(1),
            KeyCode::PageUp => self.signal_scroll = self.signal_scroll.saturating_sub(1),
            KeyCode::Home => self.signal_scroll = 0,
            KeyCode::End => self.signal_scroll = u16::MAX,
            _ if is_dismiss(key) || key.code == KeyCode::Char('q') => {
                self.read_selected_info(task_service, remote_signals).await;
                self.leave_signals();
            }
            KeyCode::Down | KeyCode::Char('j') => match self.signal_focus {
                SignalFocus::List => {
                    self.move_signal_cursor(true, task_service, remote_signals)
                        .await
                }
                SignalFocus::Detail => self.move_signal_message(true),
                SignalFocus::Action(_) => self.move_signal_focus(true),
                SignalFocus::Filter | SignalFocus::Sort => {}
            },
            KeyCode::Up | KeyCode::Char('k') => match self.signal_focus {
                SignalFocus::List => {
                    self.move_signal_cursor(false, task_service, remote_signals)
                        .await
                }
                SignalFocus::Detail => self.move_signal_message(false),
                SignalFocus::Action(_) => self.move_signal_focus(false),
                SignalFocus::Filter | SignalFocus::Sort => {}
            },
            KeyCode::Char('f') => {
                self.signal_focus = SignalFocus::Filter;
                self.signal_filter.open();
            }
            KeyCode::Char('s') => {
                self.signal_focus = SignalFocus::Sort;
                self.signal_sort.open();
            }
            KeyCode::Enter if is_secondary(key) => {
                if let Some(action) = self.signal_thread().and_then(secondary_action) {
                    self.act_on_signal(action, task_service, remote_signals)
                        .await
                }
            }
            KeyCode::Enter => match self.signal_focus {
                SignalFocus::Filter => self.signal_filter.open(),
                SignalFocus::Sort => self.signal_sort.open(),
                SignalFocus::Action(index) => {
                    if let Some(action) = self.signal_actions().get(index).copied() {
                        self.act_on_signal(action, task_service, remote_signals)
                            .await
                    }
                }
                SignalFocus::List | SignalFocus::Detail => {
                    if let Some(action) = self.signal_thread().and_then(primary_action) {
                        self.act_on_signal(action, task_service, remote_signals)
                            .await
                    }
                }
            },
            _ => {}
        }
    }

    /// Do one of the things a thread offers.
    async fn act_on_signal<E>(
        &mut self,
        action: SignalAction,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(thread) = self.signal_thread().cloned() else {
            return;
        };
        let remote = self.remote_signals.is_some();
        match action {
            // A remote task is not in this list, so it is not looked for there;
            // its reply travels to where it is.
            SignalAction::Reply => {
                if remote || self.tasks.iter().any(|task| task.id == thread.task_id) {
                    self.signal_composer = Some(Composer::new(thread.task_id, thread.id));
                    // The box takes room from the conversation, so the newest
                    // message stays the one just above it.
                    self.signal_scroll = u16::MAX;
                } else {
                    self.say("This task is no longer in the list");
                }
            }
            SignalAction::Resolve => {
                if !matches!(
                    thread.kind,
                    TaskOperatorSignalKind::Update | TaskOperatorSignalKind::Decision
                ) {
                    return;
                }
                // An update is resolved, never reopened: once read it is history.
                let resolved = if thread.kind == TaskOperatorSignalKind::Update {
                    true
                } else {
                    !thread.resolved
                };
                if resolved == thread.resolved {
                    return;
                }
                if remote {
                    self.act_on_remote_signal(
                        thread.id,
                        RemoteSignalAction::Resolve(resolved),
                        None,
                        remote_signals,
                    );
                } else {
                    match task_service
                        .resolve_signal(&thread.task_id, &thread.id, resolved)
                        .await
                    {
                        Ok(task) => {
                            self.resolve_signal_locally(&thread.id, resolved);
                            self.replace_task(task);
                            if resolved {
                                self.select_next_open_thread();
                            }
                        }
                        Err(error) => self.problem(error.to_string()),
                    }
                }
            }
            SignalAction::Accept => {
                if thread.kind != TaskOperatorSignalKind::InReview || thread.resolved {
                    return;
                }
                let question = "Accept this work and close the task?\n\
                    The agent will be shut down and its workspace cleared up."
                    .to_owned();
                self.foreground = Foreground::Confirm(PendingConfirm::confirming_signal(
                    thread.task_id,
                    thread.id,
                    question,
                ));
            }
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
        // A task belongs to the machine running it, so a key does the same thing
        // wherever the card is drawn and only the trip differs: a task on another
        // machine is reached over the link rather than called directly. Taking one
        // over has to move it here first, and its question says so.
        if is_secondary(key) {
            let asked = self.selected_held().map(|held| {
                (
                    held.summary.id.clone(),
                    held.summary.title.clone(),
                    Some(self.remote_name(&held.address)),
                )
            });
            let asked = asked.or_else(|| {
                self.selected_task()
                    .map(|task| (task.id.clone(), task.title().to_owned(), None))
            });
            if let Some((id, title, from)) = asked {
                self.ask_to_take_over(&id, &title, from);
            }
            return Ok(false);
        }

        if self.selected_is_remote() {
            // Moving a task here without its terminal is the one action a card on
            // another machine has that this machine's cards do not.
            if key.code == KeyCode::Char('p')
                && let Some(held) = self.selected_held()
            {
                self.foreground = Foreground::Confirm(PendingConfirm::pulling(
                    held.summary.id.clone(),
                    format!(
                        "Pull \"{}\" here? It stops on {} and keeps running here.",
                        held.summary.title,
                        self.remote_name(&held.address)
                    ),
                ));
                return Ok(false);
            }
            // The same screen this machine's tasks use, for a task whose actions
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
                            self.problem(format!("Could not open {}: {error}", issue.url))
                        }
                    }
                }
                return Ok(false);
            }
        }

        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Char('c') => {
                if self.reload_settings() {
                    self.foreground = Foreground::NewTask(Box::new(NewTask::new(
                        self.projects(),
                        &self.active_project_id(),
                        self.agent(),
                        self.task_hosts(),
                        self.default_task_host().as_deref(),
                    )));
                }
            }
            // Linking an issue is the other way to start work: the same question,
            // asked of the tracker instead of the operator's prose. A chord,
            // because `l` is navigation wherever there is a column to its right.
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.reload_settings() {
                    self.foreground = Foreground::NewTask(Box::new(
                        NewTask::new(
                            self.projects(),
                            &self.active_project_id(),
                            self.agent(),
                            self.task_hosts(),
                            self.default_task_host().as_deref(),
                        )
                        .linking(),
                    ));
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
            KeyCode::Right | KeyCode::Tab => {
                let columns = self.grid_columns();
                self.select_adjacent(1, columns);
            }
            KeyCode::Left | KeyCode::BackTab => {
                let columns = self.grid_columns();
                self.select_adjacent(-1, columns);
            }
            _ if is_remove(key) => {
                // A remote's task is removed the same way. Which machine holds it
                // is where the act happens, not what the act is.
                if let Some(held) = self.selected_held() {
                    let question = format!(
                        "Delete task \"{}\" from {}?",
                        held.summary.title,
                        self.remote_name(&held.address)
                    );
                    self.foreground = Foreground::Confirm(PendingConfirm::removing_remote(
                        held.address.clone(),
                        held.summary.id.clone(),
                        question,
                    ));
                } else if let Some(task) = self.selected_task() {
                    let question = format!("Stop any agents and delete task \"{}\"?", task.title());
                    self.foreground =
                        Foreground::Confirm(PendingConfirm::removing(task.id, question));
                }
            }
            // Enter reads what the task is asking. Taking over has a key of its
            // own, and asks first, because it ends the agent's autonomy.
            KeyCode::Enter => self.open_signals(task_service, remote_signals).await,
            // Moving a task across is one key: which way it goes is decided by
            // where the task already is, so there is nothing to remember.
            KeyCode::Char('p') => {
                if let Some(task) = self.selected_task() {
                    let hosts = self.send_hosts();
                    let title = task.title().to_owned();
                    let environment = task
                        .project()
                        .map(|project| project.name.clone())
                        .unwrap_or_default();
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
                            self.problem(format!("Could not open {}: {error}", issue.url))
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
            self.ask_for_task(&held, event_loop, landings, true);
            return;
        }
        let Some(task) = self.tasks.iter().find(|task| task.id == id).cloned() else {
            return;
        };
        if task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer) {
            self.say("This task is being moved");
            return;
        }
        if matches!(
            task.facts.execution_status,
            TaskExecutionStatus::Paused | TaskExecutionStatus::Failed
        ) {
            event_loop.start_operator_control(task);
            return;
        }
        if !matches!(task.facts.execution_status, TaskExecutionStatus::Running) {
            self.say("This task is not running");
            return;
        }
        if matches!(
            task.facts.execution_mode,
            TaskExecutionMode::Autopilot | TaskExecutionMode::AutopilotReport
        ) {
            if !event_loop.take_control(&task.id) {
                self.say("This task is not running, so there is nothing to take over");
            }
            return;
        }
        // Already the operator's: put its terminal back in front.
        let shell_id = ShellId::Task(task.id.clone());
        if !self.shells.contains_key(&shell_id) && !event_loop.has_active_run(&task.id) {
            self.say("This task has no run to attach to");
            return;
        }
        self.foreground = Foreground::Shell(shell_id.clone());
        self.alert = None;
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
                self.problem(error.to_string());
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
        super::widgets::chrome::content_area(area, &self.hints(), self.keyboard_hints())
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
        let task_id = task.id.clone();
        // The threads are this machine's now, whatever remote was open before.
        self.remote_signals = None;
        self.remote_signals_loading = false;
        let threads = match task_service.signal_threads(&task_id).await {
            Ok(threads) => threads,
            Err(error) => {
                self.problem(error.to_string());
                return;
            }
        };
        self.signal_task_id = Some(task_id);
        self.signal_all_threads = threads;
        self.signal_threads.clear();
        self.signals_cursor = list::Cursor::default();
        self.apply_signal_view();
        self.signal_arrivals.clear();
        self.signal_focus = SignalFocus::List;
        self.signal_scroll = u16::MAX;
        self.signal_composer = None;
        self.foreground = Foreground::Signals;
        self.reset_signal_message();
    }

    /// Open the signals of a task another machine owns, and read them over the
    /// link.
    ///
    /// The screen is shown before the answer arrives, so the operator sees the
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
        self.signal_task_id = Some(remote.task_id.clone());
        self.signal_all_threads.clear();
        self.signal_threads.clear();
        self.signals_cursor = list::Cursor::default();
        self.signal_arrivals.clear();
        self.signal_focus = SignalFocus::List;
        self.signal_message_cursor = 0;
        self.signal_scroll = u16::MAX;
        self.signal_composer = None;
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

    /// Leave the signals screen, forgetting which task it was about so a reply
    /// that lands after this changes nothing.
    fn leave_signals(&mut self) {
        self.foreground = Foreground::Tasks;
        self.remote_signals = None;
        self.remote_signals_loading = false;
        self.signal_task_id = None;
        self.signal_all_threads.clear();
        self.signal_threads.clear();
        self.signal_arrivals.clear();
        self.signal_targets.clear();
        self.signal_composer = None;
        self.signal_focus = SignalFocus::List;
        self.signal_message_cursor = 0;
        self.signal_scroll = 0;
    }

    /// What the screen says when there is no thread to read: the wait while a
    /// remote is being asked, or the task's own last words.
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
        self.signal_task()
            .and_then(|task| task.facts.latest_turn_summary.clone())
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
                "{address} is not the machine this client knew; accept its identity \
                 in Settings → Remotes first"
            ));
        }
        Ok(sighting.ticket.into())
    }

    /// Read one remote task's signals. Info logs resolve where they live when
    /// the operator leaves them, just as they do here.
    async fn read_remote_signals(
        address: &str,
        task_id: &str,
        keys: &RemoteKeys,
        client: &LinkClient,
    ) -> Result<Vec<TaskOperatorSignalThread>, String> {
        let peer = Self::remote_peer(address, keys).await?;
        client
            .signal_threads(peer, task_id)
            .await
            .map_err(|error| error.to_string())
    }

    /// Send one action to the machine that owns the open signals.
    pub(in crate::tui) fn act_on_remote_signal(
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
            RemoteSignalAction::Reply => client
                .reply_operator_signal(peer, &remote.task_id, signal_id, answer.unwrap_or_default())
                .await
                .map_err(|error| error.to_string()),
            RemoteSignalAction::Resolve(resolved) => client
                .resolve_signal(peer, &remote.task_id, signal_id, resolved)
                .await
                .map_err(|error| error.to_string()),
            RemoteSignalAction::Accept => client
                .confirm_operator_signal(peer, &remote.task_id, signal_id)
                .await
                .map_err(|error| error.to_string()),
        }
    }

    /// Take what came back from a remote task's signals. A reply that belongs to
    /// a pop-up the operator has left changes nothing; one that succeeds drops the
    /// resolved signal and re-reads the machine's cards.
    pub(in crate::tui) fn remote_signals_resolved(&mut self, reply: RemoteSignalsReply) {
        match reply {
            RemoteSignalsReply::Opened { remote, signals } => {
                if !self.is_current_remote_signals(&remote) {
                    return;
                }
                self.remote_signals_loading = false;
                match signals {
                    Ok(threads) => {
                        self.signal_all_threads = threads;
                        self.signal_threads.clear();
                        self.signals_cursor = list::Cursor::default();
                        self.apply_signal_view();
                        self.reset_signal_message();
                    }
                    // The screen stays open with what it has, which is nothing
                    // new: the operator can retry by opening it again.
                    Err(error) => self.say(error),
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
                    Ok(()) => match action {
                        RemoteSignalAction::Reply => {}
                        RemoteSignalAction::Resolve(resolved) => {
                            self.resolve_signal_locally(&signal_id, resolved);
                            if resolved {
                                self.select_next_open_thread();
                            }
                        }
                        RemoteSignalAction::Accept => {
                            self.resolve_signal_locally(&signal_id, true);
                            self.say("Task completed");
                        }
                    },
                    Err(error) => self.say(error),
                }
            }
        }
    }

    /// Whether a reply is about the screen that is open right now.
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
            TaskExecutionUpdate::Created(Err(error)) => self.problem(error.to_string()),
            TaskExecutionUpdate::Started(Ok(task)) => self.replace_task(task),
            TaskExecutionUpdate::Started(Err(error)) => self.problem(error.to_string()),
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
                self.alert = None;
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
        // Any news about a task can be news about its threads: the agent may
        // have answered while the screen was open. The read is cheap and the
        // selection is kept by id, so this cannot move the ground under the
        // operator.
        if matches!(self.foreground, Foreground::Signals) {
            self.refresh_signal_threads(event_loop).await;
        }
        Ok(())
    }

    /// Read the threads again, keeping the selection. Resolved ones come back
    /// too, because a conversation that ended is part of what the screen shows.
    pub(in crate::tui) async fn refresh_signal_threads<E>(&mut self, event_loop: &TaskEventLoop<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        if self.remote_signals.is_some() {
            return;
        }
        let Some(task_id) = self.signal_task_id.clone() else {
            return;
        };
        let selected = self.signal_thread().map(|thread| thread.id.clone());
        // What each thread looked like before this read, so a new message or a
        // new ending can be the one that moves on screen.
        let previous: HashMap<String, (usize, bool)> = self
            .signal_all_threads
            .iter()
            .map(|thread| (thread.id.clone(), (thread.messages.len(), thread.resolved)))
            .collect();
        let Ok(threads) = event_loop.signal_threads(&task_id).await else {
            return;
        };
        let grown = selected.as_ref().is_some_and(|id| {
            threads
                .iter()
                .find(|thread| &thread.id == id)
                .is_some_and(|thread| {
                    thread.messages.len() > previous.get(id).map_or(0, |before| before.0)
                })
        });
        let now = Instant::now();
        for thread in &threads {
            let changed = previous.get(&thread.id).is_none_or(|before| {
                before.0 != thread.messages.len() || before.1 != thread.resolved
            });
            if changed {
                self.signal_arrivals.insert(thread.id.clone(), now);
            }
        }
        self.signal_arrivals
            .retain(|id, _| threads.iter().any(|thread| &thread.id == id));
        self.signal_all_threads = threads;
        self.apply_signal_view();
        if grown {
            self.reset_signal_message();
        }
    }

    /// A signal thread changed. Keep the card's unresolved count current, and
    /// read the screen again when it is showing this task.
    pub(in crate::tui) async fn signal_changed<E>(
        &mut self,
        task_id: &str,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if self.tasks.iter().any(|task| task.id == task_id)
            && let Ok(Some(task)) = event_loop.task(task_id).await
        {
            self.replace_task(task);
        }
        if matches!(self.foreground, Foreground::Signals)
            && self.signal_task_id.as_deref() == Some(task_id)
        {
            self.refresh_signal_threads(event_loop).await;
        }
    }

    /// The change notices fell behind. The store has every change, so the open
    /// screen is simply read again.
    pub(in crate::tui) async fn signal_changes_lagged<E>(&mut self, event_loop: &TaskEventLoop<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        if matches!(self.foreground, Foreground::Signals) && self.remote_signals.is_none() {
            self.refresh_signal_threads(event_loop).await;
        }
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
                    Err(error) => self.say(format!("Could not open {url}: {error}")),
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

    /// Whether anything on screen is waiting on work whose progress is worth
    /// reading again while it runs.
    ///
    /// The animation clock reads this too, but it is not the animation's own
    /// question: with animations off there is still a wait to report on.
    pub(in crate::tui) fn is_awaiting_progress(&self) -> bool {
        self.is_preparing()
            || self.is_asking_remotes()
            || self.is_asking_credentials()
            || self.is_awaiting_tracker()
            || self.is_connecting_tracker()
            || matches!(self.credential_shell, CredentialShellState::Opening { .. })
    }
}

/// What one profile's login came to, as the list shows it.
fn credential_status(answer: CredentialAnswer) -> CredentialStatus {
    match (answer.is_verified, answer.signed_in) {
        (true, signed_in) => CredentialStatus::Checked {
            signed_in,
            output: answer.output,
        },
        (false, true) => CredentialStatus::Saved,
        // No login file at all is an answer: there is nothing a task would mount.
        (false, false) => CredentialStatus::Checked {
            signed_in: false,
            output: String::new(),
        },
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
    mut bundle: ConfigBundle,
    keys: &RemoteKeys,
    client: &LinkClient,
    take_tracker: bool,
) -> Result<ConfigPush, String> {
    let remembered = keys.remembered(address).await.ok().flatten();
    let sighting = meet(address, DEFAULT_PORT, remembered.as_deref())
        .await
        .map_err(|error| error.to_string())?;
    if sighting.key_status == RemoteKeyStatus::Changed {
        return Err(format!(
            "{address} is not the machine this client knew; accept its identity in \
             Settings → Remotes first"
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
    bundle.retain_changed_profiles(&held);
    let bytes = serde_json::to_vec(&bundle).map_err(|error| error.to_string())?;
    client
        .put_config(peer, &bytes, take_tracker)
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
    app.profiles = task_service.credentials().profiles();
    app.tracker_state = match task_service.tracker() {
        Some(tracker) if tracker.is_connected() => TrackerState::Connected,
        _ => TrackerState::NotConnected,
    };
    match app.remote_keys.list().await {
        Ok(keys) => app.remembered_keys = keys,
        Err(error) => app.problem(format!("could not read remembered remotes: {error}")),
    }
}

fn show_task_error(app: &mut App, id: &str, error: TaskServiceError) {
    let title = app
        .tasks
        .iter()
        .find(|task| task.id == id)
        .map_or(id, |task| task.title());
    app.problem(format!("{title}: {error}"));
}
