use std::{
    collections::{HashMap, HashSet},
    env, mem, process,
    sync::Arc,
    time::{Duration, Instant},
};

use iroh::{EndpointAddr, EndpointId};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    agent::Activity,
    config::{Config, DEFAULT_PORT, SettingsFile},
    credentials::{
        CredentialAnswer, CredentialShell, CredentialShellCancel, CredentialShellEvent,
        credential_shell_id,
    },
    isolation::Pressure,
    isolation::{EnvironmentDescription, EnvironmentProgress, ProfileSummary},
    project::ProjectCommits,
    remote::{
        Announcement, Compatibility, ConfigBundle, ConfigPush, DiscoveredRemote, LinkClient, Phase,
        RemoteKey, RemoteKeyStatus, RemoteKeys, TaskOrigin, TaskSummary, TrackerConnection, meet,
        transfer, transfer::TransferUpdate,
    },
    task::{Task, TaskAsk, TaskExecutionPauseReason, TaskServiceError},
    tracker::{IssueSnapshot, IssueSummary, ScopeSummary, TrackerKind},
    tui::keys::{is_dismiss, is_remove},
    tui::popups,
    tui::screens,
    tui::widgets::{focus::FocusRing, layers::Behind, pointer::Targets, select::Select},
    vcs::Repository,
};

use super::Pictures;

use super::background::terrain::Terrain;
use super::widgets::{alert::Alert, list, progress};
use popups::confirm::PendingConfirm;
use popups::identity::{
    GitIdentityDecision, GitIdentitySetup, IdentityChoice, IdentitySetup, RenameIdentity,
};
use popups::message::MessagePrompt;
use popups::new_task::{Hit as NewTaskHit, NewTask, SearchToken};
use popups::send_task::SendTask;
use popups::setup::Setup;
use popups::sign_in::SignIn;
use screens::get_started::GetStarted;
use screens::settings::{GitIdentity, Panel, SettingsHit};
use screens::shell::{GuestRequest, Shell};
use screens::signals::{Composer, SignalFocus, SignalHit};
use screens::tasks::{Menu, TaskControl, TaskFilter, TaskRef, TaskSort};

#[derive(Default)]
pub(in crate::tui) enum Foreground {
    Keys(Box<Foreground>),
    Confirm(PendingConfirm),
    SendTask(Box<SendTask>),
    NewTask(Box<NewTask>),
    Message(Box<MessagePrompt>),
    Panel,
    Setup(Setup),
    IdentitySetup(Box<IdentitySetup>),
    RenameIdentity(Box<RenameIdentity>),
    GitIdentity(Box<GitIdentitySetup>),
    SignIn(Box<SignIn>),
    GetStarted(Box<GetStarted>),
    Signals,
    Shell(ShellId),
    #[default]
    Tasks,
}

mod identity;
mod panel;
mod shells;

pub(super) use panel::{open_panel, open_panel_at};
mod remotes;

impl Foreground {
    pub(in crate::tui) fn is_screen(&self) -> bool {
        match self {
            Self::Panel | Self::Signals | Self::GetStarted(_) => true,
            Self::Keys(behind) => behind.is_screen(),
            _ => false,
        }
    }

    pub(in crate::tui) fn shows_signals(&self) -> bool {
        match self {
            Self::Signals => true,
            Self::Message(_) => true,
            Self::Confirm(confirm) => confirm.is_signal(),
            Self::Keys(behind) => behind.shows_signals(),
            _ => false,
        }
    }

    pub(in crate::tui) fn shows_get_started(&self) -> bool {
        match self {
            Self::GetStarted(_) => true,
            Self::Keys(behind) => behind.shows_get_started(),
            _ => false,
        }
    }

