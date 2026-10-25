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
    isolation::{EnvironmentDescription, REFRESH_AFTER},
    remote::{RemoteKey, TrackerConnection, TrackerReadiness},
    tracker::TrackerKind,
    tui::{
        app::{CredentialStatus, RemoteStatus, TrackerState},
        widgets::{
            palette,
            utils::{Dot, plain},
        },
    },
};

use super::rows::{Credential, CredentialKind, GitIdentity, mark};

/// A small heading inside a page, so a page that has to say several things can
/// be read in the order it means them.
pub(super) fn section_heading(text: &str) -> Line<'static> {
    Line::styled(text.to_owned(), palette::primary().bold())
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
    /// One value the status accents, e.g. the short key of this installation.
    status_value: Option<String>,
    /// A public name the provider reports, when the check printed one. Never
    /// guessed: absent when the report does not name one.
    identity: Option<String>,
    /// A short reason the status reads the way it does.
    note: Option<String>,
    /// The one next step, when there is one.
    action: Option<&'static str>,
    purpose: &'static str,
    fields: Vec<(&'static str, String)>,
}

/// Whether a connector is on, off, or still being asked about.
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
        spans.push(Span::styled(
            self.status.clone(),
            match self.reach {
                Reach::On => palette::primary().bold(),
                Reach::Off | Reach::Asking => palette::muted().bold(),
            },
        ));
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
        lines.push(step_line(&format!("{}.", index + 1), step.prose));
        if !step.code.is_empty() {
            lines.push(Line::from(""));
            lines.extend(step.code.iter().map(|line| sample(line)));
        }
    }
    for paragraph in guide.closing {
        lines.push(Line::from(""));
        lines.push(Line::styled(*paragraph, palette::muted()));
    }
    lines
}

fn step_line(number: &str, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{number} "), palette::accent()),
        Span::raw(text.to_owned()),
    ])
}

fn sample(text: &str) -> Line<'static> {
    Line::styled(text.to_owned(), palette::accent())
}
/// The built-in environment, described rather than configured.
///
/// The version and the packages are read back from the built snapshot, because
/// that is the only place that knows them: the image is a rolling tag and the
/// setup commands name no versions. A machine that has not built it yet says so
/// rather than guessing.
pub(super) fn base_lines(
    description: Option<&EnvironmentDescription>,
    heading: impl Fn(String) -> Line<'static>,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading("Base environment".to_owned()),
        // The subtitle rather than a postscript: it is true of the whole page.
        Line::styled("built in · cannot be edited or deleted", palette::muted()),
        Line::from(""),
        Line::from(
            "Altitude runs your tasks in sandboxes that move between machines as you delegate \
             work.",
        ),
        Line::from(""),
    ];
    match description.filter(|description| !description.alpine_release.is_empty()) {
        Some(description) => {
            // What was read back from the build is accented, so the page says at
            // a glance which parts of it are about this machine rather than about
            // Altitude.
            lines.push(Line::from(vec![
                Span::raw("New tasks are built from Alpine Linux "),
                Span::styled(description.alpine_release.clone(), palette::accent()),
                Span::raw(", with these packages installed:"),
            ]));
            lines.push(Line::from(""));
            // One package a line: a list of versions is read, not scanned.
            if description.packages.is_empty() {
                lines.push(Line::styled(
                    "nothing beyond the base image",
                    palette::muted(),
                ));
            } else {
                lines.extend(
                    description
                        .packages
                        .iter()
                        .map(|package| Line::styled(package.clone(), palette::accent())),
                );
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("Built "),
                Span::styled(met_ago(description.built_at_ms), palette::accent()),
                Span::raw(", and refreshed automatically every "),
                Span::styled(
                    counted(REFRESH_AFTER.as_secs() as i64 / 3_600, "hour"),
                    palette::accent(),
                ),
                Span::raw(", so security patches arrive without you doing anything."),
            ]));
        }
        None => {
            lines.push(Line::from(
                "It has not been built on this machine yet. The first task that uses it builds \
                 it, and this page then says what it was built from.",
            ));
        }
    }
    lines
}

