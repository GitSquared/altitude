use std::fs;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc;

use crate::{
    credentials::Slot,
    isolation::IsolationEngine,
    remote::Announcement,
    task::TaskService,
    tracker::{TrackerAccount, TrackerKind},
    tui::keys::is_dismiss,
};

use super::*;

impl App {
    pub(super) fn take_over_tracker(&mut self, address: &str) {
        self.tracker_conflict = None;
        self.conflict_declined.remove(address);
        self.taking_over.insert(address.to_owned());
    }

    pub(super) fn leave_tracker_conflict(&mut self, address: &str) {
        self.tracker_conflict = None;
        self.taking_over.remove(address);
        self.conflict_declined.insert(address.to_owned());
    }

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

    pub(super) fn note_remote_trackers(&mut self, address: &str, announcement: &Announcement) {
        self.remote_trackers
            .insert(address.to_owned(), announcement.trackers.clone());
    }

    pub(super) fn clear_remote_trackers(&mut self, address: &str) {
        self.remote_trackers.remove(address);
    }

    pub(super) fn is_default_tracker(&self, credential: &str) -> bool {
        self.settings_file
            .as_ref()
            .and_then(|file| file.settings().default_credential(Slot::Tracker))
            == Some(credential)
    }

    pub(super) fn tracker_account_name(&self, credential: &str) -> Option<String> {
        let account = self.profile_account(credential)?;
        let name = account.display_name.trim();
        if name.is_empty() {
            return None;
        }
        Some(match account.workspace.as_deref().map(str::trim) {
            Some(workspace) if !workspace.is_empty() => format!("{name}@{workspace}"),
            _ => name.to_owned(),
        })
    }

    pub(super) fn connect_tracker_for<E>(
        &mut self,
        credential: &str,
        task_service: &TaskService<E>,
        replies: &mpsc::UnboundedSender<TrackerConnectReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if let Some(handle) = self.tracker_connect.take() {
            handle.abort();
            self.tracker_connect_attempt = self.tracker_connect_attempt.wrapping_add(1);
            if let Some(previous) = self.tracker_connecting.take() {
                self.tracker_states
                    .insert(previous, TrackerState::NotConnected);
            }
            self.say("Tracker sign-in cancelled");
            return;
        }
        let Some(tracker) = task_service.tracker_connectable(credential) else {
            self.problem("This tracker identity is not available");
            return;
        };
        if tracker.is_connected() {
            self.tracker_states
                .insert(credential.to_owned(), TrackerState::Connected);
            if self.is_default_tracker(credential) {
                self.tracker_state = TrackerState::Connected;
            }
            self.say("This tracker is already connected");
            return;
        }
        self.tracker_states
            .insert(credential.to_owned(), TrackerState::Connecting);
        if self.is_default_tracker(credential) {
            self.tracker_state = TrackerState::Connecting;
        }
        self.tracker_connecting = Some(credential.to_owned());
        self.tracker_connect_attempt = self.tracker_connect_attempt.wrapping_add(1);
        let attempt = self.tracker_connect_attempt;
        let credential = credential.to_owned();
        // Nothing is said while the browser is open: the row already reads as
        // connecting, and a message in front would have to be dismissed before
        // the operator could carry on.
        let replies = replies.clone();
        self.tracker_connect = Some(tokio::spawn(async move {
            let result = match tracker.connect().await {
                Ok(()) => Ok(tracker.is_connected()),
                Err(error) => Err(error.to_string()),
            };
            let _ = replies.send(TrackerConnectReply {
                attempt,
                credential,
                result,
            });
        }));
    }

    pub(in crate::tui) fn connect_tracker<E>(
        &mut self,
        task_service: &TaskService<E>,
        replies: &mpsc::UnboundedSender<TrackerConnectReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let kind = TrackerKind::sole();
        self.ensure_credential(kind.profile(), kind.label());
        self.connect_tracker_for(kind.profile(), task_service, replies);
    }

