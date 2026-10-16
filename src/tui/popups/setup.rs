use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, Padding, Paragraph, Wrap},
};

use crate::{
    remote::TrackerConnection,
    tracker::TrackerKind,
    tui::{
        screens::settings::{remote_tracker_setup_lines, tracker_dot},
        widgets::{chrome, palette},
    },
};

pub enum SetupKind {
    Remote,
    RemoteTrackers,
    RemoteTrackerGuide(TrackerKind),
    Linear,
}

struct TrackerOption {
    provider: TrackerKind,
    is_ready: Option<bool>,
}

impl TrackerOption {
    fn status(&self) -> &'static str {
        match self.is_ready {
            Some(true) => "configured",
            Some(false) => "not connected",
            None => "unknown",
        }
    }
}

/// The supported task trackers, one row each, with the remote's own answer about
/// whether it runs them. A remote that has not answered leaves them unknown
/// rather than guessed.
fn candidates(report: Option<&[TrackerConnection]>) -> Vec<TrackerOption> {
    TrackerKind::ALL
        .iter()
        .map(|&provider| TrackerOption {
            provider,
            is_ready: report.map(|connections| {
                connections
                    .iter()
                    .find(|connection| connection.provider == provider)
                    .is_some_and(|connection| connection.is_ready)
            }),
        })
        .collect()
}

pub struct Setup {
    pub kind: SetupKind,
    lines: Vec<Line<'static>>,
    scroll: u16,
    /// The provider list a guide was opened from, so dismissing the guide returns
    /// to it rather than all the way out.
    picker: Option<Picker>,
}

struct Picker {
    address: String,
    selected: usize,
}

impl Setup {
    pub fn new(kind: SetupKind, lines: Vec<Line<'static>>) -> Self {
        Self {
            kind,
            lines,
            scroll: 0,
            picker: None,
        }
    }

    pub fn tracker_list(address: String) -> Self {
        Self {
            kind: SetupKind::RemoteTrackers,
            lines: Vec::new(),
            scroll: 0,
            picker: Some(Picker {
                address,
                selected: 0,
            }),
        }
    }

    pub fn remote_address(&self) -> Option<&str> {
        self.picker.as_ref().map(|picker| picker.address.as_str())
    }

    pub fn is_picker(&self) -> bool {
        matches!(self.kind, SetupKind::RemoteTrackers)
    }

    pub fn move_selection(&mut self, forward: bool) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        let count = TrackerKind::ALL.len();
        if count == 0 {
            return;
        }
        picker.selected = if forward {
            (picker.selected + 1) % count
        } else {
            (picker.selected + count - 1) % count
        };
    }

    pub fn open_selected(&mut self) {
        if !self.is_picker() {
            return;
        }
        let Some(picker) = &self.picker else {
            return;
        };
        let provider = TrackerKind::ALL[picker.selected];
        self.kind = SetupKind::RemoteTrackerGuide(provider);
        self.lines = remote_tracker_setup_lines(provider);
        self.scroll = 0;
    }

    /// Step back one level: from a provider's guide to the list. Returns whether
    /// the helper stays open.
    pub fn back(&mut self) -> bool {
        if matches!(self.kind, SetupKind::RemoteTrackerGuide(_)) && self.picker.is_some() {
            self.kind = SetupKind::RemoteTrackers;
            self.lines.clear();
            self.scroll = 0;
            return true;
        }
        false
    }

    pub fn scroll(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = u16::MAX,
            _ => {}
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, report: Option<&[TrackerConnection]>) {
        if self.is_picker() {
            self.draw_picker(frame, area, report);
        } else {
            self.draw_guide(frame, area);
        }
    }

    fn draw_picker(&self, frame: &mut Frame, area: Rect, report: Option<&[TrackerConnection]>) {
        let Some(picker) = &self.picker else {
            return;
        };
        let options = candidates(report);
        let height = options.len() as u16 + 2;
        let popup = chrome::centered(area, 75, height);
        let block = Block::bordered()
            .title(" Choose a task tracker ")
            .title_bottom(
                Line::from(" ↑/↓ · j/k choose · Enter open · Esc/q close ")
                    .right_aligned()
                    .style(palette::muted()),
            )
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        let width = usize::from(inner.width);
        let items = options
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                ListItem::new(candidate_line(candidate, width, index == picker.selected))
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(items), inner);
    }

    fn draw_guide(&mut self, frame: &mut Frame, area: Rect) {
        let popup = chrome::centered(area, 85, area.height.saturating_sub(2));
        let block = Block::bordered()
            .title(self.title())
            .title_bottom(
                Line::from(self.footer())
                    .right_aligned()
                    .style(palette::muted()),
            )
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        let paragraph = Paragraph::new(self.lines.clone()).wrap(Wrap { trim: false });
        let maximum = paragraph
            .line_count(inner.width)
            .saturating_sub(usize::from(inner.height));
        self.scroll = self.scroll.min(maximum.min(usize::from(u16::MAX)) as u16);
        frame.render_widget(paragraph.scroll((self.scroll, 0)), inner);
    }

    fn title(&self) -> Line<'static> {
        match &self.kind {
            SetupKind::Remote => Line::styled(" Altitude remote guide ", palette::heading()),
            SetupKind::RemoteTrackers => Line::from(" Choose a task tracker "),
            SetupKind::RemoteTrackerGuide(kind) => {
                Line::from(format!(" Set up {} on the remote ", kind.label()))
            }
            SetupKind::Linear => Line::from(" Sign in to Linear "),
        }
    }

    fn footer(&self) -> &'static str {
        match &self.kind {
            SetupKind::RemoteTrackers => " ↑/↓ · j/k choose · Enter open · Esc/q close ",
            SetupKind::RemoteTrackerGuide(_) => " ↑/↓ · j/k · PgUp/PgDn scroll · Esc/q back ",
            _ => " ↑/↓ · j/k · PgUp/PgDn scroll · Esc/q close ",
        }
    }
}

fn candidate_line(candidate: &TrackerOption, width: usize, focused: bool) -> Line<'static> {
    let status = candidate.status();
    let label = candidate.provider.label();
    let mut spans = tracker_dot(candidate.is_ready).spans(0);
    let used = spans.iter().map(Span::width).sum::<usize>() + label.len() + status.len();
    let padding = " ".repeat(width.saturating_sub(used).max(1));
    if focused {
        spans.push(Span::styled(
            format!("{label}{padding}{status}"),
            palette::focus(),
        ));
    } else {
        spans.push(Span::raw(label));
        spans.push(Span::raw(padding));
        spans.push(Span::styled(status, palette::muted()));
    }
    Line::from(spans)
}
