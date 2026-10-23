//! The terminal loop: events in, and one frame out.
//!
//! Everything here is about the terminal rather than about the work: reading
//! events, routing them to whatever is in front, and drawing. What the
//! application does with an event is its own business, in `app`.

use std::{
    fs, io, mem,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyboardEnhancementFlags, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};
use futures_util::StreamExt;
use ratatui::{DefaultTerminal, Frame, buffer::Buffer, layout::Rect};
use tachyonfx::{
    Effect, EffectManager, Interpolation, fx,
    pattern::{InvertedPattern, RadialPattern, SweepPattern},
};
use tokio::{signal::unix::Signal, sync::mpsc, task::spawn_blocking, time};

use crate::{
    isolation::IsolationEngine,
    remote::{ConfigBundle, transfer},
    task::{TaskEventLoop, TaskExecutionUpdate, TaskService},
};

use super::App;
use super::app::{
    ConfigPushReply, Foreground, HeldTask, IssueReadReply, IssueSearchReply, NO_LINEAR,
    RemoteSignalsReply, RemoveTaskReply, SendTaskReply, TaskDelegationReply, TrackerConnectReply,
};
use super::keys::{is_dismiss, is_help};
use super::popups;
use super::popups::confirm::handle_confirm_key;
use super::screens;
use super::screens::settings::Focus;
use super::widgets;
use super::widgets::utils::shimmer_clock;
use crate::credentials::CredentialShellEvent;

/// A modal's last frame, kept so it can be drawn once more while it leaves.
///
/// By the time a modal is dismissed, the screen behind it already has the
/// foreground, so the only way to animate the modal out is to hold the cells it
/// left behind and draw them back over the screen for as long as it takes.
struct Leaving {
    area: Rect,
    cells: Buffer,
}

impl Leaving {
    fn take(buffer: &Buffer, area: Rect) -> Self {
        let mut cells = Buffer::empty(area);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                cells[(x, y)] = buffer[(x, y)].clone();
            }
        }
        Self { area, cells }
    }

    fn draw(&self, buffer: &mut Buffer) {
        for y in self.area.y..self.area.bottom() {
            for x in self.area.x..self.area.right() {
                buffer[(x, y)] = self.cells[(x, y)].clone();
            }
        }
    }
}

/// How the panel arrives: drawn down from the top, the way the rail above it is
/// read.
///
/// A dissolve clears the cells it has not reached, so the area it has still to
/// cover shows the terminal behind it. A sweep would paint those cells, which is
/// a colour this interface cannot know.
fn panel_entry(area: Rect) -> Effect {
    fx::dissolve((90, Interpolation::QuadOut))
        .with_pattern(InvertedPattern::new(SweepPattern::up_to_down(12)))
        .with_area(area)
}

/// How a modal leaves. A pop-up reverses the dissolve it would have arrived
/// with; the panel slides back up the way it came down.
fn exit(panel: bool, area: Rect) -> Effect {
    if panel {
        fx::coalesce((160, Interpolation::QuadOut))
            .with_pattern(InvertedPattern::new(SweepPattern::down_to_up(12)))
    } else {
        fx::coalesce((140, Interpolation::QuadOut))
            .with_pattern(InvertedPattern::new(RadialPattern::center()))
    }
    .with_area(area)
}

/// Draw a frame, but only when something has asked for one. At rest the loop
/// never reaches this, so an idle interface costs nothing.
fn draw_when(
    redraw: &mut bool,
    terminal: &mut DefaultTerminal,
    render: impl FnOnce(&mut Frame),
) -> io::Result<()> {
    if *redraw {
        terminal.draw(render)?;
        *redraw = false;
    }
    Ok(())
}

