//! Choosing the machine that will continue a task that is already here.
//!
//! A transfer is not a copy: the task stops here, its capsule travels, and it
//! continues there. So this page names the task it is about and the machines that
//! could take it, and nothing leaves until enter is pressed. Escape is a way out
//! that sends nothing.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    agent::AgentKind,
    tui::{
        keys,
        popups::new_task::Host,
        widgets::{chrome, palette, utils::truncate},
    },
};

/// The task being sent, and the machines that could receive it.
pub(in crate::tui) struct SendTask {
    task_id: String,
    title: String,
    environment: String,
    agent: Option<AgentKind>,
    /// The remotes only: this machine is where the task already is.
    hosts: Vec<Host>,
    selected: usize,
}

/// What the operator answered.
pub(in crate::tui) enum Decision {
    Cancel,
    /// There is no machine to send to yet, so open the settings that add one.
    Settings,
    Send {
        address: String,
    },
}

impl SendTask {
    pub(in crate::tui) fn new(
        task_id: String,
        title: String,
        environment: String,
        agent: Option<AgentKind>,
        hosts: Vec<Host>,
    ) -> Self {
        // Start on the first machine that can take it, so enter is never a no-op
        // on a page whose whole point is choosing one.
        let selected = hosts.iter().position(|host| host.ready).unwrap_or(0);
        Self {
            task_id,
            title,
            environment,
            agent,
            hosts,
            selected,
        }
    }

    pub(in crate::tui) fn task_id(&self) -> &str {
        &self.task_id
    }

    pub(in crate::tui) fn title(&self) -> &str {
        &self.title
    }

    /// Put the machines back in step with what the remotes have answered, keeping
    /// the one the operator chose where it is still offered.
    pub(in crate::tui) fn refresh_hosts(&mut self, hosts: Vec<Host>) {
        let chosen = self
            .hosts
            .get(self.selected)
            .and_then(|host| host.address.clone());
        self.hosts = hosts;
        let kept = chosen.and_then(|address| {
            self.hosts
                .iter()
                .position(|host| host.address.as_deref() == Some(address.as_str()))
        });
        self.selected = match kept {
            Some(index) if self.hosts[index].ready => index,
            _ => self.hosts.iter().position(|host| host.ready).unwrap_or(0),
        };
    }

    pub(in crate::tui) fn hints(&self) -> Vec<chrome::Hint> {
        let mut hints = Vec::new();
        if self.any_ready() {
            hints.push(chrome::Hint::new("enter", "send"));
        } else {
            hints.push(chrome::Hint::new("s", "settings"));
        }
        if self.hosts.len() > 1 {
            hints.push(chrome::Hint::new("j/k", "choose"));
        }
        hints.push(chrome::Hint::new("esc", "cancel"));
        hints
    }

    pub(in crate::tui) fn handle_key(&mut self, key: KeyEvent) -> Option<Decision> {
        if keys::is_dismiss(key) {
            return Some(Decision::Cancel);
        }
        match key.code {
            KeyCode::Char('s') if !self.any_ready() => Some(Decision::Settings),
            KeyCode::Down | KeyCode::Char('j') => {
                self.step(true);
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.step(false);
                None
            }
            KeyCode::Enter => self.chosen().map(|address| Decision::Send { address }),
            _ => None,
        }
    }

    fn any_ready(&self) -> bool {
        self.hosts.iter().any(|host| host.ready)
    }

    /// Step to the next machine that can take work, rather than onto one that
    /// cannot: choosing it would mean enter does nothing.
    fn step(&mut self, forward: bool) {
        let count = self.hosts.len();
        if count == 0 {
            return;
        }
        for offset in 1..=count {
            let candidate = if forward {
                (self.selected + offset) % count
            } else {
                (self.selected + count - offset) % count
            };
            if self.hosts[candidate].ready {
                self.selected = candidate;
                return;
            }
        }
    }

    fn chosen(&self) -> Option<String> {
        let host = self.hosts.get(self.selected)?;
        host.ready.then(|| host.address.clone()).flatten()
    }
}

pub(in crate::tui) fn draw(frame: &mut Frame, area: Rect, sending: &SendTask) -> Rect {
    let popup = chrome::centered(area, 60, 15);
    frame.render_widget(Clear, popup);
    let block = chrome::modal("Send to a remote");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 6 {
        return popup;
    }
    let [summary_area, list_area, note_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .spacing(1)
    .areas(inner);

    let agent = sending
        .agent
        .map_or_else(|| "no agent".to_owned(), |kind| kind.to_string());
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                truncate(&sending.title, usize::from(summary_area.width)),
                palette::heading(),
            ),
            Line::styled(
                format!("{} · {agent}", sending.environment),
                palette::muted(),
            ),
        ])
        .wrap(Wrap { trim: true }),
        summary_area,
    );

    if sending.hosts.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No remotes are configured.", palette::attention()),
                Line::styled("Press s to add one in Settings.", palette::muted()),
            ])
            .wrap(Wrap { trim: true }),
            list_area,
        );
        return popup;
    }
    let width = usize::from(list_area.width);
    let items = sending
        .hosts
        .iter()
        .enumerate()
        .map(|(index, host)| {
            let style = if !host.ready {
                palette::attention()
            } else if index == sending.selected {
                palette::focus()
            } else {
                palette::primary()
            };
            ListItem::new(Line::styled(truncate(&host.label, width), style))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(Some(sending.selected));
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("❯ "),
        list_area,
        &mut state,
    );

    frame.render_widget(
        Paragraph::new("It stops on this machine and continues on the remote.")
            .style(palette::muted())
            .wrap(Wrap { trim: true }),
        note_area,
    );
    popup
}
