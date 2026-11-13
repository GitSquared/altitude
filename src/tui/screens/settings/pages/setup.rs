use super::*;

use ratatui::text::{Line, Span};

use crate::{
    project::Project,
    tracker::{Item, TrackerKind},
    tui::{app::TrackerState, widgets::palette},
};

use crate::tui::screens::settings::rows::View;

pub(in crate::tui) fn remote_setup_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::raw("Run "),
            Span::styled("altitude serve", palette::accent()),
            Span::raw(
                " on a headless machine to use it as a remote and keep your agents working even when you’re offline.",
            ),
        ]),
        Line::from(""),
        Line::from(
            "Remotes can be shared with your team if they are on the same network. Each team \
             member gets their own space to run their own agents as they see fit.",
        ),
        Line::from(""),
        Line::from(
            "With an altitude remote server, you can also connect your task tracker to pilot \
             tasks and agents running on the remote directly from where you organize your work, \
             without going through altitude’s terminal interface.",
        ),
        Line::from(""),
        Line::from(vec![
            Span::styled("Set up a machine on your network to run ", palette::muted()),
            Span::styled("altitude serve", palette::accent()),
            Span::styled(
                ", then press n in altitude > Settings > Remotes to add its hostname.",
                palette::muted(),
            ),
        ]),
        Line::from(""),
        Line::styled(
            "Hostname alone uses port 25848 (TCP and UDP). Anyone who can reach it is admitted; use a trusted network.",
            palette::muted(),
        ),
    ]
}

pub(in crate::tui) fn remote_tracker_setup_lines(kind: TrackerKind) -> Vec<Line<'static>> {
    let guide = kind.guide();
    let mut lines = Vec::new();
    for paragraph in guide.intro {
        lines.push(Line::from(""));
        lines.push(Line::from(*paragraph));
    }
    for (index, step) in guide.steps.iter().enumerate() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(format!("{}. ", index + 1), palette::accent()),
            Span::styled(step.title, palette::primary().bold()),
        ]));
        for item in step.items {
            lines.push(match item {
                Item::Text(text) => Line::from(format!("   {text}")),
                Item::Bullet(text) => Line::from(vec![Span::raw("   • "), Span::raw(*text)]),
                Item::Detail(text) => Line::from(vec![Span::raw("     ◦ "), Span::raw(*text)]),
                Item::Code(line) => Line::from(vec![
                    Span::raw("     "),
                    Span::styled(*line, palette::accent()),
                ]),
            });
        }
    }
    for paragraph in guide.closing {
        lines.push(Line::from(""));
        lines.push(Line::styled(*paragraph, palette::muted()));
    }
    lines
}

pub(crate) fn project_lines(
    project: &Project,
    view: &View,
    heading: impl Fn(String) -> Line<'static>,
) -> Vec<Line<'static>> {
    const LABEL: usize = 16;

    fn tag(label: &str, value: String) -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{label:<LABEL$}"), palette::muted()),
            Span::styled(value, palette::accent()),
        ])
    }

    let credentials = &view.credentials;
    let defaults = &view.defaults;
    let label = |id: Option<&str>| {
        let Some(id) = id else {
            return "not set".to_owned();
        };
        credentials
            .iter()
            .find(|credential| credential.id == id)
            .map_or_else(|| id.to_owned(), |credential| credential.label.clone())
    };

    let agent = project
        .agent_credential
        .as_deref()
        .or(defaults.agent.as_deref());
    let vcs = project
        .vcs_credential
        .as_deref()
        .or(defaults.vcs.as_deref());
    let tracker = project.tracker.as_ref().map(|binding| {
        let id = binding
            .credential
            .as_deref()
            .or(defaults.tracker.as_deref());
        let mut value = label(id);
        let scope_noun = id
            .and_then(TrackerKind::of_credential)
            .map_or("scope", TrackerKind::scope_noun);
        match &binding.scope {
            Some(scope) => value.push_str(&format!(" · {scope_noun} {scope}")),
            None => value.push_str(" · whole account"),
        }
        let state = id
            .and_then(|id| view.tracker_states.get(id))
            .copied()
            .unwrap_or_default();
        value.push_str(match state {
            TrackerState::Connected if binding.accepts_delegated_work => {
                " · connected · ready to delegate"
            }
            TrackerState::Connected => " · connected · not accepting delegated work",
            TrackerState::Connecting => " · connecting…",
            TrackerState::NotConnected => " · not connected",
        });
        value
    });

    let mut lines = vec![heading(project.name.clone()), Line::from("")];
    lines.push(tag("agent", label(agent)));
    lines.push(tag("version control", label(vcs)));
    lines.push(tag(
        "task tracker",
        tracker.unwrap_or_else(|| "not configured".to_owned()),
    ));
    lines.push(tag(
        "remote",
        project
            .host
            .clone()
            .unwrap_or_else(|| "this machine".to_owned()),
    ));
    lines.push(tag(
        "repository",
        project
            .repository_display()
            .unwrap_or_else(|| "not set".to_owned()),
    ));
    let commits = project.commits.as_ref().or(defaults.commits.as_ref());
    lines.push(tag(
        "commits",
        commits.map_or_else(
            || "not set".to_owned(),
            |commits| {
                format!(
                    "{} <{}>{}",
                    commits.name,
                    commits.email,
                    if commits.signing_key.is_some() {
                        " · signed"
                    } else {
                        ""
                    }
                )
            },
        ),
    ));

    lines.push(Line::from(""));
    lines.push(Line::styled("setup", palette::muted()));
    if project.setup_command.trim().is_empty() {
        lines.push(Line::styled(
            "Nothing on top of the base image.",
            palette::muted(),
        ));
    } else {
        for command in project.setup_command.trim_end().lines() {
            lines.push(Line::from(command.to_owned()));
        }
    }
    if let Some(description) = view
        .base
        .as_ref()
        .filter(|description| !description.os_release.is_empty())
    {
        lines.push(Line::from(""));
        lines.push(Line::styled("base image", palette::muted()));
        lines.push(Line::from(vec![
            Span::styled(description.os_release.clone(), palette::accent()),
            Span::raw(" · "),
            Span::styled(
                format!("{} packages", description.packages.len()),
                palette::accent(),
            ),
            Span::raw(" · built "),
            Span::styled(met_ago(description.built_at_ms), palette::accent()),
        ]));
        lines.push(Line::styled("packages", palette::muted()));
        lines.push(Line::styled(
            description.packages.join(", "),
            palette::accent(),
        ));
    }
    if let Some(instructions) = &project.instructions {
        lines.push(Line::from(""));
        lines.push(Line::styled("instructions", palette::muted()));
        lines.push(Line::from(instructions.clone()));
    }
    lines
}