/// A login a task signs in with, as its page reads it. The same shape serves a
/// coding agent and the GitHub CLI.
pub(super) fn profile_connector(
    title: String,
    purpose: &'static str,
    profile: &'static str,
    directory: &str,
    status: &CredentialStatus,
) -> Connector {
    let (reach, word) = match status {
        CredentialStatus::Checking => (Reach::Asking, "checking…"),
        CredentialStatus::Checked {
            signed_in: true, ..
        } => (Reach::On, "signed in"),
        CredentialStatus::Checked {
            signed_in: false, ..
        } => (Reach::Off, "not signed in"),
        CredentialStatus::Saved => (Reach::On, "saved login"),
        // Not asked, or nothing to ask: neither is a sign-out.
        CredentialStatus::Unknown | CredentialStatus::Unanswered => (Reach::Off, "not checked"),
    };
    let note = matches!(status, CredentialStatus::Saved).then(|| {
        "A login is saved on this machine. No environment has been built here to ask \
         the tool whether it still works."
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
        fields: vec![("saved at", tilde(directory))],
    }
}

/// This installation's key, or a profile's login, as the page reads it.
pub(super) fn credential_connector(credential: &Credential) -> Connector {
    match &credential.kind {
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
            fields: vec![
                ("name", label.clone()),
                ("identity", short_key(key)),
                ("state", tilde(data_dir)),
            ],
        },
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
    }
}

/// The git identity and signing a task commits with.
pub(super) fn git_connector(identity: &GitIdentity) -> Connector {
    let is_set = identity.is_set();
    let status = match (is_set, identity.signs_commits) {
        (true, true) => "configured · signing enabled",
        (true, false) => "configured · signing disabled",
        (false, _) => "not set",
    };
    Connector {
        title: "Git identity".to_owned(),
        reach: if is_set { Reach::On } else { Reach::Off },
        status: status.to_owned(),
        status_value: None,
        identity: None,
        note: None,
        action: Some("Press enter to configure Git."),
        purpose: "The name, email, and signing key your tasks commit under, read from the \
                  version-control profile's git config.",
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

/// Linear, as the page reads it: a live session rather than a login kept on disk.
pub(super) fn tracker_connector(state: TrackerState, profile_dir: &Path) -> Connector {
    let (reach, status, note, action) = match state {
        TrackerState::Connected => (
            Reach::On,
            "signed in",
            Some(
                "This account syncs to your remotes, and decides which machine receives \
                 an issue delegated in Linear."
                    .to_owned(),
            ),
            None,
        ),
        TrackerState::Connecting => (Reach::Asking, "connecting…", None, None),
        TrackerState::NotConnected => (
            Reach::Off,
            "not signed in",
            None,
            Some("Press enter to sign in."),
        ),
    };
    Connector {
        title: "Linear".to_owned(),
        reach,
        status: status.to_owned(),
        status_value: None,
        identity: None,
        note,
        action,
        purpose: "A task tracking and product management tool. Altitude reads your issues and updates their status as it works on them.",
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
            Some("Sending this machine's environments and logins.".to_owned()),
            None,
        ),
        RemoteStatus::Reached => (Reach::On, "synced", None, None),
        RemoteStatus::Asking => (Reach::Asking, "connecting…", None, None),
        RemoteStatus::Syncing => (
            Reach::Asking,
            "synchronizing…",
            Some("It is building what it needs to run a task.".to_owned()),
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
    let mut fields = vec![("address", address.to_owned())];
    match remembered {
        Some(key) => {
            fields.push(("identity", short_key(&key.key)));
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
        identity: remembered.map(|key| key.label.clone()),
        note,
        action,
        purpose: "A machine that keeps running your tasks after you close Altitude, so work \
                  carries on while you are away. Yours alone, or shared with a team.",
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
            TrackerReadiness::NotInstalled => {
                format!(
                    "{} · not installed on the remote",
                    connection.provider.label()
                )
            }
            TrackerReadiness::Unavailable => format!(
                "{} · cannot sign in on the remote",
                connection.provider.label()
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
    match profile {
        "vcs/github" => "The GitHub CLI login your tasks clone, push, and read pull requests with.",
        "agents/pi" => "Pi uses your provider account to run tasks.",
        "agents/claude" => "Claude Code uses your account to run tasks.",
        _ => "A login a task is given.",
    }
}

/// A public name a check printed, when it printed one in a shape the profile's
/// CLI is known to use. Anything else yields nothing rather than a guess:
/// signed in without a name is still true.
fn public_identity(profile: &str, output: &str) -> Option<String> {
    match profile {
        "vcs/github" => github_account(output),
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

/// How long ago something was seen, coarsely: the exact minute is not the point.
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

/// A count and its unit, saying "1 hour" rather than "1 hours".
fn counted(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

/// Which profile a coding agent's login lives in.
pub(super) fn agent_profile(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Pi => "agents/pi",
        AgentKind::Claude => "agents/claude",
    }
}

pub(super) fn agent_purpose(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Pi => "A coding harness by Earendil. Altitude drives it to run your tasks.",
        AgentKind::Claude => "A coding harness by Anthropic. Altitude drives it to run your tasks.",
    }
}
