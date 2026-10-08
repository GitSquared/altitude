//! The terminal loop: events in, and one frame out.
//!
//! Everything here is about the terminal rather than about the work: reading
//! events, routing them to whatever is in front, and drawing. What the
//! application does with an event is its own business, in `app`.

use std::{io, mem, time::Duration};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream,
    },
    execute,
};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::{signal::unix::Signal, sync::mpsc, time};

use crate::{
    isolation::IsolationEngine,
    task::{TaskEventLoop, TaskExecutionUpdate, TaskService},
};

use super::App;
use super::app::Foreground;
use super::keys::is_help;
use super::popups;
use super::popups::confirm::handle_confirm_key;
use super::screens;
use super::screens::settings::Focus;
use super::widgets;

pub async fn run<E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    event_loop: TaskEventLoop<E>,
    task_service: TaskService<E>,
    app: App,
    task_execution_updates: mpsc::UnboundedReceiver<TaskExecutionUpdate>,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    let result = run_loop(
        terminal,
        terminate,
        interrupt,
        event_loop.clone(),
        task_service,
        app,
        task_execution_updates,
    )
    .await;
    let mouse_cleanup = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    event_loop.suspend_all().await.map_err(io::Error::other)?;
    mouse_cleanup?;
    result
}

async fn run_loop<E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    event_loop: TaskEventLoop<E>,
    task_service: TaskService<E>,
    mut app: App,
    mut task_execution_updates: mpsc::UnboundedReceiver<TaskExecutionUpdate>,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    execute!(io::stdout(), EnableBracketedPaste)?;
    let mut events = EventStream::new();
    let mut refresh = time::interval(Duration::from_secs(2));
    let mut shell_notice = time::interval(Duration::from_millis(100));
    let mut shimmer = time::interval(Duration::from_millis(80));
    let mut mouse_captured = false;
    // Reaching an address is the one thing here that waits on another machine, so
    // it happens off this loop and reports back like a task's run does.
    let (remote_answers, mut answered) = mpsc::unbounded_channel();
    let mut ask = time::interval(Duration::from_millis(250));
    loop {
        if mouse_captured != app.open_shell().is_some() {
            mouse_captured = app.open_shell().is_some();
            if mouse_captured {
                execute!(io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(io::stdout(), DisableMouseCapture)?;
            }
        }
        terminal.draw(|frame| {
            let hints = app.hints();
            let content =
                widgets::chrome::draw(frame, &hints, app.keyboard_hints(), app.notice.as_deref());
            screens::tasks::draw(frame, content, &mut app);
            if let Some(shell) = app.open_shell().and_then(|id| app.shells.get(id)) {
                shell.draw(frame, content);
            }
            if matches!(app.foreground, Foreground::Panel) {
                // Fullscreen: it is the screen, not something over it.
                let view = app.settings_view();
                screens::settings::draw(frame, content, &app.panel, &view, app.shimmer);
            } else if matches!(app.foreground, Foreground::Signals) {
                widgets::chrome::dim(frame, content);
                popups::signals::draw(
                    frame,
                    content,
                    &app.operator_signals,
                    app.selected_task()
                        .and_then(|task| task.facts.latest_turn_summary)
                        .as_deref(),
                    app.selected_operator_signal,
                    app.selected_signal_action,
                    &mut app.operator_signal_scroll,
                );
            }
            if let Foreground::Confirm(confirm) = &app.foreground {
                widgets::chrome::dim(frame, content);
                confirm.draw(frame, content);
            }
            if let Foreground::NewTask(new_task) = &app.foreground {
                widgets::chrome::dim(frame, content);
                popups::new_task::draw(frame, content, new_task);
            }
            if matches!(app.foreground, Foreground::Keys(_)) {
                widgets::chrome::dim(frame, content);
                widgets::chrome::draw_help(frame, content, &hints);
            }
            // Input belongs above the settings overlay that opened it.
            if let Foreground::Reply(reply) = &app.foreground {
                widgets::chrome::dim(frame, content);
                reply.prompt.draw(frame, content);
            }
        })?;
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = refresh.tick(), if app.open_shell().is_none() => {
                if let Ok(tasks) = event_loop.list().await {
                    app.tasks = tasks;
                    app.selected = app.selected.min(app.tasks.len().saturating_sub(1));
                }
                app.capacity = Some(event_loop.capacity_limit());
            }
            _ = ask.tick() => {
                app.sync_remotes();
                if app.wants_remote_check() {
                    app.ask_next_remote(&remote_answers).await;
                }
            }
            Some(answer) = answered.recv() => app.remote_answered(answer).await,
            _ = shell_notice.tick(), if app.shell_has_active_notice() => {}
            _ = shimmer.tick(), if app.is_preparing() || app.is_asking_remotes() => {
                app.shimmer = app.shimmer.wrapping_add(1);
                app.environment_builds = task_service.environment_builds();
            }
            Some(update) = task_execution_updates.recv() =>
                app.apply(update, terminal, &event_loop).await?,
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                if handle_event(event?, terminal, &event_loop, &task_service, &mut app).await? { return Ok(()); }
            }
        }
    }
}

