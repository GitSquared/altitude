use std::{
    fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use crate::{
    config::Config,
    remote::{HISTORY_FILE, Handover, TRANSCRIPT_FILE, TransferArchive, TransferPart, WORK_FILE},
};

mod incoming;
mod outgoing;

pub use incoming::IncomingTransfers;
pub use outgoing::{HomeSender, resume_outgoing, send_to_remote};

// What a capsule is, in the one place both directions can agree about it: the names
// its files go by, the parts it is fetched in, and what its digests are worth.

pub(super) const TRANSFERS_DIRECTORY: &str = "transfers";
pub(super) const INCOMING_DIRECTORY: &str = "incoming";
pub(super) const DESTINATION_FILE: &str = "outgoing.json";
pub(super) const TRANSFER_FILE: &str = "transfer.json";
pub(super) const COMMITTED_FILE: &str = "committed";

fn message(error: impl fmt::Display) -> String {
    error.to_string()
}

pub(super) fn validated_transfer_id(id: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(id)
        .map(|parsed| parsed.to_string())
        .map_err(|_| "invalid transfer identifier".to_owned())
}

pub(super) fn transfer_directory(config: &Config, transfer_id: &str) -> Result<PathBuf, String> {
    let transfer_id = validated_transfer_id(transfer_id)?;
    Ok(config
        .data_dir()
        .join(TRANSFERS_DIRECTORY)
        .join(transfer_id))
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(message)?;
    let incoming = path.with_extension("json.incoming");
    let mut file = fs::File::create(&incoming).map_err(message)?;
    file.write_all(&bytes).map_err(message)?;
    file.sync_all().map_err(message)?;
    fs::rename(&incoming, path).map_err(message)
}

pub(super) fn part_details(
    handover: &Handover,
    part: TransferPart,
) -> Result<(&'static str, &'static str, &TransferArchive), String> {
    Ok(match part {
        TransferPart::History => ("history", HISTORY_FILE, &handover.history),
        TransferPart::Work => ("work", WORK_FILE, &handover.work),
        TransferPart::Transcript => ("transcript", TRANSCRIPT_FILE, &handover.transcript),
    })
}

pub(super) fn validate_archive(archive: &TransferArchive, name: &str) -> Result<(), String> {
    let digest = &archive.digest;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("the {name} archive has an unreadable digest"));
    }
    Ok(())
}

pub(super) fn verify_file(
    path: &Path,
    archive: &TransferArchive,
    name: &str,
) -> Result<(), String> {
    let length = fs::metadata(path).map_err(message)?.len();
    if length != archive.len {
        return Err(format!(
            "the {name} archive is {length} bytes, not {}",
            archive.len
        ));
    }
    if file_digest(path)? != archive.digest {
        return Err(format!("the {name} archive does not match its digest"));
    }
    Ok(())
}

pub(super) fn file_digest(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(message)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(message)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
