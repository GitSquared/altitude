//! The terminal loop: events in, and one frame out.
//!
//! Everything here is about the terminal rather than about the work: reading
//! events, routing them to whatever is in front, and drawing. What the
//! application does with an event is its own business, in `app`.

use std::{io, mem, time::Duration};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
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
use super::app::{
    ConfigPushReply, Foreground, HeldTask, IssueReadReply, IssueSearchReply, NO_LINEAR,
    RemoteSignalsReply, SendTaskReply, TaskDelegationReply, TrackerConnectReply,
};
use super::keys::is_help;
use super::popups;
use super::popups::confirm::handle_confirm_key;
use super::screens;
use super::screens::settings::Focus;
use super::widgets;
use crate::task::CredentialShellEvent;

struct EventReplies<'a> {
    landings: &'a mpsc::UnboundedSender<Result<String, String>>,
    credentials: &'a mpsc::UnboundedSender<CredentialShellEvent>,
    issues: &'a mpsc::UnboundedSender<IssueReadReply>,
    tracker_connects: &'a mpsc::UnboundedSender<TrackerConnectReply>,
    remote_signals: &'a mpsc::Sender<RemoteSignalsReply>,
    delegations: &'a mpsc::Sender<TaskDelegationReply>,
    sends: &'a mpsc::Sender<SendTaskReply>,
    config_pushes: &'a mpsc::Sender<ConfigPushReply>,
}

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
    // Given back whether or not it was asked for: a terminal that never took it
    // ignores the sequence, and one that did must not be left holding it.
    let terminal_cleanup = execute!(
        io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags
    );
    event_loop.suspend_all().await.map_err(io::Error::other)?;
    terminal_cleanup?;
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
    // Whether the terminal is reporting the keys the creation question needs.
    let mut keys_enhanced = false;
    // Reaching an address is the one thing here that waits on another machine, so
    // it happens off this loop and reports back like a task's run does.
    let (remote_answers, mut answered) = mpsc::unbounded_channel();
    let mut ask = time::interval(Duration::from_millis(250));
    // Reading a remote's tasks is a dial, so it happens on its own clock rather than
    // every frame, and away from this loop.
    let (remote_task_reads, mut remote_tasks_read) = mpsc::unbounded_channel::<Vec<HeldTask>>();
    // Bringing a task home is a dial, a transfer and a load, so it happens away from
    // this loop and reports back with the task it landed.
    let (landings, mut landed) = mpsc::unbounded_channel::<Result<String, String>>();
    let mut read_clock = time::interval(Duration::from_secs(3));
    // A credential shell is opened, prints, and ends on its own time: the sandbox may
    // have to be built first, which is minutes rather than frames.
    let (credential_events, mut credential_shell_events) = mpsc::unbounded_channel();
    // A profile's login is asked about off the loop too: the answer comes from a
    // sandbox, and one at a time, so the panel is never waiting on three.
    let (credential_reports, mut reported) = mpsc::unbounded_channel();
    // Searching Linear and reading one issue are network calls, so they happen on
    // their own clock and away from this loop. Each reply carries the popup and
    // query it answers, so a late one is dropped rather than shown under the
    // wrong question.
    let (issue_search_replies, mut issue_search_replied) =
        mpsc::unbounded_channel::<IssueSearchReply>();
    let (issue_read_replies, mut issue_read_replied) = mpsc::unbounded_channel::<IssueReadReply>();
    // Signing in to Linear opens a browser and waits on an OAuth callback, which
    // is minutes rather than frames, so it happens off this loop and reports back
    // like a task's run does.
    let (tracker_connects, mut tracker_connect_replied) =
        mpsc::unbounded_channel::<TrackerConnectReply>();
    // Acting on a remote task's signals is a dial, so it happens away from this
    // loop. Bounded, and each reply carries the pop-up it belongs to, so one that
    // arrives after the operator has left cannot replace what is on screen.
    let (remote_signals, mut remote_signals_replied) = mpsc::channel::<RemoteSignalsReply>(8);
    // Sending a task to a remote is a dial too. Bounded, and each reply names the
    // machine, so the notice says which one answered.
    let (delegations, mut delegated) = mpsc::channel::<TaskDelegationReply>(8);
    // Sending an existing task is a dial, a transfer and a boot. Bounded, and the
    // reply names the machine so the notice can.
    let (sends, mut sent) = mpsc::channel::<SendTaskReply>(8);
    // Sending a configuration is a dial. Bounded, and the reply carries the stamp
    // the bundle was built from.
    let (config_pushes, mut config_pushed) = mpsc::channel::<ConfigPushReply>(8);
    let mut issue_search_clock = time::interval(Duration::from_millis(80));
    loop {
        if mouse_captured != app.open_shell().is_some() {
            mouse_captured = app.open_shell().is_some();
            if mouse_captured {
                execute!(io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(io::stdout(), DisableMouseCapture)?;
            }
        }
        // A second line of intent in the creation question needs shift-enter to
        // arrive as a key of its own, which is the terminal's to say and only
        // while it is asked. Held for that page alone, so nothing else has to
        // read its keys differently.
        if keys_enhanced != matches!(app.foreground, Foreground::NewTask(_)) {
            keys_enhanced = !keys_enhanced;
            if keys_enhanced {
                execute!(
                    io::stdout(),
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    )
                )?;
            } else {
                execute!(io::stdout(), PopKeyboardEnhancementFlags)?;
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
            // Over the terminal that asked, which goes on running behind it.
            if let Foreground::OpenLink(asking) = &app.foreground {
                widgets::chrome::dim(frame, content);
                popups::open_link::draw(frame, content, asking);
            }
            if matches!(app.foreground, Foreground::Panel) {
                // Fullscreen: it is the screen, not something over it.
                let view = app.settings_view();
                screens::settings::draw(frame, content, &app.panel, &view, app.shimmer);
                if let super::app::CredentialShellState::Opening {
                    display, progress, ..
                } = &app.credential_shell
                {
                    widgets::chrome::dim(frame, content);
                    screens::shell::draw_preparing(frame, content, display, progress, app.shimmer);
                }
            } else if matches!(app.foreground, Foreground::Signals) {
                widgets::chrome::dim(frame, content);
                let summary = app.signals_summary();
                popups::signals::draw(
                    frame,
                    content,
                    &app.operator_signals,
                    summary.as_deref(),
                    app.selected_operator_signal,
                    app.selected_signal_action,
                    &mut app.operator_signal_scroll,
                );
            }
            if let Foreground::Confirm(confirm) = &app.foreground {
                widgets::chrome::dim(frame, content);
                confirm.draw(frame, content);
            }
            if let Foreground::SendTask(sending) = &app.foreground {
                widgets::chrome::dim(frame, content);
                popups::send_task::draw(frame, content, sending);
            }
            if let Foreground::NewTask(new_task) = &app.foreground {
                widgets::chrome::dim(frame, content);
                popups::new_task::draw(frame, content, new_task, app.shimmer);
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
            _ = terminate.recv() => {
                app.close_credential_shell(&task_service).await;
                return Ok(());
            }
            _ = interrupt.recv() => {
                app.close_credential_shell(&task_service).await;
                return Ok(());
            }
            _ = refresh.tick(), if app.open_shell().is_none() => {
                if let Ok(tasks) = event_loop.list().await {
                    app.tasks = tasks;
                    app.selected = app.selected.min(
                        (app.tasks.len() + app.remote_tasks.len()).saturating_sub(1),
                    );
                }
                app.capacity = Some(event_loop.capacity_limit());
            }
            _ = ask.tick() => {
                app.sync_remotes();
                if app.wants_remote_check() {
                    app.ask_next_remote(&remote_answers).await;
                }
                app.sync_credentials();
                if app.wants_credential_check() {
                    app.ask_next_credential(&task_service, &credential_reports);
                }
            }
            Some(answer) = answered.recv() => app.remote_answered(answer).await,
            Some((profile, status)) = reported.recv() => {
                app.credential_status.insert(profile, status);
            }
            _ = read_clock.tick(), if matches!(app.foreground, Foreground::Tasks) => {
                app.read_remote_tasks(&remote_task_reads);
            }
            Some(tasks) = remote_tasks_read.recv() => app.remote_tasks_read(tasks),
            Some(outcome) = landed.recv() => {
                match outcome {
                    Ok(id) => {
                        if let Ok(tasks) = event_loop.list().await {
                            app.tasks = tasks;
                        }
                        app.take_over(&id, terminal, &event_loop, &landings);
                    }
                    Err(error) => {
                        app.notice = Some(format!("could not bring it home: {error}"));
                    }
                }
            }
            _ = shell_notice.tick(), if app.shell_has_active_notice() => {}
            _ = shimmer.tick(),
                if app.is_preparing()
                    || app.is_asking_remotes()
                    || app.is_asking_credentials()
                    || app.is_awaiting_tracker()
                    || app.is_connecting_tracker()
                    || matches!(app.credential_shell, super::app::CredentialShellState::Opening { .. }) =>
            {
                app.shimmer = app.shimmer.wrapping_add(1);
                app.environment_builds = task_service.environment_builds();
            }
            Some(update) = task_execution_updates.recv() =>
                app.apply(update, terminal, &event_loop).await?,
            Some(event) = credential_shell_events.recv() =>
                app.credential_shell_event(event, terminal, &task_service).await,
            _ = issue_search_clock.tick() => {
                if let Some((token, query)) = app.due_issue_search() {
                    match task_service.tracker() {
                        Some(tracker) => {
                            let replies = issue_search_replies.clone();
                            tokio::spawn(async move {
                                let result = tracker
                                    .search(&query)
                                    .await
                                    .map_err(|error| error.to_string());
                                let _ = replies.send(IssueSearchReply { token, result });
                            });
                        }
                        // No provider at all is a property of the build.
                        None => app.issue_search_unavailable(token, NO_LINEAR),
                    }
                }
            }
            Some(reply) = issue_search_replied.recv() => {
                app.issue_search_resolved(reply, &task_service, &issue_read_replies)
            }
            Some(reply) = issue_read_replied.recv() => {
                app.issue_read_resolved(reply, &event_loop, &delegations).await
            }
            Some(reply) = delegated.recv() => {
                app.task_delegation_resolved(reply, &remote_task_reads)
            }
            Some(reply) = sent.recv() => {
                app.send_task_resolved(reply, &remote_task_reads)
            }
            Some(reply) = config_pushed.recv() => {
                app.config_push_resolved(reply)
            }
            Some(reply) = tracker_connect_replied.recv() => {
                app.tracker_connect_resolved(reply)
            }
            Some(reply) = app.pairings_replied.recv() => { app.pairing_resolved(reply); }
            Some(reply) = remote_signals_replied.recv() => {
                app.remote_signals_resolved(reply, &remote_task_reads)
            }
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                let quit = handle_event(
                    event?,
                    terminal,
                    &event_loop,
                    &task_service,
                    &mut app,
                    &EventReplies {
                        landings: &landings,
                        credentials: &credential_events,
                        issues: &issue_read_replies,
                        tracker_connects: &tracker_connects,
                        remote_signals: &remote_signals,
                        delegations: &delegations,
                        sends: &sends,
                        config_pushes: &config_pushes,
                    },
                )
                .await?;
                if quit {
                    return Ok(());
                }
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
        config_pushes,
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
        if let Some(id) = app.open_shell().cloned() {
            let size = app.shell_area(terminal.size()?.into());
            let bytes = app
                .shells
                .get_mut(&id)
                .and_then(|shell| shell.mouse_bytes(mouse, size));
            if let Some(bytes) = bytes {
                app.send_shell_input(&id, bytes, event_loop);
            }
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
        Foreground::Confirm(_) => {
            handle_confirm_key(app, terminal, event_loop, key, landings).await
        }
        Foreground::Reply(_) => app.reply_key(key, event_loop, remote_signals),
        Foreground::SendTask(_) => {
            app.send_task_key(key, event_loop, task_service, sends)
                .await
        }
        Foreground::NewTask(_) => {
            app.new_task_key(key, event_loop, task_service, issue_reads, delegations)
                .await
        }
        Foreground::Panel => {
            app.panel_keys(
                key,
                task_service,
                credential_events,
                tracker_connects,
                config_pushes,
            )
            .await
        }
        Foreground::Signals => app.signals_key(key, task_service, remote_signals).await,
        Foreground::Shell(_) => app.shell_key(key, event_loop, task_service).await,
        Foreground::OpenLink(_) => app.open_link_key(key),
        Foreground::Tasks => {
            if app.list_key(key, task_service, remote_signals).await? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
