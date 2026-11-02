use std::time::Instant;

use tokio::sync::mpsc;

use crossterm::event::KeyEvent;
use ratatui::DefaultTerminal;
use tokio::time::sleep;

use crate::{
    agent::AgentKind,
    config::DEFAULT_PORT,
    isolation::IsolationEngine,
    remote::{
        Compatibility, LOCAL_VERSION, LinkClient, RemoteKeyStatus, RemoteKeys, RemoteState,
        TaskOrigin, TaskRequest, TaskSummary, WatchEvent, bring_home, meet, send::send_to_remote,
    },
    task::{TaskEventLoop, TaskService},
    tui::widgets::progress,
};

use super::{
    App, Foreground, HeldTask, PendingTask, RemoteStatus, RemoveTaskReply, SendTaskReply,
    TaskDelegationReply, TransferView, WATCH_RETRY, WATCH_RETRY_MAX, WATCH_SETTLED, WatchEnd,
    WatchedTasks, is_listed_summary, mem, open_panel, origin_label,
    popups::confirm::PendingConfirm, popups::new_task::Host,
    popups::send_task::Decision as SendDecision, screens::settings::Section, transfer,
};

impl App {
    pub(in crate::tui) fn remote_name(&self, address: &str) -> String {
        self.remembered_keys
            .iter()
            .find(|key| key.address.eq_ignore_ascii_case(address))
            .map_or_else(|| address.to_owned(), |key| key.label.clone())
    }

