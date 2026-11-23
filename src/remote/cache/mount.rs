use std::{
    path::Path,
    process::{Command, Stdio},
};

// A kernel NFS mount needs root, and a worker host is an ordinary account, so whether
// this host can mount at all is a fact about the host. It is read once, as the worker
// starts, and told to the master, which then never asks a host for a mount it cannot make.
#[derive(Clone, Copy, Debug)]
pub(crate) enum MountPrivilege {
    Root,
    Sudo,
}

impl MountPrivilege {
    pub(crate) fn command(self, program: &str) -> Command {
        match self {
            Self::Root => Command::new(program),
            Self::Sudo => {
                let mut command = Command::new("sudo");
                command.arg("-n").arg(program);
                command
            }
        }
    }
}

// Reads without prompting and without mounting anything. `sudo -n -l` asks sudoers what
// this account may run and fails instead of asking for a password; a host with no sudo
// at all fails to spawn it, and that is an answer too.
pub(crate) fn probe() -> Result<MountPrivilege, String> {
    let has_nfs_helper = ["/sbin", "/usr/sbin"]
        .iter()
        .any(|directory| Path::new(directory).join("mount.nfs4").exists());
    if !has_nfs_helper {
        return Err("it has no mount.nfs4 (nfs-common)".to_owned());
    }
    if is_root() {
        return Ok(MountPrivilege::Root);
    }
    let sudo_may_mount = Command::new("sudo")
        .args(["-n", "-l", "mount"])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success());
    if sudo_may_mount {
        Ok(MountPrivilege::Sudo)
    } else {
        Err("it is not root and sudo does not let it mount without a password".to_owned())
    }
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
}
