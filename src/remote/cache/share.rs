use std::{
    path::Path,
    process::Command,
    sync::{Mutex, OnceLock},
};

// Sharing a cache is an export, and an export is the machine's business, not this
// process's: both sides ask for the one command they need and nothing else.

// Exports are one file on the machine, so they are written one at a time.
fn serialise() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// Hand one project's cache to one host, for as long as that host has a sandbox here.
// Everything the sandbox writes arrives as this machine's own account, so the directory
// keeps one owner however many hosts come and go.
pub(crate) fn export(client: &str, path: &Path, owner_uid: u32) -> Result<(), String> {
    let _serial = serialise().lock().expect("the export lock");
    let options =
        format!("rw,sync,no_subtree_check,all_squash,anonuid={owner_uid},anongid={owner_uid}");
    let spec = format!("{client}:{}", path.display());
    let output = Command::new("sudo")
        .arg("-n")
        .arg("/usr/sbin/exportfs")
        .arg("-o")
        .arg(&options)
        .arg(&spec)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
}

pub(crate) fn unexport(client: &str, path: &Path) -> Result<(), String> {
    let _serial = serialise().lock().expect("the export lock");
    let spec = format!("{client}:{}", path.display());
    let output = Command::new("sudo")
        .arg("-n")
        .arg("/usr/sbin/exportfs")
        .arg("-u")
        .arg(&spec)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    // A host that left before its sandbox ended leaves an export already gone, and that
    // is the state this call wanted.
    Ok(())
}

// The machine an export is granted to, from the address it joined with.
pub(crate) fn client_ip(peer: &iroh::EndpointAddr) -> Option<String> {
    peer.addrs.iter().find_map(|addr| match addr {
        iroh::TransportAddr::Ip(socket) => Some(socket.ip().to_string()),
        _ => None,
    })
}
