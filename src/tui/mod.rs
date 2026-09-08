use std::{io, time::Duration};

use crossterm::event::{EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, HorizontalAlignment, Layout},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Padding, Paragraph},
};
use throbber_widgets_tui::{BRAILLE_ONE, Throbber, ThrobberState};
use tokio::{signal::unix::Signal, sync::watch, time};

use crate::isolation::Status;

pub async fn run(
    terminal: &mut DefaultTerminal,
    terminate: &mut Signal,
    interrupt: &mut Signal,
    mut isolation_status: watch::Receiver<Status>,
) -> io::Result<()> {
    let mut events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(80));
    let mut spinner = ThrobberState::default();
    let mut status_open = true;

    loop {
        terminal.draw(|frame| draw(frame, &isolation_status.borrow(), &spinner))?;

        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            changed = isolation_status.changed(), if status_open => {
                status_open = changed.is_ok();
            }
            _ = animation.tick(), if isolation_status.borrow().is_pending() => {
                spinner.calc_next();
            }
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                if let Some(key) = event?.as_key_press_event()
                    && (key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)))
                {
                    return Ok(());
                }
            }
        }
    }
}

fn draw(frame: &mut Frame, isolation_status: &Status, spinner: &ThrobberState) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .margin(1)
    .spacing(1)
    .areas(frame.area());

    let [title, version] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(12)]).areas(header);
    frame.render_widget(Line::from("⛰  altitude".cyan().bold()), title);
    frame.render_widget(
        Line::from(concat!("v", env!("CARGO_PKG_VERSION")).dim()).right_aligned(),
        version,
    );
    frame.render_widget(
        Paragraph::new("No tasks yet".dim()).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .padding(Padding::uniform(1)),
        ),
        body,
    );

    let [status, controls] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(15)]).areas(footer);
    frame.render_widget(status_line(isolation_status, spinner), status);
    frame.render_widget(
        Line::from("press q to quit".dim()).alignment(HorizontalAlignment::Right),
        controls,
    );
}

fn status_line(status: &Status, spinner: &ThrobberState) -> Line<'static> {
    match status {
        Status::Starting => pending_status("Starting isolation…", spinner),
        Status::InstallingRuntime => pending_status("Installing isolation runtime…", spinner),
        Status::PreparingBaseImage => pending_status("Preparing Alpine…", spinner),
        Status::Ready => Line::from("Isolation ready".dim()),
        Status::Failed(error) => Line::from(vec![
            Span::styled("Isolation failed: ", Style::new().red()),
            Span::from(error.clone()).dim(),
        ]),
    }
}

fn pending_status(label: &'static str, spinner: &ThrobberState) -> Line<'static> {
    Throbber::default()
        .label(Span::from(label).dim())
        .throbber_set(BRAILLE_ONE)
        .throbber_style(Style::new().cyan())
        .to_line(spinner)
}
