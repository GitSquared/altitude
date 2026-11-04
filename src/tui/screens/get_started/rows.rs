use super::*;


use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::{
    tui::widgets::{
            format::truncate,
            palette,
            select::{Offer, Select},
        },
    vcs::Repository,
};


pub(super) fn row_spans(
    marker: Marker,
    label: &str,
    value: Vec<Span<'static>>,
    action: &str,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let prefix = format!("{}  {label:<LABEL_WIDTH$}  ", marker.glyph());
    let trailing = format!("{action:<ACTION_WIDTH$}");
    let available = usize::from(width)
        .saturating_sub(prefix.chars().count())
        .saturating_sub(trailing.chars().count());
    let used: usize = value.iter().map(Span::width).sum();
    let pad = available.saturating_sub(used);
    let mut spans = vec![Span::styled(
        prefix,
        if focused {
            palette::primary().bold()
        } else {
            palette::muted()
        },
    )];
    spans.extend(value);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(trailing, palette::muted()));
    Line::from(spans)
}

pub(super) fn repository_select(
    cached: &[Repository],
    current: Option<&str>,
) -> Select<Repository> {
    let mut offers: Vec<Offer<Repository>> = cached
        .iter()
        .map(|repository| Offer::new(repository.clone(), repository.name.clone()))
        .collect();
    match current {
        Some(url) => {
            if !offers.iter().any(|offer| offer.value.url == url) {
                offers.insert(
                    0,
                    Offer::new(
                        Repository {
                            name: url.to_owned(),
                            url: url.to_owned(),
                            is_private: false,
                        },
                        url,
                    ),
                );
            }
        }
        None => {
            // Not ready, so the pill draws it as the question it is: the mark
            // says it opens, the value says it has not been picked.
            let mut offer = Offer::new(
                Repository {
                    name: "repository not picked".to_owned(),
                    url: String::new(),
                    is_private: false,
                },
                "repository not picked",
            );
            offer.ready = false;
            offers.insert(0, offer);
        }
    }
    let chosen = current
        .and_then(|url| offers.iter().position(|offer| offer.value.url == url))
        .unwrap_or(0);
    Select::new("repository", offers, chosen)
}

pub(super) fn row(
    marker: Marker,
    label: &str,
    value: &str,
    action: &str,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let prefix = format!("{}  {label:<LABEL_WIDTH$}  ", marker.glyph());
    let trailing = format!("{action:<ACTION_WIDTH$}");
    let available = usize::from(width)
        .saturating_sub(prefix.chars().count())
        .saturating_sub(trailing.chars().count());
    let value = truncate(value, available);
    let style = if focused {
        palette::primary().bold()
    } else {
        palette::primary()
    };
    Line::from(vec![
        Span::styled(
            prefix,
            if focused {
                palette::primary().bold()
            } else {
                palette::muted()
            },
        ),
        Span::styled(format!("{value:<available$}"), style),
        Span::styled(trailing, palette::muted()),
    ])
}

pub(super) fn commits_editor(
    commits: &Commits,
    setup: &SetupState,
    width: u16,
) -> Vec<Line<'static>> {
    let indent = " ".repeat(LABEL_WIDTH + 7);
    let name = if commits.field == CommitsField::Name {
        commits.name.marked()
    } else {
        commits.name.value().to_owned()
    };
    let email = if commits.field == CommitsField::Email {
        commits.email.marked()
    } else {
        commits.email.value().to_owned()
    };
    let sign = if commits.signs {
        "● on".to_owned()
    } else {
        "○ off".to_owned()
    };
    let key = if setup.has_key {
        "present · enter show".to_owned()
    } else {
        "none · enter generate".to_owned()
    };
    vec![
        editor_line(
            &indent,
            "Name",
            &name,
            commits.field == CommitsField::Name,
            width,
        ),
        editor_line(
            &indent,
            "Email",
            &email,
            commits.field == CommitsField::Email,
            width,
        ),
        editor_line(
            &indent,
            "Sign commits",
            &sign,
            commits.field == CommitsField::Sign,
            width,
        ),
        editor_line(
            &indent,
            "Key",
            &key,
            commits.field == CommitsField::Key,
            width,
        ),
    ]
}

pub(super) fn editor_line(
    indent: &str,
    label: &str,
    value: &str,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let prefix = format!("{indent}{label:<12} ");
    let available = usize::from(width).saturating_sub(prefix.chars().count());
    let value = truncate(value, available);
    let style = if focused {
        palette::primary().bold()
    } else {
        palette::primary()
    };
    Line::from(vec![
        Span::styled(
            prefix,
            if focused {
                palette::primary().bold()
            } else {
                palette::muted()
            },
        ),
        Span::styled(format!("{value:<available$}"), style),
    ])
}

pub(super) fn row_anchor(rows: Rect, index: u16) -> Rect {
    Rect {
        y: rows.y.saturating_add(index),
        height: 1,
        ..rows
    }
}

pub(super) fn detail_text(focus: Focus, setup: &SetupState) -> String {
    let vcs = setup.vcs.label();
    match focus {
        Focus::Name => {
            "Projects bind together agents, code and task trackers into a single setup. You can \
             have multiple projects set up in altitude, for instance one for your day job and one \
             for your weekend side-project. Let's name your first project now."
                .to_owned()
        }
        Focus::Agent if setup.agent.is_none() => {
            "altitude spins up your usual coding agent and drives it autonomously to complete \
             work for you. You can always take back manual control and interact with the agent's \
             own interface directly when needed."
                .to_owned()
        }
        Focus::Agent => {
            format!(
                "Signing in happens through {}, in a sandbox of its own. What it writes is kept \
                 here and mounted into every task, so the agent is signed in once.",
                setup
                    .agent
                    .map(|agent| agent.to_string())
                    .unwrap_or_else(|| "the agent".to_owned())
            )
        }
        Focus::VersionControl if !signed_in(&setup.vcs_status) => {
            format!(
                "altitude runs your agents in isolated sandboxes. Pick a repository so they can \
                 pull your project's code and send back change suggestions. This needs a {vcs} \
                 sign-in first."
            )
        }
        Focus::VersionControl if setup.repository.is_none() => {
            "altitude runs your agents in isolated sandboxes. Pick a repository so they can pull \
             your project's code and send back change suggestions."
                .to_owned()
        }
        Focus::VersionControl => {
            "The repository a task starts from and pushes back to. Changing it changes where the \
             next task's work goes."
                .to_owned()
        }
        Focus::Commits if setup.commits.is_set() => {
            "Commits carry your name and email, and a signature proves they came from you. Enter \
             edits them here; the key is made once and stays in your version-control profile, so \
             every task commits the same way."
                .to_owned()
        }
        Focus::Commits => {
            "A commit needs a name and an email before a task can deliver its work. Enter edits \
             them here; signing is set up from the same place, with a key that stays in your \
             version-control profile."
                .to_owned()
        }
        Focus::Tracker => {
            "Optional: connect your task tracker to link issues directly to altitude agents and \
             sync status back automatically."
                .to_owned()
        }
        Focus::Remote => {
            "Optional: unlock better workflows, like running tasks when you're away and piloting \
             them entirely from your task tracker, by setting up an altitude remote server and \
             sharing it with your team.\n\
             If a remote is accessible on your network, it will show up here. Learn more about \
             setting up a remote for your team in Settings."
                .to_owned()
        }
        Focus::Ready | Focus::Settings => String::new(),
    }
}

pub(super) fn wrapped_height(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    Paragraph::new(text.to_owned())
        .wrap(Wrap { trim: true })
        .line_count(width) as u16
}
