use iroh::EndpointAddr;
use tokio::sync::mpsc;

use crate::{
    config::DEFAULT_PORT,
    remote::{LinkClient, RemoteKeyStatus, RemoteKeys, meet},
    task::TaskOperatorSignalThread,
    tui::screens,
};

use std::path::Path;

use crate::task::picture;
use crate::tui::widgets::{alert::Alert, list};
use screens::signals::SignalFocus;

use super::*;

impl App {
    pub(super) async fn remote_peer(
        address: &str,
        keys: &RemoteKeys,
    ) -> Result<EndpointAddr, String> {
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

    pub(super) async fn collect_remote_pictures(
        address: &str,
        threads: &[TaskOperatorSignalThread],
        keys: &RemoteKeys,
        client: &LinkClient,
        store: &Path,
    ) {
        let ids: Vec<String> = threads
            .iter()
            .flat_map(|thread| thread.messages.iter())
            .flat_map(|message| picture::take(&message.body).1)
            .map(|stored| stored.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let Ok(peer) = Self::remote_peer(address, keys).await else {
            return;
        };
        for id in ids {
            if picture::bytes(store, &id).is_some() {
                continue;
            }
            if let Ok(bytes) = client.picture(peer.clone(), &id).await {
                let _ = picture::store(store, &bytes);
            }
        }
    }

    pub(super) async fn read_remote_signals(
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

    pub(super) fn act_on_remote_input(
        &self,
        remote: RemoteSignals,
        body: String,
        replies: &mpsc::Sender<RemoteSignalsReply>,
    ) {
        let client = self.link.clone();
        let keys = self.remote_keys.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result = async {
                let peer = Self::remote_peer(&remote.address, &keys).await?;
                client
                    .express_input(peer, &remote.task_id, &body)
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            let _ = replies
                .send(RemoteSignalsReply::Sent { remote, result })
                .await;
        });
    }

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

    pub(super) async fn send_remote_signal_action(
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
            RemoteSignalsReply::Sent { remote, result } => {
                if !self.is_current_remote_signals(&remote) {
                    return;
                }
                match result {
                    Ok(()) => {
                        self.alert = Some(Alert::note("Message sent to the agent."));
                    }
                    Err(error) => self.problem(error),
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
                            // Accepting is the end of the task on the machine that
                            // ran it, so the card goes here too, and the list is
                            // where the next one is.
                            self.remote_tasks.retain(|held| {
                                !(held.address == remote.address
                                    && held.summary.id == remote.task_id)
                            });
                            self.clamp_selection();
                            self.leave_signals();
                            self.say("Task completed");
                        }
                    },
                    Err(error) => self.say(error),
                }
            }
        }
    }

    pub(super) fn is_current_remote_signals(&self, remote: &RemoteSignals) -> bool {
        self.remote_signals.as_ref() == Some(remote)
    }

    pub(super) fn open_remote_signals(
        &mut self,
        held: HeldTask,
        replies: &mpsc::Sender<RemoteSignalsReply>,
    ) {
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
        self.signal_plan_open = false;
        self.foreground = Foreground::Signals;
        let client = self.link.clone();
        let keys = self.remote_keys.clone();
        let replies = replies.clone();
        let pictures = self.config.attachments_dir();
        tokio::spawn(async move {
            let signals =
                Self::read_remote_signals(&remote.address, &remote.task_id, &keys, &client).await;
            if let Ok(threads) = &signals {
                // The pictures a remote raised are named by digest; the ones this
                // machine does not hold are fetched before the conversation is shown,
                // so a screen never draws a placeholder it could have had.
                Self::collect_remote_pictures(&remote.address, threads, &keys, &client, &pictures)
                    .await;
            }
            let _ = replies
                .send(RemoteSignalsReply::Opened { remote, signals })
                .await;
        });
    }
}
