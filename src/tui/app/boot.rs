use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use tokio::sync::mpsc;

use crate::{
    config::{Config, SettingsError, SettingsFile},
    remote::{LinkClient, RemoteKeys},
    task::Task,
    tui::screens,
    tui::widgets::{
        focus::FocusRing,
        layers::Behind,
        pointer::Targets,
        select::{Offer, Select},
    },
};

use crate::tui::Pictures;

use crate::tui::background::terrain::Terrain;
use crate::tui::widgets::{alert::Alert, list};
use screens::settings::{GitIdentity, Panel};
use screens::signals::{SignalFilter, SignalFocus, SignalSort};

use super::*;

impl App {
    pub fn new(
        tasks: Vec<Task>,
        config: Config,
        settings: Result<SettingsFile, SettingsError>,
        remote_keys: RemoteKeys,
        link: LinkClient,
        watch_events: mpsc::UnboundedSender<WatchedTasks>,
        pictures: Pictures,
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
            active_project: None,
            tasks_list: list::List::default(),
            task_filter: Select::new(
                "show",
                vec![
                    Offer::new(TaskFilter::Active, "active"),
                    Offer::new(TaskFilter::Failed, "✕ failed"),
                    Offer::new(TaskFilter::NeedsYou, "? needs you"),
                    Offer::new(TaskFilter::ReadyToReview, "◆ ready to review"),
                    Offer::new(TaskFilter::Decisions, "→ decisions"),
                    Offer::new(TaskFilter::Info, "○ info"),
                    Offer::new(TaskFilter::Working, "● working"),
                    Offer::new(TaskFilter::Queued, "◔ queued"),
                    Offer::new(TaskFilter::Limited, "◷ limited"),
                    Offer::new(TaskFilter::Paused, "▮ paused"),
                    Offer::new(TaskFilter::Done, "✓ done"),
                ],
                0,
            ),
            task_sort: Select::new(
                "order",
                vec![
                    Offer::new(TaskSort::Recent, "recent"),
                    Offer::new(TaskSort::Newest, "newest"),
                    Offer::new(TaskSort::Oldest, "oldest"),
                ],
                0,
            ),
            task_control_targets: Targets::default(),
            task_order: Vec::new(),
            pending_reveal: None,
            alert: settings_error.map(Alert::problem),
            removing: HashSet::new(),
            shells: HashMap::new(),
            new_task_targets: Targets::default(),
            project_targets: Targets::default(),
            credential_shell: CredentialShellState::Idle,
            git_identity: GitIdentity::default(),
            commit_edit: None,
            git_identity_over_card: false,
            pictures,
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
            ),
            signal_sort: Select::new(
                "Sort",
                vec![
                    Offer::new(SignalSort::Oldest, "Oldest"),
                    Offer::new(SignalSort::Newest, "Newest"),
                ],
                0,
            ),
            signals_list: list::List::default(),
            signal_scroll: 0,
            signal_message_rows: Vec::new(),
            signal_scroll_max: 0,
            signal_viewport_rows: 0,
            signal_composer: None,
            signal_plan_open: false,
            signal_plan_tabs: false,
            signal_focus: SignalFocus::List,
            signal_message_cursor: 0,
            signal_message_flash: None,
            signal_flash_started: None,
            signal_targets: Targets::default(),
            signal_focus_ring: FocusRing::default(),
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
            shell_behind: Behind::default(),
            transfer: None,
            moving: None,
            superseded: Vec::new(),
            pending_take: None,
            pending_tasks: Vec::new(),
            remote_status: HashMap::new(),
            remote_trackers: HashMap::new(),
            credential_status: HashMap::new(),
            credential_checked_at: HashMap::new(),
            credential_checking: HashSet::new(),
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
            configuration_digests: HashMap::new(),
            remote_config_inflight: HashSet::new(),
            discovered_remotes: HashMap::new(),
            remote_tasks: Vec::new(),
            pending_remote: None,

            remote_keys,
            remembered_keys: Vec::new(),
            base_description: None,
            repositories: None,
            repositories_listing: false,
            repositories_requested: false,
            signing_in: None,
            panel_from_setup: false,
            profiles: Vec::new(),
            tracker_state: TrackerState::NotConnected,
            tracker_states: HashMap::new(),
            tracker_connect: None,
            tracker_connecting: None,
            tracker_connect_attempt: 0,
            environment_builds: HashMap::new(),
            activities: HashMap::new(),
            signal_activity_scroll: u16::MAX,
            signal_activity_scroll_max: 0,
            signal_activity_rows: 0,
            signal_plan_scroll: u16::MAX,
            signal_plan_scroll_max: 0,
            signal_plan_rows: 0,
            shimmer: 0,
            terrain: Terrain::default(),
            help_scroll: 0,
            pressure: None,
        }
    }
}
