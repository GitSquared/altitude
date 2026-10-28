use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Clear, List, ListItem, Paragraph, Wrap},
};

use crate::{
    remote::{TrackerConnection, TrackerReadiness},
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
    SigningKey,
}

struct TrackerOption {
    provider: TrackerKind,
    /// The mark: on, off, or nothing to say yet.
    is_ready: Option<bool>,
    status: &'static str,
}

/// The supported task trackers, one row each, saying how far the remote's own
/// setup got. A remote that has not answered leaves them unknown rather than
/// guessed, and one that answered with no tracker at all has none configured.
fn candidates(report: Option<&[TrackerConnection]>) -> Vec<TrackerOption> {
    TrackerKind::ALL
        .iter()
        .map(|&provider| {
            let connection = report.and_then(|connections| {
                connections
                    .iter()
                    .find(|connection| connection.provider == provider)
            });
            let (is_ready, status) = match connection.map(|connection| connection.state) {
                None if report.is_none() => (None, "unknown"),
                None => (Some(false), "not configured"),
                Some(state) => (Some(state == TrackerReadiness::Ready), state.said()),
            };
            TrackerOption {
                provider,
                is_ready,
                status,
            }
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
    /// What `c` puts on the clipboard, when the page is showing something to take
    /// away rather than something to read.
    copy: Option<String>,
    /// Whether closing goes back to the first-run card rather than the panel.
    returns_to_card: bool,
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
            copy: None,
            returns_to_card: false,
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
            copy: None,
            returns_to_card: false,
        }
    }

    /// Make `c` copy this text, and say so in the hints.
    pub fn with_copy(mut self, text: String) -> Self {
        self.copy = Some(text);
        self
    }

    pub fn copy_text(&self) -> Option<&str> {
        self.copy.as_deref()
    }

    /// Close back to the first-run card, which is what opened this page.
    pub fn returning_to_card(mut self) -> Self {
        self.returns_to_card = true;
        self
    }

    pub fn returns_to_card(&self) -> bool {
        self.returns_to_card
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

    /// Move the guide by one wheel notch.
    pub(in crate::tui) fn wheel(&mut self, up: bool, rows: u16) {
        self.scroll = if up {
            self.scroll.saturating_sub(rows)
        } else {
            self.scroll.saturating_add(rows)
        };
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        report: Option<&[TrackerConnection]>,
    ) -> Rect {
        if self.is_picker() {
            self.draw_picker(frame, area, report)
        } else {
            self.draw_guide(frame, area)
        }
    }

    fn draw_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        report: Option<&[TrackerConnection]>,
    ) -> Rect {
        let Some(picker) = &self.picker else {
            return Rect::default();
        };
        let options = candidates(report);
        let height = options.len() as u16 + 2;
        let popup = chrome::centered(area, 75, height);
        let block = chrome::modal("Choose a task tracker");
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
        popup
    }

    fn draw_guide(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let popup = chrome::centered(area, 85, area.height.saturating_sub(2));
        let block = chrome::modal(self.title());
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        let paragraph = Paragraph::new(self.lines.clone()).wrap(Wrap { trim: false });
        let maximum = paragraph
            .line_count(inner.width)
            .saturating_sub(usize::from(inner.height));
        self.scroll = self.scroll.min(maximum.min(usize::from(u16::MAX)) as u16);
        frame.render_widget(paragraph.scroll((self.scroll, 0)), inner);
        popup
    }

    fn title(&self) -> String {
        match &self.kind {
            SetupKind::Remote => "Altitude remote guide".to_owned(),
            SetupKind::RemoteTrackers => "Choose a task tracker".to_owned(),
            SetupKind::RemoteTrackerGuide(kind) => {
                format!("Set up {} on the remote", kind.label())
            }
            SetupKind::SigningKey => "Signing key".to_owned(),
        }
    }
}

fn candidate_line(candidate: &TrackerOption, width: usize, focused: bool) -> Line<'static> {
    let status = candidate.status;
    let label = candidate.provider.label();
    let mut spans = tracker_dot(candidate.is_ready).spans(None);
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
