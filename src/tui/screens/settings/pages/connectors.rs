use super::*;

use std::path::Path;

use crate::{
    remote::{Phase, RemoteKey},
    tui::{
        app::{CredentialStatus, RemoteStatus, Sync, TrackerState},
        widgets::progress,
    },
};

use crate::tui::screens::settings::rows::{Credential, CredentialKind, GitIdentity};

pub(crate) fn profile_connector(
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

pub(crate) fn credential_connector(credential: &Credential) -> Connector {
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

pub(crate) fn git_connector(identity: &GitIdentity) -> Connector {
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

pub(crate) fn tracker_connector(state: TrackerState, profile_dir: &Path) -> Connector {
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

/// What a machine says it is doing while it starts itself up, in the words a
/// client can act on: a download it can watch, a build it should not interrupt,
/// or the one thing it should do next.
pub(crate) fn starting_up(phase: Option<&Phase>) -> String {
    match phase {
        Some(Phase::BaseImage {
            downloaded_bytes,
            total_bytes,
        }) if *total_bytes > 0 => {
            let percent = (downloaded_bytes.saturating_mul(100) / total_bytes).min(100);
            format!(
                "Downloading the base image · {percent}% of {}. A first start does this once, \
                 and every task after it starts quicker.",
                progress::amount(*total_bytes)
            )
        }
        Some(Phase::BaseImage { .. }) => "Downloading the base image. A first start does this \
             once, and every task after it starts quicker."
            .to_owned(),
        Some(Phase::BaseTools) => {
            "Installing the base tools. A first start installs them once.".to_owned()
        }
        Some(Phase::Project { .. }) => {
            "Building a project's environment. A first build installs its tools, so it takes \
             a while and is only done once."
                .to_owned()
        }
        Some(Phase::Runtime) => {
            "Installing what runs sandboxes here. A first start installs it once.".to_owned()
        }
        // An older remote does not say which part it is on.
        Some(Phase::Ready) | None => {
            "It is building what it needs to run a task. A first build installs the project's \
             tools, so it takes a while and is only done once."
                .to_owned()
        }
    }
}

pub(crate) fn remote_connector(
    title: String,
    address: &str,
    remembered: Option<&RemoteKey>,
    status: &RemoteStatus,
    sync: Sync,
) -> Connector {
    let (reach, word, note, action) = match status {
        // Nothing of this machine's runs there, so nothing will ever be sent: a
        // word saying it is on its way is a wait that never ends.
        RemoteStatus::Reached if sync == Sync::NoProjects => (
            Reach::Off,
            "no projects yet",
            Some(
                "This machine runs none of your projects, so there is nothing to send it. \
                 Choose it as a project's host in Settings → Projects."
                    .to_owned(),
            ),
            None,
        ),
        // What it holds keeps until the task it is running there is gone.
        RemoteStatus::Reached if sync == Sync::UntilItsTaskEnds => (
            Reach::Off,
            "waiting for its task",
            Some(
                "One of your tasks is still running there, so what it needs to run stays \
                 until the task is gone."
                    .to_owned(),
            ),
            None,
        ),
        RemoteStatus::Reached if sync == Sync::Configuration => (
            Reach::Asking,
            "sending setup…",
            // What is actually sent: the projects that run on it, and the logins
            // those projects use. Nothing else of this machine's goes.
            Some("Sending the projects that run there, and the logins they use.".to_owned()),
            None,
        ),
        // One project's environment, which a first push builds from nothing. It is
        // this machine's own work being done over there, which is the wait the
        // words before it were too coarse to tell apart.
        RemoteStatus::Preparing(Some(Phase::Project { name })) => (
            Reach::Asking,
            "installing tools…",
            Some(format!(
                "Building {name}'s environment. A first build installs the project's tools, \
                 so it takes a while and is only done once."
            )),
            None,
        ),
        // It is starting itself up: installing what it needs and building a base image
        // before it can run anything. Nothing here is being sent anywhere, which is
        // the difference this word exists for.
        RemoteStatus::Preparing(phase) => (
            Reach::Asking,
            "starting up…",
            Some(starting_up(phase.as_ref())),
            None,
        ),
        RemoteStatus::Reached => (Reach::On, "synced", None, None),
        RemoteStatus::Asking => (Reach::Asking, "connecting…", None, None),
        RemoteStatus::Syncing => (
            Reach::Asking,
            "sending setup…",
            Some("Sending the projects that run there, and the logins they use.".to_owned()),
            None,
        ),
        RemoteStatus::Changed => (
            Reach::Off,
            "disconnected",
            Some("It is not the machine this client knew.".to_owned()),
            Some("Press enter to accept its identity."),
        ),
        RemoteStatus::Incompatible { local, remote } => (
            Reach::Off,
            "update required",
            Some(format!(
                "This machine runs Altitude v{local}; {title} runs v{remote}."
            )),
            Some("Update both machines to the same version."),
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
