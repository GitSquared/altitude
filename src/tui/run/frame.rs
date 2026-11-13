use std::{
    io::{self, Write},
    mem,
    time::Duration,
};

use ratatui::{DefaultTerminal, Frame, buffer::Buffer, layout::Rect, widgets::Clear};
use tachyonfx::{
    Effect, EffectManager, Interpolation, fx,
    pattern::{InvertedPattern, RadialPattern, SweepPattern},
};

use crate::tui::App;
use crate::tui::app::{CredentialShellState, Foreground};
use crate::tui::background::terrain;
use crate::tui::keys::help_sections;
use crate::tui::{popups, screens, widgets};

pub(super) struct Leaving {
    pub(super) area: Rect,
    cells: Buffer,
}

impl Leaving {
    pub(super) fn take(buffer: &Buffer, area: Rect) -> Self {
        let mut cells = Buffer::empty(area);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                cells[(x, y)] = buffer[(x, y)].clone();
            }
        }
        Self { area, cells }
    }

    pub(super) fn draw(&self, buffer: &mut Buffer) {
        for y in self.area.y..self.area.bottom() {
            for x in self.area.x..self.area.right() {
                buffer[(x, y)] = self.cells[(x, y)].clone();
            }
        }
    }
}

pub(super) fn exit(panel: bool, area: Rect) -> Effect {
    if panel {
        fx::coalesce((110, Interpolation::QuadOut))
            .with_pattern(InvertedPattern::new(SweepPattern::down_to_up(12)))
    } else {
        fx::coalesce((110, Interpolation::QuadOut))
            .with_pattern(InvertedPattern::new(RadialPattern::center()))
    }
    .with_area(area)
}

pub(super) fn draw_when(
    redraw: &mut bool,
    terminal: &mut DefaultTerminal,
    render: impl FnOnce(&mut Frame),
) -> io::Result<bool> {
    if !*redraw {
        return Ok(false);
    }
    terminal.draw(render)?;
    *redraw = false;
    Ok(true)
}

// Pictures are positioned by the cursor, so they are written to the terminal once the
// frame that decided where they go has been drawn.
pub(super) fn place_pictures(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    let placements = app.pictures.settle();
    if placements.is_empty() {
        return Ok(());
    }
    let backend = terminal.backend_mut();
    backend.write_all(placements.as_bytes())?;
    backend.flush()
}

pub(super) fn draw_backdrop(
    terminal: &mut DefaultTerminal,
    snapshot: &mut Option<Buffer>,
    terrain: &mut terrain::Terrain,
) -> io::Result<bool> {
    terminal.autoresize()?;
    let area = terminal.get_frame().area();
    let Some(previous) = snapshot.as_ref().filter(|buffer| buffer.area == area) else {
        return Ok(false);
    };
    let mut next = previous.clone();
    terrain.repaint(&mut next);
    mem::swap(terminal.current_buffer_mut(), &mut next);
    terminal.apply_buffer()?;
    Ok(true)
}

// Everything one draw reads from the loop and everything it leaves for the next:
// a drawing is one pass over the interface, not a value the loop keeps.
pub(super) struct Drawing<'a> {
    pub(super) app: &'a mut App,
    pub(super) scan_running: bool,
    pub(super) keep_snapshot: bool,
    pub(super) snapshot: &'a mut Option<Buffer>,
    pub(super) modal_cells: &'a mut Option<Leaving>,
    pub(super) leaving: Option<&'a Leaving>,
    pub(super) effects: &'a mut EffectManager<()>,
    pub(super) elapsed: Duration,
}