    pub(in crate::tui) fn shows_settings(&self) -> bool {
        match self {
            Self::Panel
            | Self::Setup(_)
            | Self::IdentitySetup(_)
            | Self::RenameIdentity(_)
            | Self::GitIdentity(_)
            | Self::SignIn(_) => true,
            // A credential shell is opened from the settings list it is about, and
            // goes back to them, so the panel is the screen it sits on.
            Self::Shell(ShellId::Credentials(_)) => true,
            Self::Confirm(confirm) => confirm.is_settings(),
            Self::Keys(behind) => behind.shows_settings(),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tui) enum RemoteStatus {
    Unknown,
    Asking,
    Reached,
    // It is building what it needs to run a task here. Nothing of this machine's is
    // in flight, which is what "syncing" used to say as well. What it is building
    // is the remote's own to say, and an older one says nothing.
    Preparing(Option<Phase>),
    // This machine is sending it the projects that run there, and their logins.
    Syncing,
    Changed,
    Incompatible { local: String, remote: String },
    Silent(String),
}

impl RemoteStatus {
    pub(in crate::tui) fn is_in_progress(&self) -> bool {
        matches!(self, Self::Asking | Self::Preparing(_) | Self::Syncing)
    }
}

/// What this client still owes a remote. The panel and the pusher read the same
/// answer, so what the panel says is being sent is what being sent means: a
/// machine owed nothing was saying "sending setup…" forever, with nothing in
/// flight and nothing to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui) enum Sync {
    /// It holds what this machine would send.
    Nothing,
    /// A configuration, or the revocation of one, is what it is owed.
    Configuration,
    /// It runs none of this machine's projects, so there is nothing to send it.
    NoProjects,
    /// What it holds keeps until the task it is running there is gone.
    UntilItsTaskEnds,
}

fn sandbox_shell_title(task: &Task) -> String {
    format!("bash - {}", task.title())
}

fn task_shell_title(task: &Task) -> String {
    let agent = task
        .agent_kind
        .map_or_else(|| "Agent".to_owned(), |agent| agent.to_string());
    format!("{agent} - {}", task.title())
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(in crate::tui) enum CredentialStatus {
    #[default]
    Unknown,
    Checking,
    Checked {
        signed_in: bool,
        output: String,
    },
    Saved,
    Unanswered,
}

impl CredentialStatus {
    fn is_in_progress(&self) -> bool {
        *self == Self::Checking
    }

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
    pub answers: Vec<(String, CredentialStatus)>,
}

pub(in crate::tui) struct TrackerScopesReply {
    pub result: Result<Vec<ScopeSummary>, String>,
}

pub(in crate::tui) struct RepositoryListReply {
    pub result: Result<Vec<Repository>, String>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::tui) enum TrackerState {
    #[default]
    NotConnected,
    Connecting,
    Connected,
}

impl TrackerState {
    pub(in crate::tui) fn summary(self) -> &'static str {
        match self {
            Self::NotConnected => "not signed in",
            Self::Connecting => "connecting…",
            Self::Connected => "signed in",
        }
    }
}

pub(in crate::tui) struct IssueSearchReply {
    pub token: SearchToken,
    pub result: Result<Vec<IssueSummary>, String>,
}

pub(in crate::tui) struct IssueReadReply {
    pub token: u64,
    pub result: Result<IssueSnapshot, String>,
}

pub(in crate::tui) struct TrackerConnectReply {
    pub attempt: u64,
    pub credential: String,
    pub result: Result<bool, String>,
}

pub(in crate::tui) const NO_TRACKER: &str =
    "No task tracker is signed in; connect one in Settings.";

const ASK_AFTER: Duration = Duration::from_millis(400);

const PENDING_PATIENCE: Duration = Duration::from_secs(60);

const MESSAGE_FLASH: Duration = Duration::from_millis(480);
const MESSAGE_FLASH_BLINK: Duration = Duration::from_millis(120);

const CHECK_RETRY_AFTER: Duration = Duration::from_secs(15);
const CHECK_FRESH_FOR: Duration = Duration::from_secs(10 * 60);
const WATCH_RETRY: Duration = Duration::from_secs(1);
const WATCH_RETRY_MAX: Duration = Duration::from_secs(30);
const WATCH_SETTLED: Duration = Duration::from_secs(5);
// How often a client that is waiting on a remote reads its announcement again. A
// first start is minutes of installing and downloading, and the announcement is
// where the machine says which part it is on.
pub(in crate::tui) const ANNOUNCE_REFRESH: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::tui) enum ShellId {
    Task(String),
    // A bash shell in the same sandbox, beside the agent's terminal and alive for as
    // long as the take-over is.
    TaskShell(String),
    Credentials(String),
}

#[derive(Default)]
pub(in crate::tui) enum CredentialShellState {
    #[default]
    Idle,
    Opening {
        id: String,
        display: String,
        progress: Option<String>,
        cancel: CredentialShellCancel,
    },
    Open(CredentialShell),
}

