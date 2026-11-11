use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{
    credentials::{Service, Slot, host::HostLogin},
    tui::{
        keys::{is_dismiss, is_secondary},
        screens::settings::{hostname, tilde},
        widgets::{button, chrome, format::truncate, palette},
    },
};

const SUBTITLE: &str = "We detected these existing credentials on the host machine. Import them \
     into altitude? They will be used to run tasks and shared with configured Remotes.";

const OFFER: &str = "↩ to import";
const TAKEN: &str = "✓ imported";

// The logins a setup step found on this machine, offered before its picker opens:
// importing one is the step's own sign-in, so it happens on the row and nothing
// asks twice.
pub struct ImportLogins {
    host: String,
    rows: Vec<Row>,
    selected: usize,
    imported: usize,
    then: Slot,
}

struct Row {
    login: HostLogin,
    imported: bool,
}

pub enum Decision {
    Still,
    Import(HostLogin),
    Close,
}

impl ImportLogins {
    pub fn new(logins: Vec<HostLogin>, then: Slot) -> Self {
        Self {
            host: hostname(),
            rows: logins
                .into_iter()
                .map(|login| Row {
                    login,
                    imported: false,
                })
                .collect(),
            selected: 0,
            imported: 0,
            then,
        }
    }

    pub fn then(&self) -> Slot {
        self.then
    }

    pub fn imported(&self) -> usize {
        self.imported
    }

    pub fn mark_imported(&mut self, service: Service) {
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.login.service == service)
            && !row.imported
        {
            row.imported = true;
            self.imported += 1;
        }
        self.settle();
    }

    pub fn hints(&self) -> Vec<chrome::Hint> {
        vec![
            chrome::Hint::new("↩", "import"),
            chrome::Hint::new("ctrl-↩", if self.imported == 0 { "skip" } else { "done" }),
            chrome::Hint::new("esc", "close"),
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Decision {
        if is_dismiss(key) || is_secondary(key) {
            return Decision::Close;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                if let Some(at) = self.next(true) {
                    self.selected = at;
                }
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                if let Some(at) = self.next(false) {
                    self.selected = at;
                }
            }
            KeyCode::Enter if self.selected_is_takeable() => {
                return Decision::Import(self.rows[self.selected].login.clone());
            }
            _ => {}
        }
        Decision::Still
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) -> Rect {
        let width = area.width.saturating_sub(4).clamp(40, 80);
        let inner_width = width.saturating_sub(4);
        let subtitle_lines = Paragraph::new(SUBTITLE)
            .wrap(Wrap { trim: true })
            .line_count(inner_width) as u16;
        let rows = self.rows.len() as u16;
        let height = chrome::modal_height(subtitle_lines + 1 + rows);
        let popup = chrome::centered_at_most(area, 100, width, height);
        frame.render_widget(Clear, popup);
        let block = chrome::modal(format!("Import from {}?", self.host));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let body = chrome::body(inner);
        let [subtitle_area, _, table] = Layout::vertical([
            Constraint::Length(subtitle_lines),
            Constraint::Length(1),
            Constraint::Min(rows),
        ])
        .areas(body.content);
        frame.render_widget(
            Paragraph::new(SUBTITLE)
                .wrap(Wrap { trim: true })
                .style(palette::primary()),
            subtitle_area,
        );
        button::actions(
            frame,
            popup,
            body.actions,
            &[button::Action::primary(if self.imported == 0 {
                "Skip"
            } else {
                "Done"
            })],
        );

        let name_width = self
            .rows
            .iter()
            .map(|row| row.login.service.label().chars().count())
            .max()
            .unwrap_or(0)
            .max(6);
        let status_width = OFFER.chars().count().max(TAKEN.chars().count());
        let path_width = usize::from(table.width)
            .saturating_sub(2 + name_width + 2 + status_width + 2)
            .max(if table.width == 0 { 0 } else { 1 });
        for (index, row) in self.rows.iter().enumerate() {
            frame.render_widget(
                Paragraph::new(row_line(
                    row,
                    index == self.selected,
                    name_width,
                    path_width,
                )),
                Rect {
                    y: table.y + index as u16,
                    height: 1,
                    ..table
                },
            );
        }
        popup
    }

    fn selected_is_takeable(&self) -> bool {
        self.rows
            .get(self.selected)
            .is_some_and(|row| !row.imported)
    }

    // The selection never rests on a row that has been taken; when the one it was
    // on goes, it moves on to the next that can still be.
    fn settle(&mut self) {
        if self.selected_is_takeable() {
            return;
        }
        if let Some(at) = self.next(false) {
            self.selected = at;
        }
    }

    fn next(&self, forward: bool) -> Option<usize> {
        let count = self.rows.len();
        if count == 0 {
            return None;
        }
        let step = if forward { 1 } else { count - 1 };
        let mut at = self.selected;
        for _ in 0..count {
            at = (at + step) % count;
            if !self.rows[at].imported {
                return Some(at);
            }
        }
        None
    }
}

fn row_line(row: &Row, selected: bool, name_width: usize, path_width: usize) -> Line<'static> {
    let name = row.login.service.label();
    let path = truncate(&tilde(&row.login.source.display().to_string()), path_width);
    let status = if row.imported { TAKEN } else { OFFER };
    let name_style = if selected {
        palette::accent().bold()
    } else {
        palette::primary()
    };
    let status_style = if row.imported {
        palette::primary().bold()
    } else {
        palette::muted()
    };
    Line::from(vec![
        Span::styled(
            if selected { "› " } else { "  " },
            if selected {
                palette::accent().bold()
            } else {
                palette::muted()
            },
        ),
        Span::styled(format!("{name:<name_width$}"), name_style),
        Span::raw("  "),
        Span::styled(format!("{path:<path_width$}"), palette::muted()),
        Span::raw("  "),
        Span::styled(
            format!(
                "{status:>width$}",
                width = OFFER.chars().count().max(TAKEN.chars().count())
            ),
            status_style,
        ),
    ])
}