impl Drawing<'_> {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
        let hints = self.app.hints();
        let content = widgets::chrome::draw(frame, &hints, self.app.keyboard_hints());
        // A screen replaces the grid rather than lying over it, so the grid is
        // not drawn underneath one. The field is one layer under every screen,
        // so moving between screens does not restart it or give it a new
        // origin. Each screen marks the cells it has not covered, and the
        // field is painted into what is left once everything above it is drawn.
        // A credential terminal opened from the card sits on the card, not on
        // the settings list: the card is what asked for it.
        let first_run = self.app.foreground.shows_get_started()
            || self.app.credentials_over_card()
            || self.app.git_identity_over_card();
        let settings = self.app.foreground.shows_settings() && !first_run;
        let screen = settings || self.app.foreground.shows_signals() || first_run;
        if screen {
            frame.render_widget(Clear, content);
        }
        if self.app.backdrop_animates() {
            self.app.terrain.mark(frame.buffer_mut(), content);
        }
        if !screen {
            screens::tasks::draw(frame, content, &mut *self.app);
        }
        // What the modal in front occupied, so the transition animates what
        // was actually drawn rather than a size written down a second time.
        let mut modal: Option<Rect> = None;
        if settings {
            // Fullscreen: it is the screen, not something over it.
            let view = self.app.settings_view();
            let tick = self.app.animation_tick();
            modal = Some(screens::settings::draw(
                frame,
                content,
                &mut self.app.panel,
                &view,
                tick,
                self.scan_running,
                &mut self.app.settings_targets,
            ));
            if let CredentialShellState::Opening {
                progress: Some(progress),
                ..
            } = &self.app.credential_shell
            {
                widgets::chrome::dim(frame, content);
                modal = Some(widgets::loading::draw(
                    frame,
                    content,
                    progress,
                    self.app.animation_tick(),
                ));
            }
            match &self.app.foreground {
                Foreground::IdentitySetup(setup) => setup.draw(frame, content),
                Foreground::RenameIdentity(rename) => rename.draw(frame, content, true),
                Foreground::GitIdentity(identity) => identity.draw(frame, content),
                _ => {}
            }
        } else if self.app.foreground.shows_signals() {
            // Fullscreen too: it is the screen the operator is reading, not
            // something laid over the work. A question about a signal is
            // drawn over it, so the thread it is about stays in view.
            screens::signals::draw(frame, content, &mut *self.app);
            modal = Some(content);
        } else if first_run {
            let setup = self.app.setup();
            let tick = self.app.animation_tick();
            if let Foreground::GetStarted(panel) = &mut self.app.foreground {
                screens::get_started::draw(frame, content, panel, &setup, tick);
            } else if self.app.git_identity_over_card()
                || matches!(self.app.foreground, Foreground::ImportLogins(_))
            {
                let mut panel = self.app.started_panel();
                screens::get_started::draw(frame, content, &mut panel, &setup, tick);
            }
            if let CredentialShellState::Opening {
                progress: Some(progress),
                ..
            } = &self.app.credential_shell
            {
                widgets::chrome::dim(frame, content);
                modal = Some(widgets::loading::draw(
                    frame,
                    content,
                    progress,
                    self.app.animation_tick(),
                ));
            }
            if let Foreground::GitIdentity(identity) = &self.app.foreground {
                identity.draw(frame, content);
            }
            modal = modal.or(Some(content));
        }
        // A terminal goes over the screen it was opened from, which is the
        // settings panel for a sign-in and the task list for a task's run. It
        // is drawn after that screen and before the popups above it.
        if let Some(shell) = self.app.open_shell().and_then(|id| self.app.shells.get(id)) {
            shell.draw(frame, content);
        }
        if let Foreground::Setup(setup) = &mut self.app.foreground {
            widgets::chrome::dim(frame, content);
            let report = setup
                .remote_address()
                .and_then(|address| self.app.remote_trackers.get(address))
                .map(Vec::as_slice);
            modal = Some(setup.draw(frame, content, report));
        }
        if let Foreground::SignIn(sign_in) = &self.app.foreground {
            widgets::chrome::dim(frame, content);
            modal = Some(sign_in.draw(frame, content));
        }
        if let Foreground::Confirm(confirm) = &self.app.foreground {
            widgets::chrome::dim(frame, content);
            modal = Some(confirm.draw(frame, content));
        }
        if let Foreground::SendTask(sending) = &self.app.foreground {
            widgets::chrome::dim(frame, content);
            modal = Some(popups::send_task::draw(frame, content, sending));
        }
        if let Foreground::NewTask(new_task) = &self.app.foreground {
            widgets::chrome::dim(frame, content);
            modal = Some(popups::new_task::draw(
                frame,
                content,
                new_task,
                self.app.animation_tick(),
                &mut self.app.new_task_targets,
            ));
        }
        if let Foreground::Message(message) = &self.app.foreground {
            widgets::chrome::dim(frame, content);
            modal = Some(popups::message::draw(frame, content, message));
        }
        if let Foreground::ImportLogins(import) = &self.app.foreground {
            widgets::chrome::dim(frame, content);
            modal = Some(import.draw(frame, content));
        }
        if matches!(self.app.foreground, Foreground::Keys(_)) {
            widgets::chrome::dim(frame, content);
            modal = Some(widgets::chrome::draw_help(
                frame,
                content,
                &help_sections(),
                &mut self.app.help_scroll,
            ));
        }
        // A transfer in flight is the one wait with something to count.
        if let Some(progress) = self.app.transfer_progress() {
            widgets::chrome::dim(frame, content);
            modal = Some(widgets::progress::draw(
                frame,
                content,
                &progress,
                self.app.animation_tick(),
            ));
        }
        // A message is read over whatever asked for it, including a wait.
        if let Some(alert) = &self.app.alert {
            widgets::chrome::dim(frame, content);
            modal = Some(widgets::alert::draw(frame, content, alert));
        }
        // A question about an account holds the screen until it is answered.
        if let Some((title, question, answers)) = self.app.tracker_conflict_question() {
            widgets::chrome::dim(frame, content);
            let answers: Vec<&str> = answers;
            modal = Some(widgets::question::draw(
                frame,
                content,
                &title,
                &question,
                &answers,
                self.app.conflict_choice,
            ));
        }
        // Keep what the interface looks like before the field goes in. That
        // is the frame a later field-only tick starts from, so it never has to
        // lay the interface out again just to move the contours.
        if self.keep_snapshot {
            *self.snapshot = Some(frame.buffer_mut().clone());
        }
        // Paint the field into whatever every screen and modal left open.
        // Nothing has to tell it where those cells are: a widget that means to
        // be opaque writes them, and one that means to show the field does not.
        if self.app.backdrop_animates() {
            self.app.terrain.draw(frame.buffer_mut(), content);
        }
        // Keep the modal's last frame, ready to draw once more when it
        // leaves. Modals arrive at once; only the panel is drawn in.
        if self.app.animations()
            && let Some(area) = modal
        {
            *self.modal_cells = Some(Leaving::take(frame.buffer_mut(), area));
        }
        if let Some(cells) = self.leaving {
            cells.draw(frame.buffer_mut());
        }
        // Off, the manager is emptied rather than left holding effects that
        // would resume mid-flight if it were turned back on.
        if self.app.animations() {
            self.effects
                .process_effects(self.elapsed.into(), frame.buffer_mut(), content);
        } else {
            *self.effects = EffectManager::default();
        }
    }
}
