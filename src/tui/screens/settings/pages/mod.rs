use std::{
    env,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use ratatui::text::{Line, Span};

use crate::{
    agent::AgentKind,
    credentials::Service,
    remote::{TrackerConnection, TrackerReadiness},
    tui::widgets::{
        format::plain,
        palette,
        shimmer::{Dot, shimmer},
    },
};

use super::rows::mark;

mod connectors;
mod setup;

pub(crate) use connectors::*;
pub(crate) use setup::*;

pub(super) fn section_heading(text: &str) -> Line<'static> {
    Line::styled(text.to_owned(), palette::primary().bold())
}

pub(super) fn used_by_lines(projects: &[String]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(""), section_heading("Used by")];
    if projects.is_empty() {
        lines.push(Line::styled("No projects", palette::muted()));
    } else {
        lines.extend(projects.iter().map(|project| {
            Line::from(vec![
                Span::styled("• ", palette::muted()),
                Span::styled(project.clone(), palette::accent()),
            ])
        }));
    }
    lines
}

pub(super) fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), palette::muted()),
        Span::styled(value, palette::muted()),
    ])
}

pub(super) struct Connector {
    title: String,
    reach: Reach,
    status: String,
    status_value: Option<String>,
    identity: Option<String>,
    note: Option<String>,
    action: Option<&'static str>,
    purpose: &'static str,
    used_by: Option<Vec<String>>,
    fields: Vec<(&'static str, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Reach {
    On,
    Off,
    Asking,
}

impl Connector {
    pub(super) fn lines(
        self,
        heading: impl Fn(String) -> Line<'static>,
        tick: Option<usize>,
    ) -> Vec<Line<'static>> {
        let status = self.status_line(tick);
        let mut lines = vec![heading(self.title), Line::from(""), status];
        if let Some(action) = self.action {
            lines.push(Line::from(""));
            lines.push(Line::styled(action, palette::accent()));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(self.purpose));
        if let Some(projects) = self.used_by {
            lines.extend(used_by_lines(&projects));
        }
        if !self.fields.is_empty() {
            lines.push(Line::from(""));
            lines.extend(
                self.fields
                    .into_iter()
                    .map(|(label, value)| field(label, value)),
            );
        }
        if let Some(note) = self.note {
            lines.push(Line::from(""));
            lines.push(Line::styled(note, palette::muted()));
        }
        lines
    }

    fn status_line(&self, tick: Option<usize>) -> Line<'static> {
        let mut spans = mark(self.reach == Reach::On, self.reach == Reach::Asking).spans(tick);
        if self.reach == Reach::Asking {
            spans.extend(shimmer(&self.status, tick));
        } else {
            let style = if self.reach == Reach::On {
                palette::primary().bold()
            } else {
                palette::muted().bold()
            };
            spans.push(Span::styled(self.status.clone(), style));
        }
        if let Some(value) = &self.status_value {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(value.clone(), palette::accent()));
        }
        if let Some(identity) = &self.identity {
            spans.push(Span::styled(" as ", palette::primary().bold()));
            spans.push(Span::styled(identity.clone(), palette::accent()));
        }
        Line::from(spans)
    }
}

pub(in crate::tui) fn tracker_dot(is_ready: Option<bool>) -> Dot {
    match is_ready {
        Some(true) => Dot::still('●', palette::primary()),
        Some(false) => Dot::still('○', palette::muted()),
        None => Dot::still('○', palette::muted()),
    }
}

pub(super) fn tracker_summary(
    trackers: Option<&[TrackerConnection]>,
    tick: Option<usize>,
) -> Line<'static> {
    let Some(connections) = trackers else {
        let mut spans = tracker_dot(None).spans(tick);
        spans.push(Span::styled("unknown", palette::muted().bold()));
        return Line::from(spans);
    };
    // Answered, and answered nothing: this machine runs no shared app. Not a
    // failure, and not a reason to name a provider that is not configured.
    if connections.is_empty() {
        let mut spans = tracker_dot(None).spans(tick);
        spans.push(Span::styled("none configured", palette::muted().bold()));
        return Line::from(spans);
    }
    // How far the remote's owner got, not just that something is wrong: an app
    // that was never installed and one that will not sign in send them to
    // different steps.
    let ready = connections
        .iter()
        .any(|connection| connection.state == TrackerReadiness::Ready);
    let said = connections
        .iter()
        .map(|connection| match connection.state {
            TrackerReadiness::Ready => connection.provider.label().to_owned(),
            state => format!(
                "{} · {} on the remote",
                connection.provider.label(),
                state.said()
            ),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut spans = tracker_dot(Some(ready)).spans(tick);
    spans.push(Span::styled(
        said,
        if ready {
            palette::primary().bold()
        } else {
            palette::muted().bold()
        },
    ));
    Line::from(spans)
}

pub(super) fn profile_purpose(profile: &str) -> &'static str {
    match Service::from_id(profile) {
        Some(Service::VersionControl(_)) => {
            "The GitHub CLI login your tasks clone, push, and read pull requests with."
        }
        Some(Service::Agent(AgentKind::Pi)) => "Pi uses your provider account to run tasks.",
        Some(Service::Agent(AgentKind::Claude)) => "Claude Code uses your account to run tasks.",
        _ => "A login a task is given.",
    }
}

pub(super) fn public_identity(profile: &str, output: &str) -> Option<String> {
    match Service::from_id(profile)? {
        Service::VersionControl(_) => github_account(output),
        _ => None,
    }
}

pub(super) fn github_account(output: &str) -> Option<String> {
    const MARKERS: [&str; 2] = [
        "Logged in to github.com as ",
        "Logged in to github.com account ",
    ];
    for line in plain(output).lines() {
        for marker in MARKERS {
            let Some((_, rest)) = line.split_once(marker) else {
                continue;
            };
            let Some(name) = rest.split_whitespace().next() else {
                continue;
            };
            if name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Some(name.to_owned());
            }
        }
    }
    None
}

pub(super) fn hostname() -> String {
    let name: String = whoami::hostname()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    if name.is_empty() {
        "unknown".to_owned()
    } else {
        name
    }
}

pub(super) fn tilde(path: &str) -> String {
    let Some(home) = env::var_os("HOME") else {
        return path.to_owned();
    };
    let Ok(rest) = Path::new(path).strip_prefix(Path::new(&home)) else {
        return path.to_owned();
    };
    if rest.as_os_str().is_empty() {
        return "~".to_owned();
    }
    Path::new("~").join(rest).display().to_string()
}

pub(super) fn short_key(key: &str) -> String {
    const KEEP: usize = 12;
    let head = key.chars().take(KEEP).collect::<String>();
    if key.chars().count() > KEEP {
        format!("{head}…")
    } else {
        head
    }
}

pub(super) fn met_ago(seen_at_ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default();
    let minutes = now.saturating_sub(seen_at_ms).max(0) / 60_000;
    match minutes {
        0..=1 => "just now".to_owned(),
        2..=59 => format!("{} ago", counted(minutes, "minute")),
        60..=2879 => format!("{} ago", counted(minutes / 60, "hour")),
        _ => format!("{} ago", counted(minutes / 1_440, "day")),
    }
}

pub(super) fn counted(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}
