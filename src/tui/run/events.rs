use super::*;

use std::{
    io::{self},
    mem,
};

use crossterm::event::{Event, KeyCode, MouseEventKind};
use ratatui::DefaultTerminal;

use crate::{
    isolation::IsolationEngine,
    task::{TaskEventLoop, TaskService},
};

use crate::tui::App;
use crate::tui::app::Foreground;
use crate::tui::keys::{is_dismiss, is_help};
use crate::tui::popups::confirm::handle_confirm_key;

pub(super) async fn handle_event<E>(
    event: Event,
    terminal: &DefaultTerminal,
    event_loop: &TaskEventLoop<E>,
    task_service: &TaskService<E>,
    app: &mut App,
    replies: &EventReplies<'_>,
) -> io::Result<bool>
where
    E: IsolationEngine + Clone + 'static,
{
    let EventReplies {
        landings,
        credentials: credential_events,
        issues: issue_reads,
        tracker_connects,
        remote_signals,
        delegations,
        sends,
        removals,
        repositories,
        tracker_scopes,
    } = replies;
    if let Event::Resize(_, _) = event
        && app.open_shell().is_some()
    {
        let Some((rows, cols)) = app.shell_size(terminal) else {
            return Ok(false);
        };
        app.resize_shell(rows, cols, event_loop);
        return Ok(false);
    }
    if let Event::Paste(text) = event {
        if app.alert.is_some() {
            return Ok(false);
        }
        if let Some(composer) = &mut app.signal_composer {
            composer.body.paste(&text);
        } else if let Foreground::NewTask(new_task) = &mut app.foreground {
            new_task.paste(&text);
        } else if let Foreground::GetStarted(panel) = &mut app.foreground {
            if panel.commits.open {
                match panel.commits.field {
                    super::screens::get_started::CommitsField::Name => {
                        panel.commits.name.paste(&text)
                    }
                    super::screens::get_started::CommitsField::Email => {
                        panel.commits.email.paste(&text)
                    }
                    _ => {}
                }
            } else if panel.focus == super::screens::get_started::Focus::Name {
                panel.name.paste(&text);
            }
            panel.agent.paste(&text);
            panel.vcs.paste(&text);
            panel.tracker.paste(&text);
        } else if let Some(form) = &mut app.panel.form {
            // A list that is open is narrowed by what is pasted; otherwise the
            // text goes to the field the caret is in.
            match &mut form.repositories {
                Some(browser) => browser.select.paste(&text),
                None => form.paste(&text),
            }
        } else if let Some(id) = app.open_shell().cloned()
            && let Some(bytes) = app
                .shells
                .get(&id)
                .and_then(|shell| shell.paste_bytes(text))
        {
            app.send_shell_input(&id, bytes, event_loop);
        }
        return Ok(false);
    }
    if let Event::Mouse(mouse) = event {
        // Movement alone says nothing has been chosen or scrolled: the loop has
        // already declined to draw for it.
        if mouse.kind == MouseEventKind::Moved {
            return Ok(false);
        }
        // A message holds the pointer too: a click meant for the box behind it is
        // not a click on the list.
        if app.alert.is_some() {
            return Ok(false);
        }
        if matches!(app.foreground, Foreground::NewTask(_)) {
            app.new_task_mouse(mouse, event_loop, task_service, issue_reads, delegations)
                .await;
            return Ok(false);
        }
        if matches!(app.foreground, Foreground::Signals) {
            app.signals_mouse(mouse, remote_signals, event_loop).await;
            return Ok(false);
        }
        if matches!(app.foreground, Foreground::Tasks) {
            app.task_mouse(mouse);
            return Ok(false);
        }
        if matches!(app.foreground, Foreground::Panel) {
            app.panel_mouse(mouse);
            return Ok(false);
        }
        if matches!(app.foreground, Foreground::GetStarted(_)) {
            app.started_mouse(mouse);
            return Ok(false);
        }
        if let Some(id) = app.open_shell().cloned() {
            let size = app.shell_area(terminal.size()?.into());
            let bytes = app
                .shells
                .get_mut(&id)
                .and_then(|shell| shell.mouse_bytes(mouse, size));
            if let Some(bytes) = bytes {
                app.send_shell_input(&id, bytes, event_loop);
            }
        } else {
            app.scroll_mouse(mouse);
        }
        return Ok(false);
    }
    let Event::Key(key) = event else {
        return Ok(false);
    };
    if !key.is_press() {
        return Ok(false);
    }
    // A tracker account that is already claimed stops everything until it is
    // answered: it decides whose work the machine attributes.
    if app.tracker_conflict_key(key) {
        return Ok(false);
    }
    // A message is in front of the work, so it holds every key until it is read:
    // nothing behind it acts on a keystroke meant for it.
    if app.alert.is_some() {
        if is_dismiss(key) || matches!(key.code, KeyCode::Enter) {
            app.dismiss_alert();
        }
        return Ok(false);
    }
    // A transfer in flight owns the keys: it is the only thing on screen, and it
    // has one answer.
    if app.transfer.is_some() {
        if is_dismiss(key) {
            app.stop_transfer();
        }
        return Ok(false);
    }
    // Help owns its scrolling keys; actions behind it stay untouched.
    if matches!(app.foreground, Foreground::Keys(_)) {
        app.help_key(key);
        return Ok(false);
    }
    // A terminal is not the interface: `?` is something the program in it may
    // read, so help is not offered over one.
    if is_help(key) && !app.is_typing() && app.open_shell().is_none() {
        app.help_scroll = 0;
        app.foreground = Foreground::Keys(Box::new(mem::take(&mut app.foreground)));
        return Ok(false);
    }
    // A menu lies over the screen it belongs to, so it is asked for the key before the
    // screen is: a screen that reads a key first takes one aimed at the menu above it.
    if app.menu_key(key) {
        return Ok(false);
    }
    // In the order the states are declared: whatever is in front takes the key,
    // and the line under it says which one that is.
    match &app.foreground {
        Foreground::Keys(_) => app.dismiss_keys(),
        Foreground::Confirm(_) => {
            handle_confirm_key(
                app,
                terminal,
                event_loop,
                key,
                landings,
                removals,
                remote_signals,
            )
            .await
        }
        Foreground::SendTask(_) => {
            app.send_task_key(key, event_loop, task_service, sends)
                .await
        }
        Foreground::NewTask(_) => {
            app.new_task_key(key, event_loop, task_service, issue_reads, delegations)
                .await
        }
        Foreground::GetStarted(_) => {
            if app
                .started_key(
                    key,
                    task_service,
                    credential_events,
                    repositories,
                    tracker_connects,
                )
                .await
            {
                return Ok(true);
            }
        }
        Foreground::Message(_) => app.message_key(key, event_loop, remote_signals).await,
        Foreground::Panel => {
            app.panel_keys(
                key,
                task_service,
                credential_events,
                tracker_connects,
                repositories,
                tracker_scopes,
            )
            .await
        }
        Foreground::IdentitySetup(_) => {
            app.identity_setup_key(key, task_service, credential_events, tracker_connects)
                .await
        }
        Foreground::RenameIdentity(_) => app.rename_identity_key(key),
        Foreground::GitIdentity(_) => app.git_identity_key(key).await,
        Foreground::Setup(_) => app.setup_key(key),
        Foreground::SignIn(_) => app.sign_in_key(key, task_service, credential_events).await,
        Foreground::Signals => app.signals_key(key, remote_signals, event_loop).await,
        Foreground::Shell(_) => app.shell_key(key, terminal, event_loop, task_service).await,
        Foreground::Tasks => {
            if app.list_key(key, task_service, remote_signals).await? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
