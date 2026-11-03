use std::{collections::HashMap, sync::Arc};

use tokio::sync::mpsc;

use crate::{
    config::DEFAULT_PORT,
    remote::{ConfigBundle, ConfigPush, DiscoveryEvent, Phase, RemoteState, meet},
    tui::screens,
};

use screens::settings::{RowKind, Section};

use super::*;

impl App {
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

    /// Ask those machines what they are doing now. A first start is minutes of
    /// installing and downloading, and the announcement is where a machine says
    /// which part it is on. A watch says "ready" once, to whoever is listening at
    /// that moment, so a client waiting on a machine reads the announcement again
    /// rather than sitting on the word it got before the machine moved on.
    pub(in crate::tui) fn refresh_remotes(&self) {
        let addresses = self.remotes_to_refresh();
        if addresses.is_empty() {
            return;
        }
        let events = self.watch_events.clone();
        let keys = self.remote_keys.clone();
        tokio::spawn(async move {
            for address in addresses {
                let remembered = keys.remembered(&address).await.ok().flatten();
                let Ok(sighting) = meet(&address, DEFAULT_PORT, remembered.as_deref()).await else {
                    continue;
                };
                let phase = match sighting.announcement.phase {
                    Some(phase) => phase,
                    // Too old to say which part it is on: the watch is already
                    // carrying the one word it does have.
                    None if sighting.announcement.state == RemoteState::Ready => Phase::Ready,
                    None => continue,
                };
                let event = WatchedTasks::Phase { address, phase };
                if events.send(event).is_err() {
                    return;
                }
            }
        });
    }

    /// What this machine still owes a remote, if anything.
    pub(in crate::tui) fn sync_for(&self, address: &str) -> Sync {
        if !self.remote_needs_config(address) {
            return Sync::Nothing;
        }
        if self.remote_runs_projects(address) {
            return Sync::Configuration;
        }
        // Nothing of this machine's runs there. What it was given is taken back
        // once the task it is running is gone, so that is a configuration too.
        if self.remote_tasks.iter().any(|held| held.address == address) {
            return Sync::UntilItsTaskEnds;
        }
        if self.remote_was_given_config(address) {
            return Sync::Configuration;
        }
        Sync::NoProjects
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
                .configuration_digests
                .get(address)
                .is_none_or(|digest| self.remote_synced.get(address) != Some(digest))
    }

    pub(in crate::tui) fn configuration_read(
        &mut self,
        result: Result<Vec<(String, ConfigBundle)>, String>,
        replies: &mpsc::Sender<ConfigPushReply>,
    ) {
        let bundles = match result {
            Ok(bundles) => bundles,
            Err(error) => {
                tracing::warn!(%error, "could not read configuration for remote sync");
                return;
            }
        };
        let mut digests = HashMap::new();
        for (address, bundle) in &bundles {
            match bundle.digest() {
                Ok(digest) => {
                    digests.insert(address.clone(), digest);
                }
                Err(error) => {
                    tracing::warn!(%error, "could not fingerprint remote configuration");
                    return;
                }
            }
        }
        // A configuration that changed may carry a different login, so a question
        // already answered about an account is worth asking again. Only the
        // machines whose own configuration changed are asked.
        for (address, digest) in &digests {
            if self.configuration_digests.get(address) != Some(digest) {
                self.conflict_declined.remove(address);
            }
        }
        self.configuration_digests = digests;
        let bundles: HashMap<String, Arc<ConfigBundle>> = bundles
            .into_iter()
            .map(|(address, bundle)| (address, Arc::new(bundle)))
            .collect();
        for address in self.configured_remotes() {
            let (Some(bundle), Some(digest)) = (
                bundles.get(&address).cloned(),
                self.configuration_digests.get(&address).cloned(),
            ) else {
                continue;
            };
            // A machine that still has a task of ours keeps its copy until the task
            // is no longer there: its logins are what the task runs with, and the
            // bundle that would take them away is not sent yet.
            if bundle.remove_logins && self.remote_tasks.iter().any(|held| held.address == address)
            {
                continue;
            }
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
                Some(RemoteStatus::Changed | RemoteStatus::Incompatible { .. })
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
    }

    pub(in crate::tui) fn is_asking_remotes(&self) -> bool {
        self.remote_status
            .values()
            .any(RemoteStatus::is_in_progress)
    }

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

    pub(in crate::tui) async fn check_remote(&mut self, address: &str) {
        self.unwatch_remote(address);
        self.remote_status
            .insert(address.to_owned(), RemoteStatus::Asking);
        self.watch_remote(address);
    }

    pub(in crate::tui) async fn reload_remembered_keys(&mut self) {
        match self.remote_keys.list().await {
            Ok(keys) => self.remembered_keys = keys,
            Err(error) => tracing::error!(%error, "could not read the remembered remotes"),
        }
    }
}
