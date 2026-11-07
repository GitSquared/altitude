use std::{
    fs, io,
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
};
use futures_util::StreamExt;
use ratatui::{DefaultTerminal, buffer::Buffer, layout::Rect, widgets::Clear};
use tachyonfx::EffectManager;
use tokio::{
    signal::unix::Signal,
    sync::{Notify, broadcast, mpsc},
    task::spawn_blocking,
    time,
};

use crate::{
    isolation::{IsolationEngine, Unwatched},
    remote::{ConfigBundle, Logins, Scan, transfer},
    task::{TaskEventLoop, TaskExecutionUpdate, TaskService},
};

use super::App;
use super::app::{
    ANNOUNCE_REFRESH, ConfigPushReply, Foreground, IssueReadReply, IssueSearchReply, NO_TRACKER,
    RemoteSignalsReply, RemoveTaskReply, RepositoryListReply, SendTaskReply, TaskDelegationReply,
    TrackerConnectReply, TrackerScopesReply, WatchedTasks,
};
use super::background::terrain;
use super::popups;
use super::screens;
use super::widgets;
use super::widgets::shimmer::shimmer_clock;
use crate::credentials::CredentialShellEvent;

mod events;
mod frame;

use events::handle_event;
use frame::{Leaving, draw_backdrop, draw_when, exit, place_pictures};

struct EventReplies<'a> {
    landings: &'a mpsc::UnboundedSender<Result<String, String>>,
    credentials: &'a mpsc::UnboundedSender<CredentialShellEvent>,
    issues: &'a mpsc::UnboundedSender<IssueReadReply>,
    tracker_connects: &'a mpsc::UnboundedSender<TrackerConnectReply>,
    remote_signals: &'a mpsc::Sender<RemoteSignalsReply>,
    delegations: &'a mpsc::Sender<TaskDelegationReply>,
    sends: &'a mpsc::Sender<SendTaskReply>,
    removals: &'a mpsc::Sender<RemoveTaskReply>,
    repositories: &'a mpsc::UnboundedSender<RepositoryListReply>,
    tracker_scopes: &'a mpsc::UnboundedSender<TrackerScopesReply>,
}

pub struct Feeds {
    pub task_execution: mpsc::UnboundedReceiver<TaskExecutionUpdate>,
    pub watched_tasks: mpsc::UnboundedReceiver<WatchedTasks>,
    // A task's history moving is also what the tracker back here has to report, and
    // this is where that is noticed: the queue is drained because something happened,
    // not because a clock says so.
    pub tracker_sync: Arc<Notify>,
}

