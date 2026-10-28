use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo::rerun-if-changed=assets/macos/Info.plist");
    println!("cargo::rerun-if-env-changed=ALTITUDE_GIT_SHA");
    watch_commits();

    // The commit is part of the name the binary and the interface show, so a new
    // commit has to reach the compiler even when no source file changed.
    let package = env::var("CARGO_PKG_VERSION").expect("Cargo sets the package version");
    let version = match commit() {
        Some(commit) => format!("v{package}-{commit}"),
        None => format!("v{package}"),
    };
    println!("cargo::rustc-env=ALTITUDE_VERSION={version}");

    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|target| target == "macos") {
        let manifest =
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
        let plist = manifest.join("assets/macos/Info.plist");
        println!(
            "cargo::rustc-link-arg-bin=altitude=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}

/// The first six characters of the commit this build came from.
///
/// CI passes it in `ALTITUDE_GIT_SHA`, because a checkout can be shallow and a
/// container context may not carry `.git` at all.
fn commit() -> Option<String> {
    env::var("ALTITUDE_GIT_SHA")
        .ok()
        .map(|commit| commit.trim().to_owned())
        .filter(|commit| !commit.is_empty())
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .filter(|commit| commit.len() >= 6)
        .map(|commit| commit[..6].to_owned())
}

/// Ask Cargo to run this script again once HEAD moves.
///
/// HEAD carries the commit when it is detached, and a commit on a branch updates
/// HEAD's reflog. Watching the reflog rather than the whole git directory keeps
/// `git status` from rebuilding the binary; both paths are asked of git because
/// a worktree keeps them outside its own directory.
fn watch_commits() {
    for name in ["HEAD", "logs/HEAD"] {
        let Some(path) = git(&["rev-parse", "--git-path", name]) else {
            continue;
        };
        if Path::new(&path).exists() {
            println!("cargo::rerun-if-changed={path}");
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env::var_os("CARGO_MANIFEST_DIR")?)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