    pub(in crate::tui) fn tracker_connect_resolved(&mut self, reply: TrackerConnectReply) {
        if self.tracker_connecting.as_deref() != Some(reply.credential.as_str())
            || reply.attempt != self.tracker_connect_attempt
        {
            return;
        }
        self.tracker_connect = None;
        self.tracker_connecting = None;
        let state = match reply.result {
            Ok(true) => {
                self.say("Tracker identity connected");
                if let Some(name) = self.tracker_account_name(&reply.credential) {
                    let moved = self.adopt_identity_name(&reply.credential, &name);
                    if moved != reply.credential {
                        self.tracker_states
                            .insert(reply.credential.clone(), TrackerState::NotConnected);
                        self.tracker_states
                            .insert(moved.clone(), TrackerState::Connected);
                        if self.is_default_tracker(&moved) {
                            self.tracker_state = TrackerState::Connected;
                        }
                        return;
                    }
                }
                TrackerState::Connected
            }
            // The provider answered but reports no live session, which is not a
            // connection whatever else it may have kept.
            Ok(false) => {
                self.say("The tracker did not report a session");
                TrackerState::NotConnected
            }
            Err(error) => {
                self.problem(format!("Tracker sign-in did not finish: {error}"));
                TrackerState::NotConnected
            }
        };
        self.tracker_states.insert(reply.credential.clone(), state);
        if self.is_default_tracker(&reply.credential) {
            self.tracker_state = state;
        }
    }

    pub(in crate::tui) fn is_connecting_tracker(&self) -> bool {
        self.tracker_state == TrackerState::Connecting
            || self
                .tracker_states
                .values()
                .any(|state| *state == TrackerState::Connecting)
    }

    pub(in crate::tui) fn is_awaiting_tracker(&self) -> bool {
        match &self.foreground {
            Foreground::NewTask(question) => question.is_awaiting(),
            _ => false,
        }
    }

    pub(in crate::tui) fn tracker_credential(&self) -> Option<String> {
        let settings = self.settings_file.as_ref()?.settings();
        if let Some(default) = settings.default_credential(Slot::Tracker) {
            return Some(default.to_owned());
        }
        let mut trackers = settings
            .credentials()
            .iter()
            .filter(|credential| {
                credential
                    .service()
                    .is_some_and(|service| service.slot() == Slot::Tracker)
            })
            .map(|credential| credential.id.clone());
        match (trackers.next(), trackers.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    }

    pub(in crate::tui) fn tracker_state_for(&self, id: &str) -> TrackerState {
        self.tracker_states
            .get(id)
            .copied()
            .unwrap_or(TrackerState::NotConnected)
    }

    pub(in crate::tui) fn refresh_tracker_state<E>(&mut self, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        self.tracker_states.clear();
        for credential in self.settings_credentials() {
            if credential
                .service()
                .is_some_and(|service| service.slot() == Slot::Tracker)
            {
                // A tracker that answers at all is one with a saved sign-in: that is
                // what this row says, and it does not wait for a session to be opened.
                let state = match task_service.tracker_for(&credential.id) {
                    Some(_) => TrackerState::Connected,
                    None => TrackerState::NotConnected,
                };
                self.tracker_states.insert(credential.id.clone(), state);
            }
        }
        let default = self.settings_file.as_ref().and_then(|file| {
            file.settings()
                .default_credential(Slot::Tracker)
                .map(str::to_owned)
        });
        self.tracker_state = default
            .as_deref()
            .and_then(|id| self.tracker_states.get(id).copied())
            .unwrap_or(TrackerState::NotConnected);
    }

    pub(in crate::tui) fn tracker_scopes_listed(&mut self, reply: TrackerScopesReply) {
        let Some(form) = self.panel.form.as_mut() else {
            return;
        };
        let Some(scopes) = form.scopes.as_mut() else {
            return;
        };
        match reply.result {
            Ok(scopes_list) => scopes.fill(&scopes_list),
            Err(error) => scopes.failed(error),
        }
    }

    pub(in crate::tui) fn profile_account(&self, credential: &str) -> Option<TrackerAccount> {
        let text =
            fs::read_to_string(self.config.profile_dir(credential).join("identity.json")).ok()?;
        serde_json::from_str(&text).ok()
    }
}