pub async fn run<E>(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    event_loop: TaskEventLoop<E>,
    task_service: TaskService<E>,
    app: App,
    feeds: Feeds,
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
        feeds,
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
    feeds: Feeds,
) -> io::Result<()>
where
    E: IsolationEngine + Clone + 'static,
{
    execute!(io::stdout(), EnableBracketedPaste)?;
    app.watch_remotes();
    // What each remote answered with last time, before anything is asked, so the
    // first picker of a session names its machines instead of printing addresses
    // the operator never typed as names.
    app.reload_remembered_keys().await;
    // What a profile is called where an operator reads it, and the first thing a
    // new installation is shown.
    app.reload_profiles(&task_service);
    app.refresh_tracker_state(&task_service);
    app.refresh_git_identity().await;
    // A task cannot run without these, so the list is not behind the card: the
    // card is what there is until they are settled.
    if app.setup_incomplete() {
        app.open_get_started();
    }
    let Feeds {
        task_execution: mut task_execution_updates,
        watched_tasks: mut watched,
        tracker_sync,
    } = feeds;
    // Ports a sandbox makes reachable while its terminal is open are said on that
    // terminal's border, for a moment.
    let mut port_notices = task_service.port_notices();
    let mut events = EventStream::new();
    let mut refresh = time::interval(Duration::from_secs(2));
    let mut shell_notice = time::interval(Duration::from_millis(100));
    let mut shimmer = shimmer_clock();
    // The field drifts on its own, slower clock. It is not a reason for the
    // foreground to redraw, and the foreground is not a reason for it to.
    let mut backdrop = time::interval(terrain::CLOCK);
    backdrop.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    // A wait still has progress to report with animations off, so the clock that
    // reads it is not the one that moves the screen. Skip, so a long wait with
    // nothing to report does not come back as a burst of catch-up ticks.
    let mut progress_clock = time::interval(Duration::from_millis(100));
    progress_clock.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut configuration_clock = time::interval(Duration::from_secs(3));
    // What a remote is doing, while this client is waiting on it. Nothing is asked
    // of a machine that is ready and owed nothing.
    let mut remote_clock = time::interval(ANNOUNCE_REFRESH);
    remote_clock.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut reading_configuration = false;
    let (configuration_reads, mut configuration_read) = mpsc::unbounded_channel();
    let mut scan = None;
    let mut discovering = false;
    // A scan is a moment rather than a subscription: it ends when every source
    // has answered. On a page that stays open that would be the last thing
    // discovery ever did, so it is asked again while the page is there — the
    // network a remote is on can change under a screen nobody left.
    let mut discovery_failed = false;
    let mut discovery_clock = time::interval(Duration::from_secs(3));
    discovery_clock.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let mut mouse_captured = false;
    // Whether the terminal is reporting the keys the creation question needs.
    let mut keys_enhanced = false;
    // Anything that waits on another machine reports back through a watch, so the
    // clock below only drives what this machine checks about itself.
    let mut ask = time::interval(Duration::from_millis(250));

    // Bringing a task home is a dial, a transfer and a load, so it happens away from
    // this loop and reports back with the task it landed.
    let (landings, mut landed) = mpsc::unbounded_channel::<Result<String, String>>();
    // A credential shell is opened, prints, and ends on its own time: the sandbox may
    // have to be built first, which is minutes rather than frames.
    let (credential_events, mut credential_shell_events) = mpsc::unbounded_channel();
    // A profile's login is asked about off the loop too: the answer comes from a
    // sandbox, and one at a time, so the panel is never waiting on three.
    let (credential_reports, mut reported) = mpsc::unbounded_channel();
    // Listing repositories runs in the login's own sandbox, which may have to be
    // installed the first time, so it is off this loop like every other wait.
    let (repository_lists, mut repositories_listed) = mpsc::unbounded_channel();
    let (tracker_scopes, mut tracker_scopes_replied) = mpsc::unbounded_channel();
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
    let mut last_frame = Instant::now();
    // A modal that has just been dismissed, held as its last frame so it can be
    // drawn once more while it leaves, and the last frame of whatever modal is in
    // front, ready for when it does.
    let mut leaving: Option<Leaving> = None;
    let mut modal_cells: Option<Leaving> = None;
    let mut was_modal = app.has_modal();
    let mut was_panel = app.foreground.is_screen();
    // A frame is drawn only when something asks for one: at rest the loop sits in
    // its select and the terminal is left alone. While an effect runs, the clock
    // wakes it fast enough for the animation to move smoothly.
    let mut redraw = true;
    let mut backdrop_moved = false;
    // The frame a full draw left, so a frame where only the field moved does not
    // lay out the interface again.
    let mut snapshot: Option<Buffer> = None;
    // Comments on a signal thread come from the agent's tools, not from this
    // loop, so the service tells whoever is showing the thread instead of the
    // screen asking on a clock.
    let mut signal_changes = task_service.subscribe_signals();
    let mut listen_signals = true;
    let mut motion = time::interval(Duration::from_millis(8));
    motion.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    // A trackpad can deliver wheel events faster than a frame is worth drawing.
    // The state is updated for every one of them; the paint waits for the next
    // tick, so a fast scroll does not queue a frame per event.
    let mut scroll_pending = false;
    let mut paint = time::interval(Duration::from_millis(16));
    paint.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    // The project a new task starts in is built before a task asks for it, so the
    // first task is not a cold start. Off the loop, and only once an agent has been
    // chosen, because only then is there a task that could run.
    if let Some(agent) = app.agent()
        && let Some(project) = app
            .projects()
            .into_iter()
            .find(|project| project.id == app.active_project_id())
    {
        let service = task_service.clone();
        tokio::spawn(async move {
            if let Err(error) = service.prepare_project(&project, agent, &Unwatched).await {
                tracing::debug!(%error, "could not prepare the first project");
            }
        });
    }
    loop {
        // Discovery runs wherever a discovered machine can be shown: the first
        // screen's Remote row, and the Remotes page. Starting it when the card
        // opens is what lets the row offer a machine without being pressed first.
        let wants_discovery = app.foreground.shows_get_started()
            || app.credentials_over_card()
            || (app.foreground.shows_settings()
                && !app.credentials_over_card()
                && app.panel.section == screens::settings::Section::Remotes);
        if wants_discovery != discovering {
            discovering = wants_discovery;
            scan = None;
            discovery_failed = false;
            if discovering {
                app.discovered_remotes.clear();
                scan = start_scan(&mut app).await;
                discovery_failed = scan.is_none();
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
        // ctrl-enter and shift-enter only arrive as keys of their own under this
        // protocol. The task grid and signals use ctrl-enter as their secondary
        // action, and so do the creation page, the signal composer and a settings
        // form; the signal composer uses shift-enter as well. A shell needs it
        // too, or a prompt that asks for shift-enter only ever sees enter.
        let wants_enhanced = matches!(
            app.foreground,
            Foreground::NewTask(_) | Foreground::Tasks | Foreground::Signals | Foreground::Shell(_)
        ) || app.signal_composer.is_some()
            || app.panel.form.is_some();
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
        let elapsed = last_frame.elapsed().min(Duration::from_millis(80));
        last_frame = Instant::now();
        // A modal that has just gone keeps its last frame for a moment, so it can
        // be animated out after the screen behind it has taken the foreground.
        let is_modal = app.has_modal();
        let is_panel = app.foreground.is_screen();
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
            effects = EffectManager::default();
        }
        was_modal = is_modal;
        was_panel = is_panel;
        let can_patch = app.animations()
            && app.open_shell().is_none()
            && !app.needs_shimmer()
            && !effects.is_running()
            && leaving.is_none();
        if backdrop_moved {
            if !redraw && can_patch && draw_backdrop(terminal, &mut snapshot, &mut app.terrain)? {
                backdrop_moved = false;
            } else {
                redraw = true;
                backdrop_moved = false;
            }
        }
        let keep_snapshot = can_patch;
        // A copied message flashes for a moment and then stops on its own, so the
        // frame is where it ends rather than anything remembering to end it.
        app.expire_message_flash();
        let painted = draw_when(&mut redraw, terminal, |frame| {
            let hints = app.hints();
            let content = widgets::chrome::draw(frame, &hints, app.keyboard_hints());
            // A screen replaces the grid rather than lying over it, so the grid is
            // not drawn underneath one. The field is one layer under every screen,
            // so moving between screens does not restart it or give it a new
            // origin. Each screen marks the cells it has not covered, and the
            // field is painted into what is left once everything above it is drawn.
            // A credential terminal opened from the card sits on the card, not on
            // the settings list: the card is what asked for it.
            let first_run = app.foreground.shows_get_started()
                || app.credentials_over_card()
                || app.git_identity_over_card();
            let settings = app.foreground.shows_settings() && !first_run;
            let screen = settings || app.foreground.shows_signals() || first_run;
            if screen {
                frame.render_widget(Clear, content);
            }
            if app.backdrop_animates() {
                app.terrain.mark(frame.buffer_mut(), content);
            }
            if !screen {
                screens::tasks::draw(frame, content, &mut app);
            }
            // What the modal in front occupied, so the transition animates what
            // was actually drawn rather than a size written down a second time.
            let mut modal: Option<Rect> = None;
            if settings {
                // Fullscreen: it is the screen, not something over it.
                let view = app.settings_view();
                let tick = app.animation_tick();
                modal = Some(screens::settings::draw(
                    frame,
                    content,
                    &mut app.panel,
                    &view,
                    tick,
                    scan.is_some(),
                    &mut app.settings_targets,
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
                match &app.foreground {
                    Foreground::IdentitySetup(setup) => setup.draw(frame, content),
                    Foreground::RenameIdentity(rename) => rename.draw(frame, content, true),
                    Foreground::GitIdentity(identity) => identity.draw(frame, content),
                    _ => {}
                }
            } else if app.foreground.shows_signals() {
                // Fullscreen too: it is the screen the operator is reading, not
                // something laid over the work. A question about a signal is
                // drawn over it, so the thread it is about stays in view.
                screens::signals::draw(frame, content, &mut app);
                modal = Some(content);
            } else if first_run {
                let setup = app.setup();
                let tick = app.animation_tick();
                if let Foreground::GetStarted(panel) = &mut app.foreground {
                    screens::get_started::draw(frame, content, panel, &setup, tick);
                } else if app.git_identity_over_card() {
                    let mut panel = app.started_panel();
                    screens::get_started::draw(frame, content, &mut panel, &setup, tick);
                }
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
                if let Foreground::GitIdentity(identity) = &app.foreground {
                    identity.draw(frame, content);
                }
                modal = modal.or(Some(content));
            }
            // A terminal goes over the screen it was opened from, which is the
            // settings panel for a sign-in and the task list for a task's run. It
            // is drawn after that screen and before the popups above it.
            if let Some(shell) = app.open_shell().and_then(|id| app.shells.get(id)) {
                shell.draw(frame, content);
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
            if let Foreground::Message(message) = &app.foreground {
                widgets::chrome::dim(frame, content);
                modal = Some(popups::message::draw(frame, content, message));
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
            // A question about an account holds the screen until it is answered.
            if let Some((title, question, answers)) = app.tracker_conflict_question() {
                widgets::chrome::dim(frame, content);
                let answers: Vec<&str> = answers;
                modal = Some(widgets::question::draw(
                    frame,
                    content,
                    &title,
                    &question,
                    &answers,
                    app.conflict_choice,
                ));
            }
            // Keep what the interface looks like before the field goes in. That
            // is the frame a later field-only tick starts from, so it never has to
            // lay the interface out again just to move the contours.
            if keep_snapshot {
                snapshot = Some(frame.buffer_mut().clone());
            }
            // Paint the field into whatever every screen and modal left open.
            // Nothing has to tell it where those cells are: a widget that means to
            // be opaque writes them, and one that means to show the field does not.
            if app.backdrop_animates() {
                app.terrain.draw(frame.buffer_mut(), content);
            }
            // Keep the modal's last frame, ready to draw once more when it
            // leaves. Modals arrive at once; only the panel is drawn in.
            if app.animations()
                && let Some(area) = modal
            {
                modal_cells = Some(Leaving::take(frame.buffer_mut(), area));
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
        // A frame that was not painted decided nothing about where pictures go, so
        // its placements are left exactly as they were.
        if painted {
            place_pictures(terminal, &mut app)?;
        }
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
                    app.list_tasks(tasks);
                }
                app.pressure = event_loop.pressure();
                redraw = true;
            }
            event = async { scan.as_mut().expect("discovery active").recv().await }, if scan.is_some() => {
                match event {
                    Some(event) => app.discovery_observed(event),
                    None => scan = None,
                }
                redraw = true;
            }
            _ = discovery_clock.tick(), if discovering && scan.is_none() && !discovery_failed => {
                // Only rescans while there is nothing to show from the last one: the
                // rows already found stay on screen rather than blinking away.
                scan = start_scan(&mut app).await;
                discovery_failed = scan.is_none();
                redraw = true;
            }
            _ = remote_clock.tick() => app.refresh_remotes(),
            _ = configuration_clock.tick(), if !reading_configuration => {
                reading_configuration = true;
                let config = app.config.clone();
                // A machine no project runs on is not spoken to: there is nothing
                // there to keep in step, and what a bundle would carry for it is a
                // revocation. One this session gave something still hears it, so
                // unbinding a project takes its logins back.
                let addresses: Vec<String> = app
                    .configured_remotes()
                    .into_iter()
                    .filter(|address| {
                        app.remote_runs_projects(address) || app.remote_was_given_config(address)
                    })
                    .collect();
                if addresses.is_empty() {
                    reading_configuration = false;
                    continue;
                }
                let reports = configuration_reads.clone();
                tokio::spawn(async move {
                    let result = spawn_blocking(move || {
                        let settings = fs::read_to_string(config.settings_path())?;
                        // One bundle per machine, because each is sent the projects
                        // it runs and nothing else.
                        addresses
                            .into_iter()
                            .map(|address| {
                                let bundle = ConfigBundle::of(
                                    &settings,
                                    config.data_dir(),
                                    &[],
                                    Some(&address),
                                    Logins::WithWork,
                                )?;
                                Ok((address, bundle))
                            })
                            .collect::<Result<Vec<_>, io::Error>>()
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
                changed |= app.sync_credentials();
                if app.wants_credential_check() {
                    app.ask_credential_checks(&task_service, &credential_reports);
                    changed = true;
                }
                if app.wants_setup_check() {
                    app.ask_setup_check(&task_service, &credential_reports);
                    changed = true;
                }
                changed |= app.prefetch_setup_repositories(&task_service, &repository_lists);
                redraw |= changed;
            }
            Some(reply) = repositories_listed.recv() => {
                app.repositories_listed(reply);
                redraw = true;
            }
            Some(reply) = reported.recv() => {
                app.credential_checked(reply);
                redraw = true;
            }
            Some(reply) = tracker_scopes_replied.recv() => {
                app.tracker_scopes_listed(reply);
                redraw = true;
            }
            Some(event) = watched.recv() => {
                app.tasks_watched(event).await;
                redraw = true;
            }
            Some(outcome) = landed.recv() => {
                match outcome {
                    Ok(id) => {
                        if let Ok(tasks) = event_loop.list().await {
                            app.list_tasks(tasks);
                        }
                        app.settle_take(true);
                        app.task_landed(&id, terminal, &event_loop, &landings);
                    }
                    Err(error) if error == transfer::STOPPED => {
                        // The operator pressed the key that stops it, and the task
                        // is where they left it: the bar going is the answer.
                        app.settle_take(false);
                        app.transfer = None;
                    }
                    Err(error) => {
                        app.settle_take(false);
                        app.transfer = None;
                        app.problem(format!("could not pull the task: {error}"));
                    }
                }
                redraw = true;
            }
            _ = shell_notice.tick(), if app.shell_has_active_notice() => {
                // A notice that has run its course is cleared by this frame.
                redraw = true;
            }
            _ = backdrop.tick(), if app.backdrop_animates() => {
                app.terrain.advance();
                backdrop_moved = true;
            }
            _ = shimmer.tick(), if app.animations()
                && app.open_shell().is_none()
                && app.needs_shimmer() =>
            {
                app.shimmer = app.shimmer.wrapping_add(1);
                redraw = true;
            }
            // Lists and effects are the parts of the interface that move in less
            // than a character cell, so they run on the fast clock.
            _ = motion.tick(), if effects.is_running() || app.is_gliding() => {
                redraw = true;
                redraw |= app.glide_lists(elapsed);
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
                tracker_sync.notify_one();
                redraw = true;
            }
            notice = port_notices.recv() => {
                if let Ok(notice) = notice {
                    app.port_is_reachable(&notice.sandbox, notice.port);
                    redraw = true;
                }
            }
            Some(event) = credential_shell_events.recv() => {
                app.credential_shell_event(event, terminal, &task_service).await;
                redraw = true;
            }
            // The creation question debounces its search, so the clock only has to
            // run while it is the thing in front.
            _ = issue_search_clock.tick(), if matches!(app.foreground, Foreground::NewTask(_)) => {
                if let Some((token, query)) = app.due_issue_search() {
                    let tracker = app
                        .tracker_credential()
                        .and_then(|credential| task_service.tracker_for(&credential));
                    match tracker {
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
                        // Nothing is signed in to ask: the operator's to fix, and
                        // the page says so rather than blaming the build.
                        None => app.issue_search_unavailable(token, NO_TRACKER),
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
                app.task_delegation_resolved(reply);
                redraw = true;
            }
            Some(reply) = removed.recv() => {
                app.remove_remote_resolved(reply);
                redraw = true;
            }
            Some(reply) = sent.recv() => {
                app.send_task_resolved(reply);
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
                app.remote_signals_resolved(reply);
                redraw = true;
            }
            changed = signal_changes.recv(), if listen_signals => {
                match changed {
                    Ok(task_id) => app.signal_changed(&task_id, &event_loop).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        app.signal_changes_lagged(&event_loop).await
                    }
                    Err(broadcast::error::RecvError::Closed) => listen_signals = false,
                }
                redraw = true;
            }
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                let event = event?;
                // A captured mouse reports every movement it makes, and a movement
                // that only passes over something changes nothing. Drawing for
                // each of them is what turns a moving pointer into a stutter.
                let quiet = matches!(
                    &event,
                    Event::Mouse(mouse)
                        if matches!(
                            mouse.kind,
                            MouseEventKind::Moved
                                | MouseEventKind::ScrollUp
                                | MouseEventKind::ScrollDown
                                | MouseEventKind::ScrollLeft
                                | MouseEventKind::ScrollRight
                        )
                );
                let scrolling = matches!(
                    &event,
                    Event::Mouse(mouse)
                        if matches!(
                            mouse.kind,
                            MouseEventKind::ScrollUp
                                | MouseEventKind::ScrollDown
                                | MouseEventKind::ScrollLeft
                                | MouseEventKind::ScrollRight
                        )
                );
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
                        repositories: &repository_lists,
                        tracker_scopes: &tracker_scopes,
                    },
                )
                .await?;
                if quit {
                    return Ok(());
                }
                if scrolling {
                    scroll_pending = true;
                } else if !quiet {
                    redraw = true;
                }
            }
            _ = paint.tick(), if scroll_pending => {
                scroll_pending = false;
                redraw = true;
            }
        }
    }
}

async fn start_scan(app: &mut App) -> Option<Scan> {
    match app.link.scan_remotes().await {
        Ok(scan) => Some(scan),
        Err(error) => {
            tracing::warn!(%error, "Network discovery unavailable");
            app.say("Network discovery unavailable · add a remote by hostname");
            None
        }
    }
}