pub(in crate::tui) struct Moving {
    pub task_id: String,
    pub watching: transfer::Watching,
}

pub(in crate::tui) struct PendingTask {
    pub label: String,
    pub address: Option<String>,
    pub started: Instant,
}

pub(in crate::tui) struct TransferView {
    title: String,
    label: String,
    watching: transfer::Watching,
    update: TransferUpdate,
    measured: Option<(Instant, u64)>,
    rate: Option<f64>,
    is_stopping: bool,
    opens_terminal: bool,
}

impl TransferView {
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

    fn progress(&self) -> progress::Progress<'_> {
        progress::Progress {
            title: &self.title,
            label: &self.label,
            stage: if self.is_stopping {
                "stopping…"
            } else {
                &self.update.stage
            },
            detail: match self.update.unit {
                transfer::Unit::Bytes => progress::Detail::Bytes {
                    done: self.update.done,
                    total: self.update.total,
                    rate: self.rate,
                },
                transfer::Unit::Files => progress::Detail::Files {
                    done: self.update.done,
                    total: self.update.total,
                },
            },
        }
    }
}

pub enum WatchedTasks {
    Dialling {
        address: String,
    },
    Reached {
        address: String,
        announcement: Box<Announcement>,
        key_status: RemoteKeyStatus,
        compatibility: Compatibility,
    },
    /// What the remote is doing now: ready, or building what it needs first.
    Phase {
        address: String,
        phase: Phase,
    },
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
    Ended {
        address: String,
        reason: String,
    },
}

pub(in crate::tui) enum WatchEnd {
    Closed,
    UnapprovedKey,
    Incompatible,
    Lost,
}

#[derive(Clone)]
pub(in crate::tui) struct HeldTask {
    pub(in crate::tui) address: String,
    pub(in crate::tui) summary: TaskSummary,
}

