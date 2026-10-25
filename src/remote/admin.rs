//! What a remote knows about the clients that have used it, from its own terminal.
//!
//! A remote has no account system: reaching it is the environment's job, so
//! admitting someone is not its decision to make. That leaves the housekeeping to
//! whoever runs the machine — who has been here, what they left behind, and how to
//! be rid of it — which is what this is for.
//!
//! Nothing here reads a credential. What it reports is the shape of what a client
//! pushed: which services it is signed in to, and which tracker account its work
//! would be attributed to.

use std::{fs, io, path::Path};

use super::LinkResult;
use crate::{
    config::Config,
    db::Database,
    remote::{
        ClientRecord,
        tracker::{AccountRecord, accounts_path},
    },
    task::TaskStore,
};

/// One client, as the remote can describe it without reading what it sent.
pub struct KnownClient {
    pub key: String,
    /// The profiles it pushed, by name: what it is signed in to here.
    pub services: Vec<String>,
    /// The tracker accounts it pushed, which are what routes its work in a tracker
    /// to this machine.
    pub accounts: Vec<AccountRecord>,
}

/// Print what this remote knows about its clients.
pub async fn list(config: &Config) -> LinkResult<()> {
    let database = Database::open(config).await?;
    let store = TaskStore::new(&database);
    let tasks = store.list().await?;
    let known = archive(config.data_dir())?;
    if known.is_empty() {
        println!("No client has used this remote.");
        return Ok(());
    }
    for client in known {
        let owned = tasks
            .iter()
            .filter(|task| task.facts.operator_id.as_deref() == Some(client.key.as_str()))
            .count();
        println!("{}", client.key);
        println!(
            "  services: {}",
            if client.services.is_empty() {
                "none".to_owned()
            } else {
                client.services.join(", ")
            }
        );
        if client.accounts.is_empty() {
            println!("  tracker:  none");
        }
        for record in &client.accounts {
            println!(
                "  tracker:  {} as {} ({})",
                record.account.provider.label(),
                record.account.display_name,
                record.account.user_id
            );
        }
        println!("  tasks:    {owned}");
    }
    Ok(())
}

/// Forget everything this remote knows about one client.
///
/// The directory it was given, the entry in the client record, and the tasks it
/// owns here. What the container runtime has on disk for those tasks is the
/// runtime's, and is left for it to reclaim.
pub async fn evict(config: &Config, key: &str) -> LinkResult<()> {
    let parsed = key
        .parse::<iroh::EndpointId>()
        .map_err(|error| format!("{key} is not a client key: {error}"))?;
    let directory = config.data_dir().join("operators").join(parsed.to_string());
    if !directory.is_dir() {
        return Err(format!("{parsed} has never used this remote").into());
    }
    println!("Stop the remote before evicting: a running one keeps its own state.");
    let database = Database::open(config).await?;
    let store = TaskStore::new(&database);
    let owned: Vec<String> = store
        .list()
        .await?
        .into_iter()
        .filter(|task| task.facts.operator_id.as_deref() == Some(parsed.to_string().as_str()))
        .map(|task| task.id)
        .collect();
    for id in &owned {
        store.forget(id).await?;
    }
    fs::remove_dir_all(&directory)?;
    // The record is who has been here, not what they left: failing to read it is
    // not a reason to leave the rest behind.
    let forgotten = ClientRecord::open(config.data_dir().join("clients.json"))
        .and_then(|record| record.forget(parsed))
        .map_err(|error| tracing::warn!(%error, "could not update the client record"))
        .is_ok();
    println!("Evicted {parsed}");
    println!("  configuration and logins: removed");
    println!("  tasks:                    {}", owned.len());
    println!(
        "  client record:            {}",
        if forgotten {
            "removed"
        } else {
            "was not listed"
        }
    );
    println!("Stop the remote before evicting: a running one keeps its own state.");
    Ok(())
}

/// The clients with a directory here, which is every client that pushed anything.
fn archive(data_dir: &Path) -> io::Result<Vec<KnownClient>> {
    let root = data_dir.join("operators");
    let mut clients = Vec::new();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(clients),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let directory = entry.path();
        let key = entry.file_name().to_string_lossy().into_owned();
        clients.push(KnownClient {
            services: services(&directory)?,
            accounts: accounts(&directory),
            key,
        });
    }
    clients.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(clients)
}

/// The profiles a client pushed, named by the directory each one lives in.
///
/// A profile is a nested path — `agents/claude`, `vcs/github` — with its files
/// under `profile`, so what counts is any directory two levels down that holds
/// one. Anything else here is bookkeeping the remote keeps beside it.
fn services(operator_dir: &Path) -> io::Result<Vec<String>> {
    let mut services = Vec::new();
    for family in fs::read_dir(operator_dir)? {
        let family = family?;
        if !family.file_type()?.is_dir() {
            continue;
        }
        for profile in fs::read_dir(family.path())? {
            let profile = profile?;
            if !profile.file_type()?.is_dir() || !profile.path().join("profile").is_dir() {
                continue;
            }
            services.push(format!(
                "{}/{}",
                family.file_name().to_string_lossy(),
                profile.file_name().to_string_lossy()
            ));
        }
    }
    services.sort();
    Ok(services)
}

fn accounts(operator_dir: &Path) -> Vec<AccountRecord> {
    fs::read_to_string(accounts_path(operator_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}
