use std::{
    env, io,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::{
    config::Config,
    db::Database,
    remote::{InstallationKey, LinkClient, RemoteKeys},
    task::TaskStore,
};

use super::{App, Pictures, screens, widgets};

// Draws the task list over a database of fabricated tasks, with no task service
// behind it: nothing can start, nothing is watched, and the only keys are the
// ones the list itself answers.
//
// It exists so the interface can be looked at — a screen is judged by looking at
// it, and judging it on real work means asking real agents to run. Debug builds
// only; a release build has no such mode.
pub(crate) async fn run() -> io::Result<()> {
    let config = Config::load()?;
    let database = Database::open(&config).await.map_err(io::Error::other)?;
    let tasks = TaskStore::new(&database)
        .list()
        .await
        .map_err(io::Error::other)?;
    let settings = config.load_or_create_settings();
    let remote_keys = RemoteKeys::new(database.pool());
    let key = InstallationKey::load_or_create(config.data_dir().join("identity.key"))
        .map_err(io::Error::other)?;
    let link = LinkClient::bind(&key).await.map_err(io::Error::other)?;
    let (watched, _) = mpsc::unbounded_channel();
    let pictures = Pictures::detect(config.attachments_dir());
    let mut app = App::new(
        tasks,
        config,
        settings,
        remote_keys,
        link,
        watched,
        pictures,
    );

    let mut terminal = ratatui::init();
    let result = draw_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn draw_loop(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    // A screen of its own that repaints twelve times a second is a terminal's work
    // for nothing while it is being looked at: the field is drawn once and left
    // still unless the run asks for the motion back.
    let animate = env::var_os("ALTITUDE_PREVIEW_ANIMATE").is_some();
    let mut drawn_at = Instant::now();
    // A terminal that attaches a moment after the process starts sees a screen
    // that was drawn for a size it did not have yet, so the first frames are
    // repainted even though nothing has asked for them.
    let started_at = Instant::now();
    let mut redraw = true;
    loop {
        // Input first and on its own clock: a frame that is still being painted
        // must not be what a key press waits for.
        if event::poll(Duration::from_millis(10))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if leaves(app, key) {
                        return Ok(());
                    }
                    redraw = true;
                }
                Event::Mouse(mouse) => {
                    app.task_mouse(mouse);
                    redraw = true;
                }
                Event::Resize(..) => redraw = true,
                _ => {}
            }
        }
        let elapsed = drawn_at.elapsed();
        if app.is_gliding() && elapsed >= Duration::from_millis(16) {
            app.glide_lists(elapsed);
            redraw = true;
        }
        // The clocks a real run keeps, on the rate it keeps them: the travelling
        // shimmer, and the field drifting on its own slower clock.
        if animate && elapsed >= Duration::from_millis(80) {
            drawn_at = Instant::now();
            if app.animations() {
                app.shimmer = app.shimmer.wrapping_add(1);
            }
            if app.backdrop_animates() {
                app.terrain.advance();
            }
            redraw = true;
        }
        if !redraw && started_at.elapsed() > Duration::from_millis(600) {
            continue;
        }
        redraw = false;
        terminal.draw(|frame| {
            let hints = app.hints();
            let content = widgets::chrome::draw(frame, &hints, app.keyboard_hints());
            if app.backdrop_animates() {
                app.terrain.mark(frame.buffer_mut(), content);
            }
            screens::tasks::draw(frame, content, app);
            if app.backdrop_animates() {
                app.terrain.draw(frame.buffer_mut(), content);
            }
        })?;
    }
}

fn leaves(app: &mut App, key: KeyEvent) -> bool {
    // A menu owns the keyboard while it is open, as it does in the interface:
    // escape closes it rather than leaving, and so does q.
    if app.task_filter.is_open() {
        app.task_filter.handle_key(key);
        app.apply_task_view();
        return false;
    }
    if app.task_sort.is_open() {
        app.task_sort.handle_key(key);
        app.apply_task_view();
        return false;
    }
    // Leaving is q, as it is on the list this draws.
    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return true;
    }
    if app.tasks_list.key(key).is_some() {
        return false;
    }
    match key.code {
        KeyCode::Char('f') => app.task_filter.open(),
        KeyCode::Char('t') => app.task_sort.open(),
        KeyCode::Tab => app.switch_project(true),
        KeyCode::BackTab => app.switch_project(false),
        _ => {}
    }
    false
}