struct EventReplies<'a> {
    landings: &'a mpsc::UnboundedSender<Result<String, String>>,
    credentials: &'a mpsc::UnboundedSender<CredentialShellEvent>,
    issues: &'a mpsc::UnboundedSender<IssueReadReply>,
    tracker_connects: &'a mpsc::UnboundedSender<TrackerConnectReply>,
    remote_signals: &'a mpsc::Sender<RemoteSignalsReply>,
    delegations: &'a mpsc::Sender<TaskDelegationReply>,
    sends: &'a mpsc::Sender<SendTaskReply>,
    removals: &'a mpsc::Sender<RemoveTaskReply>,
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
    //
    // This happens before the work is stopped. Suspending every task can take a
    // moment, and a shell prompt is a better place to wait for it than a screen
    // that has stopped drawing.
    execute!(
        io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags
    )?;
    ratatui::restore();
    if let Err(error) = event_loop.suspend_all().await {
        tracing::error!(%error, "not every task could be suspended on the way out");
    }
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
    // What each remote answered with last time, before anything is asked, so the
    // first picker of a session names its machines instead of printing addresses
    // the operator never typed as names.
    app.reload_remembered_keys().await;
    let mut events = EventStream::new();
    let mut refresh = time::interval(Duration::from_secs(2));
    let mut shell_notice = time::interval(Duration::from_millis(100));
    let mut shimmer = shimmer_clock();
    // A wait still has progress to report with animations off, so the clock that
    // reads it is not the one that moves the screen. Skip, so a long wait with
    // nothing to report does not come back as a burst of catch-up ticks.
    let mut progress_clock = time::interval(Duration::from_millis(100));
    progress_clock.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut configuration_clock = time::interval(Duration::from_secs(3));
    let mut connection_clock = time::interval(Duration::from_secs(15));
    let mut reading_configuration = false;
    let (configuration_reads, mut configuration_read) = mpsc::unbounded_channel();
    let mut scan = None;
    let mut remotes_visible = false;

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
    // machine, so the message says which one answered.
    let (delegations, mut delegated) = mpsc::channel::<TaskDelegationReply>(8);
    // Sending an existing task is a dial, a transfer and a boot. Bounded, and the
    // reply names the machine so the message can.
    let (sends, mut sent) = mpsc::channel::<SendTaskReply>(8);
    // Removing a task another machine holds is a dial, so it happens away from
    // this loop and reports back what that machine did.
    let (removals, mut removed) = mpsc::channel::<RemoveTaskReply>(8);
    // Acknowledgements identify the exact content sent while local edits continue.
    let (config_pushes, mut config_pushed) = mpsc::channel::<ConfigPushReply>(8);
    let mut issue_search_clock = time::interval(Duration::from_millis(80));
    let mut effects: EffectManager<()> = EffectManager::default();
    let mut previous_foreground = mem::discriminant(&app.foreground);
    let mut last_frame = Instant::now();
    // A modal that has just been dismissed, held as its last frame so it can be
    // drawn once more while it leaves, and the last frame of whatever modal is in
    // front, ready for when it does.
    let mut leaving: Option<Leaving> = None;
    let mut modal_cells: Option<Leaving> = None;
    let mut was_modal = app.has_modal();
    let mut was_panel = matches!(app.foreground, Foreground::Panel);
    // A frame is drawn only when something asks for one: at rest the loop sits in
    // its select and the terminal is left alone. While an effect runs, the clock
    // wakes it fast enough for the animation to move smoothly.
    let mut redraw = true;
    let mut motion = time::interval(Duration::from_millis(8));
    motion.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    // A task's environment is built before a task asks for it, so the first task is
    // not a cold start. Off the loop, and only once an agent has been chosen, because
    // only then is there a task that could run.
    if let Some(agent) = app.agent() {
        let environment = app.default_environment_value();
        let service = task_service.clone();
        tokio::spawn(async move {
            if let Err(error) = service.prepare_environment(&environment, agent).await {
                tracing::debug!(%error, "could not prepare the default environment");
            }
        });
    }
    loop {
        let showing_remotes = app.foreground.shows_settings()
            && app.panel.section == screens::settings::Section::Remotes;
        if showing_remotes != remotes_visible {
            remotes_visible = showing_remotes;
            scan = None;
            if showing_remotes {
                app.discovered_remotes.clear();
                scan = match app.link.scan_remotes().await {
                    Ok(scan) => Some(scan),
                    Err(error) => {
                        tracing::warn!(%error, "Network discovery unavailable");
                        app.say("Network discovery unavailable · add a remote by hostname");
                        None
                    }
                };
            }
        }
        let wants_mouse = app.wants_mouse();
        if mouse_captured != wants_mouse {
            mouse_captured = wants_mouse;
            if mouse_captured {
                execute!(io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(io::stdout(), DisableMouseCapture)?;
            }
        }
        // The gesture that proceeds is ctrl-enter, which only this protocol reports
        // as a key of its own. It is held for the two screens that offer it, and
        // given back everywhere else, so no other surface reads its keys
        // differently and a shell never sees the protocol's own sequences.
        let wants_enhanced = matches!(app.foreground, Foreground::NewTask(_) | Foreground::Tasks);
        if keys_enhanced != wants_enhanced {
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
        let foreground = mem::discriminant(&app.foreground);
        let entering = foreground != previous_foreground;
        previous_foreground = foreground;
        let elapsed = last_frame.elapsed().min(Duration::from_millis(80));
        last_frame = Instant::now();
        // A modal that has just gone keeps its last frame for a moment, so it can
        // be animated out after the screen behind it has taken the foreground.
        let is_modal = app.has_modal();
        let is_panel = matches!(app.foreground, Foreground::Panel);
        if !app.animations() {
            leaving = None;
        } else if was_modal && !is_modal {
            if let Some(cells) = modal_cells.take() {
                effects = EffectManager::default();
                effects.add_effect(exit(was_panel, cells.area));
                leaving = Some(cells);
                redraw = true;
            }
        } else if is_modal && !was_modal {
            leaving = None;
        }
        was_modal = is_modal;
        was_panel = is_panel;
        draw_when(&mut redraw, terminal, |frame| {
            let hints = app.hints();
            let content = widgets::chrome::draw(frame, &hints, app.keyboard_hints());
            screens::tasks::draw(frame, content, &mut app);
            // What the modal in front occupied, so the transition animates what
            // was actually drawn rather than a size written down a second time.
            let mut modal: Option<Rect> = None;
            if app.foreground.shows_settings() {
                // Fullscreen: it is the screen, not something over it.
                let view = app.settings_view();
                modal = Some(screens::settings::draw(
                    frame,
                    content,
                    &app.panel,
                    &view,
                    app.animation_tick(),
                    scan.is_some(),
                ));
                if let super::app::CredentialShellState::Opening {
                    progress: Some(progress),
                    ..
                } = &app.credential_shell
                {
                    widgets::chrome::dim(frame, content);
                    modal = Some(widgets::loading::draw(
                        frame,
                        content,
                        progress,
                        app.animation_tick(),
                    ));
                }
            }
            // A terminal goes over the screen it was opened from, which is the
            // settings panel for a sign-in and the task list for a task's run. It
            // is drawn after that screen and before the popups above it.
            if let Some(shell) = app.open_shell().and_then(|id| app.shells.get(id)) {
                shell.draw(frame, content);
            }
            // Over the terminal that asked, which goes on running behind it.
            if let Foreground::OpenLink(asking) = &app.foreground {
                widgets::chrome::dim(frame, content);
                modal = Some(popups::open_link::draw(frame, content, asking));
            }
            if matches!(app.foreground, Foreground::Signals) {
                widgets::chrome::dim(frame, content);
                let summary = app.signals_summary();
                modal = Some(popups::signals::draw(
                    frame,
                    content,
                    &app.operator_signals,
                    summary.as_deref(),
                    app.selected_operator_signal,
                    app.selected_signal_action,
                    &mut app.operator_signal_scroll,
                ));
            }
            if let Foreground::Setup(setup) = &mut app.foreground {
                widgets::chrome::dim(frame, content);
                let report = setup
                    .remote_address()
                    .and_then(|address| app.remote_trackers.get(address))
                    .map(Vec::as_slice);
                modal = Some(setup.draw(frame, content, report));
            }
            if let Foreground::SignIn(sign_in) = &app.foreground {
                widgets::chrome::dim(frame, content);
                modal = Some(sign_in.draw(frame, content));
            }
            if let Foreground::Confirm(confirm) = &app.foreground {
                widgets::chrome::dim(frame, content);
                modal = Some(confirm.draw(frame, content));
            }
            if let Foreground::SendTask(sending) = &app.foreground {
                widgets::chrome::dim(frame, content);
                modal = Some(popups::send_task::draw(frame, content, sending));
            }
            if let Foreground::NewTask(new_task) = &app.foreground {
                widgets::chrome::dim(frame, content);
                modal = Some(popups::new_task::draw(
                    frame,
                    content,
                    new_task,
                    app.animation_tick(),
                    &mut app.new_task_targets,
                ));
            }
            if matches!(app.foreground, Foreground::Keys(_)) {
                widgets::chrome::dim(frame, content);
                modal = Some(widgets::chrome::draw_help(
                    frame,
                    content,
                    &super::keys::help_sections(),
                    &mut app.help_scroll,
                ));
            }
            // Input belongs above the settings overlay that opened it.
            if let Foreground::Reply(reply) = &app.foreground {
                widgets::chrome::dim(frame, content);
                modal = Some(reply.prompt.draw(frame, content));
            }
            // A transfer in flight is the one wait with something to count.
            if let Some(progress) = app.transfer_progress() {
                widgets::chrome::dim(frame, content);
                modal = Some(widgets::progress::draw(
                    frame,
                    content,
                    &progress,
                    app.animation_tick(),
                ));
            }
            // A message is read over whatever asked for it, including a wait.
            if let Some(alert) = &app.alert {
                widgets::chrome::dim(frame, content);
                modal = Some(widgets::alert::draw(frame, content, alert));
            }
            // Keep the modal's last frame, ready to draw once more when it
            // leaves. Modals arrive at once; only the panel is drawn in.
            if app.animations()
                && let Some(area) = modal
            {
                modal_cells = Some(Leaving::take(frame.buffer_mut(), area));
            }
            if entering && app.animations() && is_panel {
                effects = EffectManager::default();
                if let Some(area) = modal {
                    effects.add_effect(panel_entry(area));
                }
            }
            if let Some(cells) = &leaving {
                cells.draw(frame.buffer_mut());
            }
            // Off, the manager is emptied rather than left holding effects that
            // would resume mid-flight if it were turned back on.
            if app.animations() {
                effects.process_effects(elapsed.into(), frame.buffer_mut(), content);
            } else {
                effects = EffectManager::default();
            }
        })?;
        // A modal has finished leaving once nothing is drawing it out.
        if leaving.is_some() && !effects.is_running() {
            leaving = None;
        }
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
                redraw = true;
            }
            event = async { scan.as_mut().expect("discovery active").recv().await }, if scan.is_some() => {
                match event {
                    Some(event) => app.discovery_observed(event),
                    None => scan = None,
                }
                redraw = true;
            }
            _ = connection_clock.tick(), if !app.configured_remotes().is_empty() => {
                app.refresh_remote_connections();
                redraw = true;
            }
            _ = configuration_clock.tick(), if !reading_configuration => {
                reading_configuration = true;
                let config = app.config.clone();
                let profiles: Vec<_> = task_service.credentials().profiles().iter().map(|profile| profile.profile).collect();
                let reports = configuration_reads.clone();
                tokio::spawn(async move {
                    let result = spawn_blocking(move || {
                        let settings = fs::read_to_string(config.settings_path())?;
                        ConfigBundle::of(&settings, config.data_dir(), &profiles, &[])
                    }).await.map_err(|error| error.to_string())
                        .and_then(|result| result.map_err(|error| error.to_string()));
                    let _ = reports.send(result);
                });
            }
            Some(result) = configuration_read.recv() => {
                reading_configuration = false;
                app.configuration_read(result, &config_pushes);
                redraw = true;
            }
            _ = ask.tick() => {
                // Every quarter second, so it only asks for a frame when one of the
                // checks it runs actually changed something.
                let mut changed = false;
                if app.wants_remote_check() {
                    app.ask_next_remote(&remote_answers).await;
                    changed = true;
                }
                changed |= app.sync_credentials();
                if app.wants_credential_check() {
                    app.ask_credential_checks(&task_service, &credential_reports);
                    changed = true;
                }
                redraw |= changed;
            }
            Some(answer) = answered.recv() => {
                app.remote_answered(answer).await;
                redraw = true;
            }
            Some(reply) = reported.recv() => {
                app.credential_checked(reply);
                redraw = true;
            }
            _ = read_clock.tick(), if matches!(app.foreground, Foreground::Tasks) => {
                app.read_remote_tasks(&remote_task_reads);
            }
            Some(tasks) = remote_tasks_read.recv() => {
                app.remote_tasks_read(tasks);
                redraw = true;
            }
            Some(outcome) = landed.recv() => {
                match outcome {
                    Ok(id) => {
                        if let Ok(tasks) = event_loop.list().await {
                            app.tasks = tasks;
                        }
                        app.task_landed(&id, terminal, &event_loop, &landings);
                    }
                    Err(error) if error == transfer::STOPPED => {
                        app.transfer = None;
                        app.say("Transfer stopped; the task did not move");
                    }
                    Err(error) => {
                        app.transfer = None;
                        app.problem(format!("could not pull the task: {error}"));
                    }
                }
                redraw = true;
            }
            _ = shell_notice.tick(), if app.shell_has_active_notice() => {}
            _ = shimmer.tick(), if app.animations() && app.open_shell().is_none() => {
                app.shimmer = app.shimmer.wrapping_add(1);
                redraw = true;
            }
            _ = motion.tick(), if effects.is_running() => {
                redraw = true;
            }
            // The two waits with something to report: a build's place in its
            // queue, and a transfer's bytes. One clock, because neither is worth
            // waking the loop for on its own.
            _ = progress_clock.tick(), if app.transfer.is_some()
                || (app.open_shell().is_none() && app.is_awaiting_progress()) => {
                app.observe_transfer();
                if app.open_shell().is_none() && app.is_awaiting_progress() {
                    app.environment_builds = task_service.environment_builds();
                }
                redraw = true;
            }
            Some(update) = task_execution_updates.recv() => {
                app.apply(update, terminal, &event_loop).await?;
                redraw = true;
            }
            Some(event) = credential_shell_events.recv() => {
                app.credential_shell_event(event, terminal, &task_service).await;
                redraw = true;
            }
            // The creation question debounces its search, so the clock only has to
            // run while it is the thing in front.
            _ = issue_search_clock.tick(), if matches!(app.foreground, Foreground::NewTask(_)) => {
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
                    redraw = true;
                }
            }
            Some(reply) = issue_search_replied.recv() => {
                app.issue_search_resolved(reply, &task_service, &issue_read_replies);
                redraw = true;
            }
            Some(reply) = issue_read_replied.recv() => {
                app.issue_read_resolved(reply, &event_loop, &delegations).await;
                redraw = true;
            }
            Some(reply) = delegated.recv() => {
                app.task_delegation_resolved(reply, &remote_task_reads);
                redraw = true;
            }
            Some(reply) = removed.recv() => {
                app.remove_remote_resolved(reply);
                redraw = true;
            }
            Some(reply) = sent.recv() => {
                app.send_task_resolved(reply, &remote_task_reads);
                redraw = true;
            }
            Some(reply) = config_pushed.recv() => {
                app.config_push_resolved(reply);
                redraw = true;
            }
            Some(reply) = tracker_connect_replied.recv() => {
                app.tracker_connect_resolved(reply);
                redraw = true;
            }
            Some(reply) = remote_signals_replied.recv() => {
                app.remote_signals_resolved(reply, &remote_task_reads);
                redraw = true;
            }
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                let event = event?;
                // A captured mouse reports every movement it makes, and a movement
                // that only passes over something changes nothing. Drawing for
                // each of them is what turns a moving pointer into a stutter.
                let quiet = matches!(&event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved);
                let quit = handle_event(
                    event,
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
                        removals: &removals,
                    },
                )
                .await?;
                if quit {
                    return Ok(());
                }
                if !quiet {
                    redraw = true;
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
        removals,
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
    if is_help(key) && !app.is_typing() {
        app.help_scroll = 0;
        app.foreground = Foreground::Keys(Box::new(mem::take(&mut app.foreground)));
        return Ok(false);
    }
    // In the order the states are declared: whatever is in front takes the key,
    // and the line under it says which one that is.
    match &app.foreground {
        Foreground::Keys(_) => app.dismiss_keys(),
        Foreground::Confirm(_) => {
            handle_confirm_key(app, terminal, event_loop, key, landings, removals).await
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
            app.panel_keys(key, task_service, credential_events, tracker_connects)
                .await
        }
        Foreground::Setup(_) => app.setup_key(key),
        Foreground::SignIn(_) => app.sign_in_key(key, task_service, credential_events).await,
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
