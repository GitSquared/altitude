use std::{
    io::{self, Write},
    mem,
};

use ratatui::{DefaultTerminal, Frame, buffer::Buffer, layout::Rect};
use tachyonfx::{
    Effect, Interpolation, fx,
    pattern::{InvertedPattern, RadialPattern, SweepPattern},
};

use crate::tui::App;
use crate::tui::background::terrain;

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