async fn handle_event<E>(
    event: Event,
    terminal: &DefaultTerminal,
    event_loop: &TaskEventLoop<E>,
    task_service: &TaskService<E>,
    app: &mut App,
) -> io::Result<bool>
where
    E: IsolationEngine + Clone + 'static,
{
    if let Event::Resize(_, _) = event
        && let Some(id) = app.open_shell().map(str::to_owned)
    {
        let (rows, cols) = screens::shell::size(terminal.size()?.into());
        event_loop.resize_terminal(&id, rows, cols);
        if let Some(shell) = app.shells.get_mut(&id) {
            shell.resize(rows, cols);
        }
        return Ok(false);
    }
    if let Event::Paste(text) = event {
        if let Foreground::Reply(reply) = &mut app.foreground {
            reply.prompt.paste(&text);
        } else if let Foreground::NewTask(new_task) = &mut app.foreground {
            new_task.paste(&text);
        } else if let Some(form) = &mut app.panel.form {
            match form.focus {
                Focus::Primary => form.primary.paste(&text),
                Focus::Setup => form.setup.paste(&text),
                _ => {}
            }
        } else if let Some(id) = app.open_shell().map(str::to_owned)
            && let Some(shell) = app.shells.get(&id)
            && let Some(bytes) = shell.paste_bytes(text)
        {
            event_loop.send_terminal_input(&id, bytes);
        }
        return Ok(false);
    }
    if let Event::Mouse(mouse) = event {
        if let Some(id) = app.open_shell().map(str::to_owned)
            && let Some(shell) = app.shells.get_mut(&id)
            && let Some(bytes) = shell.mouse_bytes(mouse, terminal.size()?.into())
        {
            event_loop.send_terminal_input(&id, bytes);
        }
        return Ok(false);
    }
    let Event::Key(key) = event else {
        return Ok(false);
    };
    if !key.is_press() {
        return Ok(false);
    }
    // Anything at all closes help: a help screen is not somewhere to be.
    if matches!(app.foreground, Foreground::Keys(_)) {
        app.dismiss_keys();
        return Ok(false);
    }
    if is_help(key) && !app.is_typing() {
        app.foreground = Foreground::Keys(Box::new(mem::take(&mut app.foreground)));
        return Ok(false);
    }
    // In the order the states are declared: whatever is in front takes the key,
    // and the line under it says which one that is.
    match &app.foreground {
        Foreground::Keys(_) => app.dismiss_keys(),
        Foreground::Confirm(_) => handle_confirm_key(app, terminal, event_loop, key).await,
        Foreground::Reply(_) => app.reply_key(key, event_loop),
        Foreground::NewTask(_) => app.new_task_key(key, event_loop),
        Foreground::Panel => app.panel_keys(key).await,
        Foreground::Signals => app.signals_key(key, task_service).await,
        Foreground::Shell(_) => app.shell_key(key, event_loop),
        Foreground::Tasks => {
            if app.list_key(key, task_service).await? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