#[derive(Clone, PartialEq, Eq)]
pub(in crate::tui) struct RemoteSignals {
    pub(in crate::tui) address: String,
    pub(in crate::tui) task_id: String,
    pub(in crate::tui) attempt: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum RemoteSignalAction {
    Reply,
    Resolve(bool),
    Accept,
}

#[derive(Clone)]
enum CommitIdentityEdit {
    Installation,
    New,
    Saved(ProjectCommits),
}

pub(in crate::tui) enum RemoteSignalsReply {
    Opened {
        remote: RemoteSignals,
        signals: Result<Vec<TaskAsk>, String>,
    },
    Resolved {
        remote: RemoteSignals,
        signal_id: String,
        action: RemoteSignalAction,
        result: Result<(), String>,
    },
    Sent {
        remote: RemoteSignals,
        result: Result<(), String>,
    },
}

pub(in crate::tui) struct TaskDelegationReply {
    pub(in crate::tui) address: String,
    pub(in crate::tui) result: Result<(), String>,
}

pub(in crate::tui) struct SendTaskReply {
    pub(in crate::tui) address: String,
    pub(in crate::tui) result: Result<String, String>,
}

pub(in crate::tui) struct RemoveTaskReply {
    pub(in crate::tui) address: String,
    pub(in crate::tui) task_id: String,
    pub(in crate::tui) result: Result<(), String>,
}

pub(in crate::tui) struct ConfigPushReply {
    pub(in crate::tui) address: String,
    pub(in crate::tui) digest: String,
    pub(in crate::tui) result: Result<ConfigPush, String>,
}

pub(in crate::tui) struct TrackerConflict {
    address: String,
    provider: TrackerKind,
    display_name: String,
}

mod actions;
mod boot;
mod chrome;
mod commits;
mod credentials;
mod input;
mod naming;
mod projects;
mod proposals;
mod remote_signals;
mod remote_sync;
mod selection;
mod setup;
mod signals;
mod state;
mod tabs;
mod tasks;
mod tracker;

pub struct App {
    pub(in crate::tui) tasks: Vec<Task>,
    active_project: Option<String>,
    pub(in crate::tui) tasks_list: list::List,
    pub(in crate::tui) task_filter: Select<TaskFilter>,
    pub(in crate::tui) task_sort: Select<TaskSort>,
    pub(in crate::tui) task_control_targets: Targets<TaskControl>,
    pub(in crate::tui) task_order: Vec<TaskRef>,
    pub(in crate::tui) pending_reveal: Option<TaskRef>,
    pub(in crate::tui) environment_builds: HashMap<String, EnvironmentProgress>,
    // What each running agent is doing, straight from its own stream. Live only:
    // nothing here outlives the run it describes, and the task screen is the only
    // reader. Capped, because an agent that works for an hour says a lot.
    pub(in crate::tui) activities: HashMap<String, Vec<Activity>>,
    // What this terminal can draw, and the diagrams it has been asked for. Owned
    // here so a picture outlives the frame that first wanted it.
    pub(in crate::tui) pictures: Pictures,
    pub(in crate::tui) signal_activity_scroll: u16,
    pub(in crate::tui) signal_activity_scroll_max: u16,
    pub(in crate::tui) signal_activity_rows: u16,
    pub(in crate::tui) signal_plan_scroll: u16,
    pub(in crate::tui) signal_plan_scroll_max: u16,
    pub(in crate::tui) signal_plan_rows: u16,
    pub(in crate::tui) shimmer: usize,
    pub(in crate::tui) terrain: Terrain,
    pub(in crate::tui) help_scroll: u16,
    pub(in crate::tui) pressure: Option<Pressure>,
    pub(in crate::tui) alert: Option<Alert>,
    pub(in crate::tui) removing: HashSet<String>,
    pub(in crate::tui) shells: HashMap<ShellId, Shell>,
    pub(in crate::tui) new_task_targets: Targets<NewTaskHit>,
    pub(in crate::tui) credential_shell: CredentialShellState,
    git_identity: GitIdentity,
    commit_edit: Option<CommitIdentityEdit>,
    git_identity_over_card: bool,
    pub(in crate::tui) signal_task_id: Option<String>,
    pub(in crate::tui) signal_all_threads: Vec<TaskAsk>,
    pub(in crate::tui) signal_threads: Vec<TaskAsk>,
    pub(in crate::tui) signals_list: list::List,
    pub(in crate::tui) signal_scroll: u16,
    pub(in crate::tui) signal_message_rows: Vec<(u16, u16)>,
    pub(in crate::tui) signal_scroll_max: u16,
    pub(in crate::tui) signal_viewport_rows: u16,
    pub(in crate::tui) signal_composer: Option<Composer>,
    pub(in crate::tui) signal_plan_open: bool,
    pub(in crate::tui) signal_plan_tabs: bool,
    pub(in crate::tui) signal_focus: SignalFocus,
    pub(in crate::tui) signal_message_cursor: usize,
    pub(in crate::tui) signal_message_flash: Option<usize>,
    signal_flash_started: Option<Instant>,
    pub(in crate::tui) signal_targets: Targets<SignalHit>,
    // Rebuilt by every draw of the task screen, so it cannot describe a screen
    // that is no longer on it.
    pub(in crate::tui) signal_focus_ring: FocusRing<SignalFocus>,
    pub(in crate::tui) task_targets: Targets<list::Hit>,
    pub(in crate::tui) project_targets: Targets<usize>,
    pub(in crate::tui) settings_targets: Targets<SettingsHit>,
    pub(in crate::tui) signal_arrivals: HashMap<String, Instant>,
    pub(in crate::tui) remote_signals: Option<RemoteSignals>,
    pub(in crate::tui) remote_signals_loading: bool,
    remote_signals_attempt: u64,
    pub(in crate::tui) config: Config,
    pub(in crate::tui) settings_file: Option<SettingsFile>,
    pub(in crate::tui) link: Arc<LinkClient>,
    pub(in crate::tui) panel: Panel,
    pub(in crate::tui) foreground: Foreground,
    shell_behind: Behind<Foreground>,
    pub(in crate::tui) transfer: Option<TransferView>,
    /// The task that is on its way to another machine, with the progress to show on
    /// its card. One at a time: a handback is something the operator started.
    pub(in crate::tui) moving: Option<Moving>,
    /// What this machine has taken over, by the machine it came from and the id it
    /// had there. A landing is a task of its own with its own id, so the copy the
    /// source still lists cannot be recognised by what it is — only by where it was.
    pub(in crate::tui) superseded: Vec<(String, String)>,
    /// The take over that is under way, if any: hidden from the moment it is asked
    /// for, so the list never shows the task twice.
    pub(in crate::tui) pending_take: Option<(String, String)>,
    pub(in crate::tui) pending_tasks: Vec<PendingTask>,
    pub(in crate::tui) remote_status: HashMap<String, RemoteStatus>,
    pub(in crate::tui) remote_trackers: HashMap<String, Vec<TrackerConnection>>,
    pub(in crate::tui) credential_status: HashMap<String, CredentialStatus>,
    credential_checked_at: HashMap<String, Instant>,
    credential_checking: HashSet<String>,
    credentials_visible_since: Option<Instant>,
    credential_check_generation: u64,
    credential_retry_at: Option<Instant>,
    pub(in crate::tui) remote_tasks: Vec<HeldTask>,
    pending_remote: Option<(String, String)>,
    pub(in crate::tui) tracker_conflict: Option<TrackerConflict>,
    pub(in crate::tui) conflict_choice: usize,
    taking_over: HashSet<String>,
    conflict_declined: HashSet<String>,
    watched: HashMap<String, JoinHandle<()>>,
    watch_events: mpsc::UnboundedSender<WatchedTasks>,
    remote_synced: HashMap<String, String>,
    configuration_digests: HashMap<String, String>,
    remote_config_inflight: HashSet<String>,
    pub(in crate::tui) discovered_remotes: HashMap<EndpointId, DiscoveredRemote>,

