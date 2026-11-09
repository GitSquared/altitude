use std::{
    fs,
    io::{self, ErrorKind, Read, Write},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    thread,
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc;

use crate::isolation::{
    Command, CommandEvent, CommandSession, IsolationError, OutputStream, shell_quote,
};

use super::{SandboxRecord, machine_path};

// The skeleton a sandbox's own disk holds. Everything else a command sees is bound
// over it: the machine's system trees, and the plan's mounts.
pub(super) fn prepare_root(root: &Path) -> Result<(), IsolationError> {
    for directory in [
        "usr", "etc", "dev", "sys", "proc", "tmp", "run", "root", "altitude", "var", ".testbed",
    ] {
        fs::create_dir_all(root.join(directory)).map_err(io_error)?;
    }
    // Ubuntu's merged-`/usr` layout: the top-level names are links into it, so a
    // program that resolves `/bin/sh` lands on the machine's.
    for (name, target) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/sbin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib64"),
    ] {
        let link = root.join(name);
        match symlink(target, &link) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    // The machine's own filesystem, in the guest's shape. `/tmp` and `/run` are
    // the sandbox's rather than the machine's, so a command does not see the host's
    // sockets and scratch, and their contents live as long as the sandbox does.
    for (name, mode) in [("tmp", 0o1777), ("run", 0o755)] {
        let path = root.join(name);
        fs::create_dir_all(&path).map_err(io_error)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).map_err(io_error)?;
    }
    Ok(())
}

pub(super) fn command_session(
    root: PathBuf,
    record: SandboxRecord,
    command: String,
) -> Result<CommandSession, IsolationError> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| IsolationError::new(error.to_string()))?;

    let mut builder = CommandBuilder::new("/usr/bin/unshare");
    builder.args(["--mount", "--propagation", "private", "/bin/bash", "-c"]);
    builder.arg(mount_script(&root, &record));
    // `bash -c script name argument`: the name is `$0`, the command `$1`.
    builder.arg("altitude-testbed");
    builder.arg(command);
    // `portable-pty` would otherwise start the child in `$HOME`, which is the
    // sandbox's `/root`; starting at the sandbox's own root keeps the spawn from
    // depending on whether the machine's account can reach that path. `chroot`
    // puts the command at `/` inside it either way.
    builder.cwd(&root);
    builder.env("PATH", machine_path());
    for (key, value) in &record.environment {
        builder.env(key, value);
    }

    let mut child = pair.slave.spawn_command(builder).map_err(|error| {
        IsolationError::new(format!("could not start a testbed command: {error:#}"))
    })?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| IsolationError::new(error.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| IsolationError::new(error.to_string()))?;
    let master = pair.master;

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<Command>();

    // Writing to a pty is blocking, so the commands live on a thread of their own
    // and the async side only hands them over.
    thread::spawn(move || {
        let mut writer = writer;
        while let Some(command) = command_rx.blocking_recv() {
            match command {
                Command::Input(bytes) => {
                    if writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                Command::Resize { rows, cols } => {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                Command::Interrupt => {
                    // The pty is the command's controlling terminal, so an interrupt
                    // is the byte a terminal would send.
                    let _ = writer.write_all(&[3]).and_then(|()| writer.flush());
                }
            }
        }
    });

    // Reading to the end is what makes the exit status the last event: a channel
    // that carried the exit before the output it ends would cut a run short.
    let exited = event_tx.clone();
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let output = CommandEvent::Output {
                        stream: OutputStream::Stdout,
                        bytes: buffer[..read].to_vec(),
                    };
                    if event_tx.send(output).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let event = match child.wait() {
            Ok(status) if status.success() => CommandEvent::Exited(0),
            Ok(status) => CommandEvent::Exited(status.exit_code() as i32),
            Err(error) => CommandEvent::Failed(format!(
                "the testbed command did not report an exit: {error}"
            )),
        };
        let _ = exited.send(event);
    });

    Ok(CommandSession {
        commands: command_tx,
        events: event_rx,
    })
}

// A fresh namespace per command, because the mounts a command needs are what make
// the guest paths mean the sandbox's files, and a namespace that outlived the
// command would have to be kept alive by something. The mounts cost nothing to
// repeat, and a sandbox that is swept leaves nothing mounted.
fn mount_script(root: &Path, record: &SandboxRecord) -> String {
    let mut script = String::from("set -eu\n");
    let quote = |path: &Path| shell_quote(&path.to_string_lossy());
    let target = |guest: &str| root.join(guest.trim_start_matches('/'));

    for directory in ["proc", "tmp", "root", "altitude", "run"] {
        script.push_str(&format!("mkdir -p {}\n", quote(&root.join(directory))));
    }
    script.push_str(&format!(
        "mount --rbind /usr {}\n",
        quote(&root.join("usr"))
    ));
    script.push_str(&format!(
        "mount --rbind /etc {}\n",
        quote(&root.join("etc"))
    ));
    script.push_str(&format!(
        "mount --rbind /dev {}\n",
        quote(&root.join("dev"))
    ));
    script.push_str(&format!(
        "mount --rbind /sys {}\n",
        quote(&root.join("sys"))
    ));
    script.push_str(&format!(
        "mount -t proc proc {} 2>/dev/null || mount --rbind /proc {}\n",
        quote(&root.join("proc")),
        quote(&root.join("proc"))
    ));
    script.push_str(&format!(
        "mount --rbind /root {}\n",
        quote(&root.join("root"))
    ));

    for mount in &record.mounts {
        let target = target(&mount.guest_path);
        if mount.host_path.is_dir() {
            script.push_str(&format!("mkdir -p {}\n", quote(&target)));
        } else {
            let parent = target.parent().unwrap_or(root);
            script.push_str(&format!("mkdir -p {}\n", quote(parent)));
            script.push_str(&format!("touch {}\n", quote(&target)));
        }
        script.push_str(&format!(
            "mount --bind {} {}\n",
            quote(&mount.host_path),
            quote(&target)
        ));
        if mount.readonly {
            script.push_str(&format!("mount -o remount,bind,ro {}\n", quote(&target)));
        }
    }

    script.push_str(&format!(
        "exec /usr/sbin/chroot {} /bin/sh -c \"$1\"\n",
        quote(root)
    ));
    script
}

fn io_error(error: io::Error) -> IsolationError {
    IsolationError::new(error.to_string())
}
