//! What the third column says about each row, and the words the setup helpers
//! print. A page is one shape: what the thing is, whether it is connected, and
//! the one next step.

use std::{
    env,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use ratatui::text::{Line, Span};

use crate::{
    agent::AgentKind,
    credentials::Service,
    project::Project,
    remote::{RemoteKey, TrackerConnection, TrackerReadiness},
    tracker::{TrackerKind, setup::Item},
    tui::{
        app::{CredentialStatus, RemoteStatus, TrackerState},
        widgets::{
            palette,
            utils::{Dot, plain, shimmer},
        },
    },
};

use super::rows::{Credential, CredentialKind, GitIdentity, View, mark};

/// A small heading inside a page, so a page that has to say several things can
/// be read in the order it means them.
pub(super) fn section_heading(text: &str) -> Line<'static> {
    Line::styled(text.to_owned(), palette::primary().bold())
}

/// The projects a credential is effectively bound to, named in the one colour
/// the panel uses for a value that belongs to the operator.
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

fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), palette::muted()),
        Span::styled(value, palette::muted()),
    ])
}

/// The third column's page for anything with a connection to maintain: a login,
/// a remote, or this installation.
///
/// They all open the same way — what it is, whether it is connected, and the one
/// next step — so an operator reads one shape rather than five, and the
/// technical detail waits at the bottom where it does not push the status down.
pub(super) struct Connector {
    title: String,
    reach: Reach,
    status: String,
    status_value: Option<String>,
    /// A public name the provider reports, when the check printed one. Never
    /// guessed: absent when the report does not name one.
    identity: Option<String>,
    note: Option<String>,
    action: Option<&'static str>,
    purpose: &'static str,
    /// Projects whose effective binding is this identity, when it is one. A
    /// remote or an installation has no projects of its own.
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