    pub(in crate::tui) fn task_hosts(&self) -> Vec<Host> {
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
                Some(RemoteStatus::Incompatible { .. }) => {
                    (format!("{name} (update required)"), false)
                }
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
                    // Retrying cannot resolve an identity or version mismatch.
                    WatchEnd::UnapprovedKey | WatchEnd::Incompatible => break,
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

    pub(in crate::tui) fn unwatch_remote(&mut self, address: &str) {
        if let Some(handle) = self.watched.remove(address) {
            handle.abort();
        }
    }

    pub(in crate::tui) fn watch_remotes(&mut self) {
        for address in self.configured_remotes() {
            self.watch_remote(&address);
        }
    }

    pub(in crate::tui) async fn watch_once(
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
        let compatibility = sighting.compatibility;
        let _ = events.send(WatchedTasks::Reached {
            address: address.to_owned(),
            announcement: Box::new(sighting.announcement.clone()),
            key_status: key_status.clone(),
            compatibility,
        });
        if key_status == RemoteKeyStatus::Changed {
            return WatchEnd::UnapprovedKey;
        }
        if compatibility == Compatibility::RequiresUpdate {
            return WatchEnd::Incompatible;
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
                compatibility,
            } => match key_status {
                RemoteKeyStatus::New | RemoteKeyStatus::Known => {
                    self.remember(&address, &announcement).await;
                    if compatibility == Compatibility::RequiresUpdate {
                        self.clear_remote_trackers(&address);
                        self.remote_synced.remove(&address);
                        self.remote_status.insert(
                            address.clone(),
                            RemoteStatus::Incompatible {
                                local: LOCAL_VERSION.to_owned(),
                                remote: announcement.version.clone(),
                            },
                        );
                    } else {
                        self.note_remote_trackers(&address, &announcement);
                        let status = match announcement.state {
                            RemoteState::Preparing => RemoteStatus::Syncing,
                            RemoteState::Ready => RemoteStatus::Reached,
                        };
                        self.remote_status.insert(address, status);
                    }
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
            // They stay on screen anyway: a machine that restarts still holds the
            // work it was running, and a card that vanishes and comes back reads as
            // a task that ended. The machine's own row is what says it is silent.
            WatchedTasks::Ended { address, reason } => {
                self.clear_remote_trackers(&address);
                self.remote_synced.remove(&address);
                self.remote_status
                    .insert(address, RemoteStatus::Silent(reason));
                self.settle_selection();
            }
        }
        // A creation question may be open over the list, and a machine that just
        // answered may now be one it can offer.
    }

    pub(in crate::tui) fn holding(&mut self, address: String, summaries: Vec<TaskSummary>) {
        // A take over is done when the machine it came from stops listing the task:
        // until then the record here is the only card, and after it there is nothing
        // left to hide.
        self.superseded.retain(|(from, taken)| {
            from != &address || summaries.iter().any(|summary| &summary.id == taken)
        });
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

    pub(in crate::tui) fn reveal_pending(&mut self) {
        let Some((address, id)) = self.pending_remote.take() else {
            return;
        };
        let project_id = self
            .remote_tasks
            .iter()
            .find(|held| held.address == address && held.summary.id == id)
            .and_then(|held| held.summary.project_id.clone());
        if let Some(project_id) = project_id {
            self.focus_project(&project_id);
        }
        let (local, _) = self.visible_counts();
        let index = self
            .visible_remote_tasks()
            .position(|held| held.address == address && held.summary.id == id);
        if let Some(index) = index {
            self.selected = local + index;
        }
    }

    pub(in crate::tui) fn settle_selection(&mut self) {
        let (local, remote) = self.visible_counts();
        self.selected = self.selected.min((local + remote).saturating_sub(1));
    }

    pub(in crate::tui) fn ask_to_take_over(
        &mut self,
        id: &str,
        title: &str,
        agent: &str,
        from: Option<String>,
    ) {
        let question = match from {
            Some(_) => format!("Take over {title} and speak directly to {agent}?"),
            None => format!("Take over {title} and speak directly to {agent}?"),
        };
        self.foreground = Foreground::Confirm(PendingConfirm::taking_over(id.to_owned(), question));
    }

    pub(in crate::tui) fn ask_for_task<E>(
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
        // Asked for is as good as taken: the source keeps listing the task until it
        // lets go, and the operator has just decided where it lives.
        self.pending_take = Some((address.clone(), summary.id.clone()));
        self.alert = None;
        let transfer = self.begin_transfer(
            &format!("Taking over: {}", summary.title),
            &self.remote_name(&address),
            "collecting the working files…",
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

    pub(in crate::tui) fn begin_transfer(
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

    pub(in crate::tui) fn watch_transfer(
        &mut self,
        title: String,
        label: String,
        watching: transfer::Watching,
    ) {
        let update = watching.updates.borrow().clone();
        self.transfer = Some(TransferView {
            title,
            label,
            watching,
            update,
            measured: None,
            rate: None,
            is_stopping: false,
            opens_terminal: false,
        });
    }

    pub(in crate::tui) fn observe_transfer(&mut self) -> bool {
        self.transfer.as_mut().is_some_and(TransferView::observe)
    }

    pub(in crate::tui) fn stop_transfer(&mut self) {
        if let Some(transfer) = &mut self.transfer {
            transfer.watching.stop();
            transfer.is_stopping = true;
        }
    }

    pub(in crate::tui) fn transfer_progress(&self) -> Option<progress::Progress<'_>> {
        self.transfer.as_ref().map(TransferView::progress)
    }

    /// The take over that was under way finished, or it did not: either way it is no
    /// longer pending, and a task that arrived stays hidden on the machine it left.
    pub(in crate::tui) fn settle_take(&mut self, arrived: bool) {
        let Some((address, id)) = self.pending_take.take() else {
            return;
        };
        if arrived {
            self.superseded.push((address, id));
        }
    }

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
        }
    }

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
        self.pending_tasks.push(PendingTask {
            label: origin_label(&origin),
            address: Some(self.remote_name(&address)),
            started: Instant::now(),
        });
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

    pub(in crate::tui) async fn send_task(
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
        if sighting.compatibility == Compatibility::RequiresUpdate {
            return Err(format!(
                "{} needs an update before it can take tasks",
                sighting.announcement.label_or_address(address)
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

    pub(in crate::tui) fn task_delegation_resolved(&mut self, reply: TaskDelegationReply) {
        // The machine has taken it, so the row that stood in for the task has done
        // its job: the task itself arrives in the list it was sent to.
        let machine = self.remote_name(&reply.address);
        self.pending_tasks
            .retain(|pending| pending.address.as_deref() != Some(machine.as_str()));
        // Nothing is said when it works: the task the remote founded is in the
        // list, so a modal saying so is a keypress to dismiss what is already on
        // the screen. A refusal is the operator's to act on.
        if let Err(error) = reply.result {
            self.problem(format!(
                "{} did not take it: {error}",
                self.remote_name(&reply.address)
            ));
        }
    }

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

    pub(in crate::tui) async fn remove_remote_task(
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

    pub(in crate::tui) fn remove_remote_resolved(&mut self, reply: RemoveTaskReply) {
        match reply.result {
            Ok(()) => {
                self.remote_tasks.retain(|held| {
                    !(held.address == reply.address && held.summary.id == reply.task_id)
                });
                let (local, remote) = self.visible_counts();
                self.selected = self.selected.min((local + remote).saturating_sub(1));
            }
            Err(error) => self.problem(format!("Could not drop the task: {error}")),
        }
    }

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
            &format!("Handing back to {}", self.remote_name(&address)),
            &title,
            "collecting the working files…",
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

    pub(in crate::tui) fn send_task_resolved(&mut self, reply: SendTaskReply) {
        self.transfer = None;
        match reply.result {
            Ok(task_id) => {
                // It just answered, so it is reachable; recording that lets the
                // read below dial it rather than skip an address the panel had
                // never checked.
                self.remote_status
                    .insert(reply.address.clone(), RemoteStatus::Reached);
                self.pending_remote = Some((reply.address, task_id));
            }
            // Stopped is what the operator asked for, and the task is visibly
            // theirs still; saying so would only be read a second time.
            Err(error) if error == transfer::STOPPED => {}
            Err(error) => {
                // Not "did not take it": the remote may hold the task while the
                // final acknowledgement was lost, in which case a restart finishes
                // the transfer. What is true here is that it did not finish.
                self.problem(format!("Could not send the task: {error}"));
            }
        }
    }

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
}
