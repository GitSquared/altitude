use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc;

use crate::{
    credentials::{CredentialShellEvent, Slot},
    isolation::IsolationEngine,
    task::TaskService,
    tui::keys::{is_dismiss, is_secondary},
    tui::popups,
    tui::screens,
    tui::widgets::text::Field,
};

use popups::repositories::Key as RepositoriesKey;
use popups::scopes::Key as ScopesKey;
use screens::settings::{Focus, Target};

use super::*;

impl App {
    pub(in crate::tui) async fn panel_keys<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
        repository_lists: &mpsc::UnboundedSender<RepositoryListReply>,
        tracker_scopes: &mpsc::UnboundedSender<TrackerScopesReply>,
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
            // The repository field opens its list from its own keys. A page this
            // long does not get a chord per field: reaching one is what tabbing
            // and the arrows are for.
            let wants_repositories = self.panel.form.as_ref().is_some_and(|form| {
                !form.target.is_remote()
                    && form.repositories.is_none()
                    && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
                    && form.focus == Focus::Repository
            });
            if wants_repositories {
                self.begin_repository_listing(task_service, repository_lists);
                return;
            }
            let scope_credential = self.panel.form.as_ref().and_then(|form| {
                form.tracker_credential().or_else(|| {
                    self.settings_file.as_ref().and_then(|file| {
                        file.settings()
                            .default_credential(Slot::Tracker)
                            .map(str::to_owned)
                    })
                })
            });
            let wants_scopes = matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
                && self.panel.form.as_ref().is_some_and(|form| {
                    form.focus == Focus::Scope && form.scopes.is_none() && form.has_tracker()
                })
                && scope_credential
                    .as_deref()
                    .is_some_and(|id| self.tracker_state_for(id) == TrackerState::Connected);
            if wants_scopes {
                self.begin_scope_listing(task_service, tracker_scopes, scope_credential);
                return;
            }
            self.panel_form_key(key).await;
        } else {
            self.panel_key(key, task_service, events, tracker_connects)
                .await;
        }
    }

    pub(in crate::tui) async fn panel_form_key(&mut self, key: KeyEvent) {
        let Some(mut form) = self.panel.form.take() else {
            return;
        };
        let mut keep = true;
        let mut save = false;
        // An open list holds every key, escape included: it closes the list
        // rather than the page, which is what a dropdown does everywhere else.
        if form.list_open() {
            form.list_key(key);
            self.panel.form = Some(form);
            return;
        }
        if let Some(mut scopes) = form.scopes.take() {
            match scopes.key(key) {
                ScopesKey::Still => {
                    form.scopes = Some(scopes);
                    self.panel.form = Some(form);
                    return;
                }
                ScopesKey::Closed => {
                    self.panel.form = Some(form);
                    return;
                }
                ScopesKey::Chosen(scope) => {
                    let key = if scope.key.is_empty() {
                        scope.id.as_str()
                    } else {
                        scope.key.as_str()
                    };
                    form.scope = Field::with(key);
                    self.panel.form = Some(form);
                    return;
                }
                // A save while the list is open means the form is finished with
                // its choice, so the list closes and the save proceeds.
                ScopesKey::Save => save = true,
            }
        }
        if let Some(browser) = form.repositories.as_mut() {
            match browser.key(key) {
                RepositoriesKey::Still => {
                    self.panel.form = Some(form);
                    return;
                }
                RepositoriesKey::Closed => {
                    form.repositories = None;
                    self.panel.form = Some(form);
                    return;
                }
                RepositoriesKey::Chosen(repository) => {
                    form.repositories = None;
                    form.choose_repository(&repository.url);
                    self.panel.form = Some(form);
                    return;
                }
                RepositoriesKey::Save => {
                    form.repositories = None;
                    save = true;
                }
            }
        } else {
            match key.code {
                // Escape leaves the page from a text box too, so cancelling a form
                // is one gesture rather than a first press that only moves focus.
                KeyCode::Esc => keep = false,
                _ if is_dismiss(key) && !form.focus.takes_text() => keep = false,
                // The chord that starts work on the creation page saves here, so a
                // page with fields is finished the same way from wherever the caret
                // is.
                _ if is_secondary(key) => save = true,
                KeyCode::BackTab => form.move_focus(false),
                KeyCode::Tab => form.move_focus(true),
                // A field that is a pill has no caret to move, so the arrows and
                // the vim keys are travel: they go where tab goes. A one-line box
                // keeps left and right for its text but lets the vertical arrows
                // out; a box with several lines keeps all of them.
                KeyCode::Down if !form.focus.is_multiline() => form.move_focus(true),
                KeyCode::Up if !form.focus.is_multiline() => form.move_focus(false),
                KeyCode::Down | KeyCode::Right | KeyCode::Char('j') | KeyCode::Char('l')
                    if !form.focus.takes_text() =>
                {
                    form.move_focus(true)
                }
                KeyCode::Up | KeyCode::Left | KeyCode::Char('k') | KeyCode::Char('h')
                    if !form.focus.takes_text() =>
                {
                    form.move_focus(false)
                }
                KeyCode::Enter | KeyCode::Char(' ') if form.focus == Focus::Accept => {
                    if form.has_tracker() {
                        form.accepts_delegated_work = !form.accepts_delegated_work;
                    }
                }
                // A pill opens its list, the way a pill does everywhere else.
                // The two whose list is a page of its own are opened before this
                // by the keys that fetch it.
                KeyCode::Enter | KeyCode::Char(' ') if form.focus.is_a_pill() => {
                    form.open_pill();
                }
                KeyCode::Enter if form.enters_newline() => form.handle_text(key),
                KeyCode::Enter => match form.focus {
                    Focus::Cancel => keep = false,
                    Focus::Save => save = true,
                    _ => form.move_focus(true),
                },
                _ => form.handle_text(key),
            }
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
}
