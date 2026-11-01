use std::{
    fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::tracker::{ACCOUNT_FILE, TrackerAccount, TrackerKind};

use super::write_private;

pub struct Login {
    pub owner: String,
    pub account: TrackerAccount,
    pub arrived: SystemTime,
}

pub fn logins(operators_dir: &Path) -> Vec<Login> {
    let Ok(entries) = fs::read_dir(operators_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .flat_map(|entry| {
            let owner = entry.file_name().to_string_lossy().into_owned();
            TrackerKind::ALL
                .iter()
                .flat_map(move |provider| logins_of(&entry.path(), &owner, *provider))
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn logins_of(operator_dir: &Path, owner: &str, provider: TrackerKind) -> Vec<Login> {
    if yielded_providers(operator_dir).contains(&provider) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(operator_dir.join(provider.profile())) else {
        return Vec::new();
    };
    let mut logins: Vec<Login> = entries
        .flatten()
        .filter_map(|login| {
            let identity = login.path().join("profile").join(ACCOUNT_FILE);
            let arrived = fs::metadata(&identity).ok()?.modified().ok()?;
            let account = fs::read(&identity)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<TrackerAccount>(&bytes).ok())
                .filter(|account| account.provider == provider)?;
            Some(Login {
                owner: owner.to_owned(),
                account,
                arrived,
            })
        })
        .collect();
    logins.sort_by(|left, right| left.account.user_id.cmp(&right.account.user_id));
    logins
}

pub fn yield_account(operator_dir: &Path, provider: TrackerKind) -> io::Result<()> {
    remember_yielded(operator_dir, provider, true)
}

pub fn claim_account(operator_dir: &Path, provider: TrackerKind) -> io::Result<()> {
    remember_yielded(operator_dir, provider, false)
}

#[derive(Default, Serialize, Deserialize)]
struct Yielded {
    #[serde(default)]
    providers: Vec<TrackerKind>,
}

fn yielded_path(operator_dir: &Path) -> PathBuf {
    operator_dir.join("yielded.json")
}

fn yielded_providers(operator_dir: &Path) -> Vec<TrackerKind> {
    fs::read_to_string(yielded_path(operator_dir))
        .ok()
        .and_then(|text| serde_json::from_str::<Yielded>(&text).ok())
        .map(|yielded| yielded.providers)
        .unwrap_or_default()
}

fn remember_yielded(operator_dir: &Path, provider: TrackerKind, yielded: bool) -> io::Result<()> {
    let path = yielded_path(operator_dir);
    let mut held = yielded_providers(operator_dir);
    let had = held.contains(&provider);
    if yielded == had {
        return Ok(());
    }
    if yielded {
        held.push(provider);
    } else {
        held.retain(|held| *held != provider);
    }
    if held.is_empty() {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    let text =
        serde_json::to_string_pretty(&Yielded { providers: held }).map_err(io::Error::other)?;
    write_private(&path, &text).map_err(io::Error::other)
}
