//! What `altitude serve connect linear` does: say what this machine holds, read
//! the manual, install the app, say what landed.
//!
//! Every fact printed here belongs to this machine. The manual it reads is shared
//! with the client, where none of those facts are present, so nothing this host
//! has is ever rendered on a host that is not this one.

use std::{error::Error, io, path::Path, time::SystemTime};

use crate::{
    config::{LinearSettings, ServeSettings},
    tracker::{TrackerKind, oauth, setup::Guide, setup::Step},
};

use super::console::{ask, field, has_terminal, paragraph, prose};

pub async fn connect_linear(
    settings: &ServeSettings,
    settings_path: &Path,
    credentials_path: &Path,
) -> Result<(), Box<dyn Error>> {
    let guide = TrackerKind::Linear.guide();
    let Some(linear) = settings.linear.clone() else {
        println!("\n{}\n", guide.title);
        print_guide(guide);
        return Err(io::Error::other(format!(
            "{} has no [linear] section; the sample above is what it needs",
            settings_path.display()
        ))
        .into());
    };

    print_report(&linear, settings_path)?;
    match oauth::installed(credentials_path) {
        Some(installed) => replace(&linear, credentials_path, &installed).await,
        None => {
            print_guide(guide);
            if !ask(
                "Press enter to open Linear's install page · it waits up to 15 minutes",
                true,
            )? {
                println!("\nNothing was installed.");
                return Ok(());
            }
            let credentials = oauth::connect(linear, credentials_path)
                .await
                .map_err(io::Error::other)?;
            print_installed(&credentials, credentials_path);
            Ok(())
        }
    }
}

/// What this machine is configured with, and what it is missing.
///
/// Printed before the manual, because the operator needs to know what has been
/// read before being told what to write.
fn print_report(linear: &LinearSettings, settings_path: &Path) -> Result<(), Box<dyn Error>> {
    let callback = linear
        .endpoint("/linear/oauth/callback")
        .map_err(|error| io::Error::other(format!("serve.toml: {error}")))?;
    let webhook = linear
        .endpoint("/linear/webhook")
        .map_err(|error| io::Error::other(format!("serve.toml: {error}")))?;

    println!(
        "\n{} · this machine's own app\n",
        TrackerKind::Linear.label()
    );
    field("settings", settings_path.display());
    field("address", &linear.public_url);
    field("webhooks", &linear.listen);
    field("callback", callback);
    field("webhook", webhook);

    // Presence only: the values are never printed, not even redacted ones, so
    // this output is safe to paste into a bug report.
    let client = linear.client_secret();
    let webhook_secret = linear.webhook_secret();
    field(
        "secrets",
        format!(
            "client_secret {} · webhook_secret {}",
            present(&client),
            present(&webhook_secret)
        ),
    );

    let mut missing = Vec::new();
    if client.is_err() {
        missing.push(("client_secret", linear.client_secret_env.as_str()));
    }
    if webhook_secret.is_err() {
        missing.push(("webhook_secret", linear.webhook_secret_env.as_str()));
    }
    if missing.is_empty() {
        println!();
        return Ok(());
    }
    let listed = missing
        .iter()
        .map(|(field, variable)| {
            format!(
                "  {field:<16} add it to [linear] in {}, or set {variable}",
                settings_path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Err(io::Error::other(format!("Missing from this machine:\n{listed}")).into())
}

/// A login is already here. Say what it is, and replace it only when asked.
async fn replace(
    linear: &LinearSettings,
    credentials_path: &Path,
    installed: &oauth::Installed,
) -> Result<(), Box<dyn Error>> {
    println!("Already installed on this machine:");
    field("workspace", workspace(&installed.credentials));
    field("on", when(installed.installed_at));
    if !has_terminal() {
        println!("\nRun this from a terminal to replace the login.");
        return Ok(());
    }
    if !ask("Replace it with a fresh install? [y/N]", false)? {
        return Ok(());
    }
    let credentials = oauth::connect(linear.clone(), credentials_path)
        .await
        .map_err(io::Error::other)?;
    print_installed(&credentials, credentials_path);
    Ok(())
}

fn print_installed(credentials: &oauth::AppCredentials, credentials_path: &Path) {
    println!("\nInstalled:");
    field("workspace", workspace(credentials));
    field("saved", credentials_path.display());
    println!();
    paragraph(
        "The token refreshes itself while `altitude serve` runs. Start it to receive delegated \
         work, and keep it up with `altitude serve install`.",
    );
}

/// A workspace by name when Linear gave one, and by id when it did not.
fn workspace(credentials: &oauth::AppCredentials) -> String {
    if credentials.workspace_name.is_empty() {
        credentials.workspace_id.clone()
    } else {
        credentials.workspace_name.clone()
    }
}

fn when(installed_at: Option<SystemTime>) -> String {
    installed_at
        .map(|time| {
            chrono::DateTime::<chrono::Local>::from(time)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn present(secret: &Result<String, String>) -> &'static str {
    if secret.is_ok() { "set" } else { "missing" }
}

fn print_guide(guide: &Guide) {
    for text in guide.intro {
        paragraph(text);
        println!();
    }
    for (index, step) in guide.steps.iter().enumerate() {
        print_step(index + 1, step);
    }
    for text in guide.closing {
        paragraph(text);
        println!();
    }
}

fn print_step(number: usize, step: &Step) {
    let lead = format!("  {number}.  ");
    let indent = " ".repeat(lead.len());
    prose(step.prose, &lead, &indent);
    for line in step.code {
        println!("{indent}  {line}");
    }
    println!();
}