    /// The status is the page's verdict, so it is bold and monochrome with the
    /// same dot the list uses; only the value beside it, if any, is accented.
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
/// Instructions shown after opening the remote setup helper.
///
/// Only getting a remote running: the shared task tracker is set up afterwards,
/// on the remote it belongs to, and lives in its own helper.
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

/// The provider's manual, styled for this panel.
///
/// The words are the ones `altitude serve connect linear` prints on the machine
/// that performs the steps, so the two cannot drift. Nothing here depends on what
/// a particular remote is configured with: the guide is the same everywhere, and
/// the values that fill it in are printed where they belong.
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

/// A project's page: what the work is, and the machine state it runs in.
///
/// Read here and edited in the form, so the page always says what the file says
/// rather than what a form remembers.
pub(super) fn project_lines(
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
    let tracker = project.tracker.as_ref().map(|route| {
        let id = route.credential.as_deref().or(defaults.tracker.as_deref());
        let mut value = label(id);
        let kind = TrackerKind::sole();
        match &route.scope {
            Some(scope) => value.push_str(&format!(" · {} {scope}", kind.scope_noun())),
            None => value.push_str(" · whole account"),
        }
        let state = id
            .and_then(|id| view.tracker_states.get(id))
            .copied()
            .unwrap_or_default();
        value.push_str(match state {
            TrackerState::Connected if route.accepts_delegated_work => {
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
            .repository_label()
            .or_else(|| project.repository.clone())
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

/// A login a task signs in with, as its page reads it. The same shape serves a
/// coding agent and the GitHub CLI.
pub(super) fn profile_connector(
    title: String,
    purpose: &'static str,
    profile: &str,
    directory: &str,
    status: &CredentialStatus,
) -> Connector {
    let reach = match status {
        CredentialStatus::Checking => Reach::Asking,
        CredentialStatus::Checked {
            signed_in: true, ..
        }
        | CredentialStatus::Saved => Reach::On,
        // Not asked, or nothing to ask: neither is a sign-out.
        CredentialStatus::Checked {
            signed_in: false, ..
        }
        | CredentialStatus::Unknown
        | CredentialStatus::Unanswered => Reach::Off,
    };
    let word = status.summary();
    let note = matches!(status, CredentialStatus::Saved).then(|| {
        "A login is saved on this machine. Nothing has been built here to ask the \
         tool whether it still works."
            .to_owned()
    });
    let identity = match status {
        CredentialStatus::Checked {
            signed_in: true,
            output,
        } => public_identity(profile, output),
        _ => None,
    };
    Connector {
        title,
        reach,
        status: word.to_owned(),
        status_value: None,
        identity,
        note,
        action: matches!(
            status,
            CredentialStatus::Checked {
                signed_in: false,
                ..
            } | CredentialStatus::Unanswered
        )
        .then_some("Press enter to sign in."),
        purpose,
        used_by: None,
        fields: vec![("saved at", tilde(directory))],
    }
}

pub(super) fn credential_connector(credential: &Credential) -> Connector {
    let mut connector = match &credential.kind {
        CredentialKind::Installation {
            hostname,
            label,
            key,
            data_dir,
        } => Connector {
            title: format!("{hostname} (this machine)"),
            reach: Reach::On,
            status: "identity".to_owned(),
            status_value: Some(short_key(key)),
            identity: None,
            note: None,
            action: Some("Press enter to learn more about altitude remotes."),
            purpose: "How a remote recognises this installation. It travels with \
                      every task you delegate, and a remote you have used remembers it.",
            used_by: None,
            fields: vec![
                ("name", label.clone()),
                ("identity", short_key(key)),
                ("state", tilde(data_dir)),
            ],
        },
        CredentialKind::Tracker {
            profile,
            directory,
            status,
        } => {
            let mut connector = tracker_connector(*status, Path::new(directory));
            connector.title = credential.label.clone();
            let _ = profile;
            connector
        }
        CredentialKind::Profile {
            profile,
            directory,
            status,
        } => profile_connector(
            credential.label.clone(),
            profile_purpose(profile),
            profile,
            directory,
            status,
        ),
    };
    if credential.profile().is_some() {
        connector.used_by = Some(credential.used_by.clone());
    }
    connector
}

pub(super) fn git_connector(identity: &GitIdentity) -> Connector {
    let is_set = identity.is_set();
    let status = match (is_set, identity.signs_commits) {
        (true, true) => "configured · signing enabled",
        (true, false) => "configured · signing disabled",
        (false, _) => "not set",
    };
    Connector {
        title: if !identity.label.is_empty() {
            identity.label.clone()
        } else {
            identity
                .name
                .clone()
                .unwrap_or_else(|| "Git identity".to_owned())
        },
        reach: if is_set { Reach::On } else { Reach::Off },
        status: status.to_owned(),
        status_value: None,
        identity: None,
        note: None,
        action: Some("Press enter to configure Git."),
        purpose: "The name, email, and signing key your tasks commit under, read from the \
                  version-control profile's git config.",
        used_by: Some(identity.used_by.clone()),
        fields: vec![
            (
                "name",
                identity.name.clone().unwrap_or_else(|| "—".to_owned()),
            ),
            (
                "email",
                identity.email.clone().unwrap_or_else(|| "—".to_owned()),
            ),
            (
                "signing",
                match (&identity.signing_key, identity.signs_commits) {
                    (Some(key), true) => key.clone(),
                    (Some(key), false) => format!("{key} (not used yet)"),
                    (None, _) => "—".to_owned(),
                },
            ),
        ],
    }
}

pub(super) fn tracker_connector(state: TrackerState, profile_dir: &Path) -> Connector {
    let (reach, note, action) = match state {
        TrackerState::Connected => (
            Reach::On,
            Some(
                "This account syncs to your remotes, and decides which machine receives \
                 an issue delegated in Linear."
                    .to_owned(),
            ),
            None,
        ),
        TrackerState::Connecting => (Reach::Asking, None, None),
        TrackerState::NotConnected => (Reach::Off, None, Some("Press enter to sign in.")),
    };
    let status = state.summary();
    Connector {
        title: "Linear".to_owned(),
        reach,
        status: status.to_owned(),
        status_value: None,
        identity: None,
        note,
        action,
        purpose: "A task tracking and product management tool. Altitude reads your issues and updates their status as it works on them.",
        used_by: None,
        fields: vec![("profile", tilde(&profile_dir.display().to_string()))],
    }
}

/// A remote, as the page reads it: what it is, whether it answered, and the
/// address and key the list keeps for it.
pub(super) fn remote_connector(
    title: String,
    address: &str,
    remembered: Option<&RemoteKey>,
    status: &RemoteStatus,
    needs_config: bool,
) -> Connector {
    let (reach, word, note, action) = match status {
        RemoteStatus::Reached if needs_config => (
            Reach::Asking,
            "synchronizing…",
            // What is actually sent: the projects that run on it, and the logins
            // those projects use. Nothing else of this machine's goes.
            Some("Sending the projects that run there, and the logins they use.".to_owned()),
            None,
        ),
        RemoteStatus::Reached => (Reach::On, "synced", None, None),
        RemoteStatus::Asking => (Reach::Asking, "connecting…", None, None),
        RemoteStatus::Syncing => (
            Reach::Asking,
            "synchronizing…",
            Some(
                "It is building what it needs to run a task. A first build installs the \
                 project's tools, so it takes a while and is only done once."
                    .to_owned(),
            ),
            None,
        ),
        RemoteStatus::Changed => (
            Reach::Off,
            "disconnected",
            Some("It is not the machine this client knew.".to_owned()),
            Some("Press enter to accept its identity."),
        ),
        RemoteStatus::Silent(reason) => (
            Reach::Off,
            "disconnected",
            Some(format!("It did not answer — {reason}.")),
            Some("Press enter to try again."),
        ),
        RemoteStatus::Unknown => (
            Reach::Off,
            "disconnected",
            None,
            Some("Press enter to reach it."),
        ),
    };
    // Its own name is the row's title, and its key is what this client
    // remembers: neither is an identity of ours, so the status line says nothing
    // "as".
    let mut fields = vec![("address", address.to_owned())];
    match remembered {
        Some(key) => {
            fields.push(("key", short_key(&key.key)));
            fields.push(("version", key.version.clone()));
            fields.push(("met", met_ago(key.seen_at_ms)));
        }
        None => fields.push(("identity", "never met".to_owned())),
    }
    Connector {
        title,
        reach,
        status: word.to_owned(),
        status_value: None,
        identity: None,
        note,
        action,
        purpose: "A machine that keeps running your tasks after you close Altitude, so work \
                  carries on while you are away. Yours alone, or shared with a team.",
        used_by: None,
        fields,
    }
}

/// The mark for what a remote's announcement said about one tracker: connected,
/// not connected, or unknown because the remote has not answered at all.
pub(in crate::tui) fn tracker_dot(is_ready: Option<bool>) -> Dot {
    match is_ready {
        Some(true) => Dot::still('●', palette::primary()),
        Some(false) => Dot::still('○', palette::muted()),
        None => Dot::still('○', palette::muted()),
    }
}

/// The remote's shared trackers, under its primary details: a status line in the
/// same shape as the first, not a page of its own.
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

/// What a profile is for, in one line, so a page opens by saying what the
/// operator is looking at.
fn profile_purpose(profile: &str) -> &'static str {
    match Service::from_id(profile) {
        Some(Service::VersionControl(_)) => {
            "The GitHub CLI login your tasks clone, push, and read pull requests with."
        }
        Some(Service::Agent(AgentKind::Pi)) => "Pi uses your provider account to run tasks.",
        Some(Service::Agent(AgentKind::Claude)) => "Claude Code uses your account to run tasks.",
        _ => "A login a task is given.",
    }
}

/// A public name a check printed, when it printed one in a shape the profile's
/// CLI is known to use. Anything else yields nothing rather than a guess:
/// signed in without a name is still true.
pub(super) fn public_identity(profile: &str, output: &str) -> Option<String> {
    match Service::from_id(profile)? {
        Service::VersionControl(_) => github_account(output),
        _ => None,
    }
}

fn github_account(output: &str) -> Option<String> {
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

/// A path as the operator would type it: the home directory's prefix is replaced
/// with `~`, component by component so `/home/gaby-other` is not mistaken for a
/// path under `/home/gaby`.
fn tilde(path: &str) -> String {
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

/// The head of a key, long enough to tell two apart and short enough to read on
/// the status line. The whole key stays in the fields below.
fn short_key(key: &str) -> String {
    const KEEP: usize = 12;
    let head = key.chars().take(KEEP).collect::<String>();
    if key.chars().count() > KEEP {
        format!("{head}…")
    } else {
        head
    }
}

fn met_ago(seen_at_ms: i64) -> String {
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

fn counted(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}