    pub(in crate::tui) remote_keys: RemoteKeys,
    pub(in crate::tui) remembered_keys: Vec<RemoteKey>,
    pub(in crate::tui) base_description: Option<EnvironmentDescription>,
    repositories: Option<Vec<Repository>>,
    repositories_listing: bool,
    repositories_requested: bool,
    signing_in: Option<String>,
    panel_from_setup: bool,
    pub(in crate::tui) profiles: Vec<ProfileSummary>,
    pub(in crate::tui) tracker_state: TrackerState,
    tracker_states: HashMap<String, TrackerState>,
    tracker_connect: Option<JoinHandle<()>>,
    tracker_connecting: Option<String>,
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

// What the list is for: work that is still the operator's. An issue that ended elsewhere,
// or a delegation that was withdrawn, is a record rather than a task to act on, and it is
// read where it happened.
// Work that ended somewhere else — the issue was closed, the delegation withdrawn —
// is a record rather than a task to act on, and it is read where it happened. A
// finished task is not that: it stays, so the list can show it when asked.
fn is_listed(task: &Task) -> bool {
    !task
        .facts
        .execution_pause_reason
        .as_ref()
        .is_some_and(TaskExecutionPauseReason::is_out_of_play)
}

fn listed(tasks: Vec<Task>) -> Vec<Task> {
    tasks.into_iter().filter(is_listed).collect()
}

fn is_listed_summary(summary: &TaskSummary) -> bool {
    !summary
        .pause_reason
        .as_ref()
        .is_some_and(TaskExecutionPauseReason::is_out_of_play)
}

// Whether a task is on a project's tab. Work with no project has no other tab to be
// in, so every one of them holds it.
fn belongs_to(project: Option<&str>, tab: &str) -> bool {
    project.is_none_or(|own| own == tab)
}

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

fn origin_label(origin: &TaskOrigin) -> String {
    match origin {
        TaskOrigin::Intent(intent) => first_line(intent),
        TaskOrigin::Issue(issue) => first_line(&issue.issue.title),
    }
}

fn first_line(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default().trim();
    first.chars().take(64).collect()
}

fn copy_to_clipboard(text: String) -> Result<(), arboard::Error> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text)
}

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
    if sighting.compatibility == Compatibility::RequiresUpdate {
        return Err(format!(
            "{} needs an update before it can receive this configuration",
            sighting.announcement.label_or_address(address)
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
        // A machine that answered with a reason is saying something about the
        // build it is running, and that is the first thing an operator needs to
        // know when the two disagree about a login.
        .map_err(|error| {
            format!(
                "{} (v{}) refused the configuration: {error}",
                sighting.announcement.label_or_address(address),
                sighting.announcement.version
            )
        })
}

fn show_task_error(app: &mut App, id: &str, error: TaskServiceError) {
    let title = app
        .tasks
        .iter()
        .find(|task| task.id == id)
        .map_or(id, |task| task.title());
    app.problem(format!("{title}: {error}"));
}
