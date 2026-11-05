use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use microsandbox::Sandbox;

use crate::isolation::{FileTally, FileTransferSink, IsolationError};

use super::connect;
use super::files::{carry_in, carry_out, confirm_arrival, disagreement, read_archive, walk};

const TREE_ARCHIVE: &str = "/tmp/altitude-tree.tar";
const ARRIVAL_ARCHIVE: &str = "/tmp/altitude-arrival.tar";

// A tree leaves a sandbox as one file, and what left is held against what the
// sandbox says it has before a byte of it is believed.
//
// The sandbox's own tar is what concatenates the tree: fourteen thousand files cost
// fourteen thousand open-read-close round trips through the filesystem protocol and
// about a fifth of a second through `tar`. It is not trusted with the tree, only
// with the bytes — the walk through the filesystem protocol is what the archive is
// checked against, and a disagreement ends the transfer rather than travelling as a
// capsule short of work.
pub(super) async fn carry_tree_out(
    id: &str,
    root: &str,
    excluded: &[String],
    destination: &Path,
    progress: &dyn FileTransferSink,
) -> Result<FileTally, IsolationError> {
    let started = Instant::now();
    let sandbox = connect(id).await?;
    let fs = sandbox.fs();
    let tree = walk(&fs, root, excluded).await?;
    progress.moving(tree.whole);
    tracing::debug!(
        sandbox_id = %id,
        root,
        files = tree.whole.files,
        bytes = tree.whole.bytes,
        directories = tree.directories.len(),
        links = tree.links.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "walked a task's tree to carry it out"
    );
    pack(&sandbox, root, excluded).await?;
    // A tar that exits 0 and writes nothing is the cheapest thing to catch, and it is
    // worth saying so rather than failing later on a file that is not there.
    if fs.stat(TREE_ARCHIVE).await.is_err() {
        return Err(IsolationError::new(format!(
            "the sandbox's tar wrote no archive of {root}, so the task stays where it is"
        )));
    }
    let carried = carry_out(&fs, TREE_ARCHIVE, destination, 0o600).await;
    if let Err(error) = fs.remove(TREE_ARCHIVE).await {
        tracing::debug!(%error, archive = TREE_ARCHIVE, "could not remove the sandbox's archive");
    }
    carried.map_err(|error| {
        IsolationError::new(format!("carrying the archive of {root} out: {error}"))
    })?;
    let archive = read_archive(destination)?;
    if let Err(error) = disagreement(&archive, &tree) {
        tracing::error!(sandbox_id = %id, root, %error, "the sandbox did not pack its own tree");
        return Err(IsolationError::new(format!(
            "the sandbox packed a tree that is not the one it has ({error}), so the task stays where it is"
        )));
    }
    tracing::info!(
        sandbox_id = %id,
        root,
        files = tree.whole.files,
        bytes = tree.whole.bytes,
        archive = %destination.display(),
        elapsed_ms = started.elapsed().as_millis(),
        "carried a task's tree out in one archive the sandbox packed"
    );
    Ok(tree.whole)
}

// A tree arrives as one file, is unpacked by the sandbox's own tar, and the checkout
// is then read to prove the tree standing there is the tree the capsule carries.
//
// Nothing writes through a link: `tar -x` replaces a name that collides with what it
// is unpacking rather than following it, which is why there is no pass to refuse the
// links beforehand. A checkout may hold more than the capsule, because the clone on
// this side brought files of its own; it may not hold less.
pub(super) async fn put_tree_in(
    id: &str,
    source: PathBuf,
    root: &str,
    progress: &dyn FileTransferSink,
) -> Result<FileTally, IsolationError> {
    let started = Instant::now();
    let arrival = read_archive(&source)?;
    progress.moving(arrival.whole);
    tracing::debug!(
        sandbox_id = %id,
        root,
        source = %source.display(),
        files = arrival.whole.files,
        bytes = arrival.whole.bytes,
        directories = arrival.directories.len(),
        links = arrival.links.len(),
        "read the archive a task arrived with"
    );
    let sandbox = connect(id).await?;
    let fs = sandbox.fs();
    let _ = fs.mkdir(root).await;
    carry_in(&fs, &source, ARRIVAL_ARCHIVE, 0o600)
        .await
        .map_err(|error| IsolationError::new(format!("putting the archive in: {error}")))?;
    let unpacked = unpack(&sandbox, root).await;
    if let Err(error) = fs.remove(ARRIVAL_ARCHIVE).await {
        tracing::debug!(%error, "could not remove the archive from the sandbox");
    }
    unpacked?;
    if let Err(error) = confirm_arrival(&fs, root, &arrival).await {
        tracing::error!(sandbox_id = %id, root, %error, "the sandbox did not unpack the tree it was given");
        return Err(IsolationError::new(format!(
            "the sandbox unpacked a tree that is not the one it was given ({error}), so the task goes back where it was"
        )));
    }
    tracing::info!(
        sandbox_id = %id,
        root,
        source = %source.display(),
        files = arrival.whole.files,
        bytes = arrival.whole.bytes,
        elapsed_ms = started.elapsed().as_millis(),
        "put a task's arrived tree into its sandbox"
    );
    Ok(arrival.whole)
}

// The sandbox's tar, run by its own path with arguments: no shell, and nothing from
// a `PATH` the agent could have changed.
async fn pack(sandbox: &Sandbox, root: &str, excluded: &[String]) -> Result<(), IsolationError> {
    let root = root.trim_end_matches('/');
    let mut arguments = vec![
        "-cf".to_owned(),
        TREE_ARCHIVE.to_owned(),
        "-C".to_owned(),
        root.to_owned(),
        // Two names for one file are two files here, which is what the walk saw and
        // what the far side should get.
        "--hard-dereference".to_owned(),
    ];
    // An exclusion is a guest path; what tar names its entries by is relative to the
    // root it was given, so only the ones inside the tree are worth saying.
    for path in excluded {
        let inside = path
            .strip_prefix(&format!("{root}/"))
            .or_else(|| path.strip_prefix(root));
        if let Some(relative) = inside.filter(|relative| !relative.is_empty()) {
            arguments.push(format!("--exclude=./{}", relative.trim_start_matches('/')));
        }
    }
    arguments.push(".".to_owned());
    // A link at the archive's name would have tar write through it, so the name is
    // cleared first: replacing a link costs nothing.
    let _ = sandbox.fs().remove(TREE_ARCHIVE).await;
    let started = Instant::now();
    tar(sandbox, arguments).await.map_err(|error| {
        IsolationError::new(format!(
            "the sandbox could not pack {root}, which it needs a working tar to do: {error}"
        ))
    })?;
    tracing::debug!(
        sandbox_id = %sandbox.name(),
        root,
        elapsed_ms = started.elapsed().as_millis(),
        "the sandbox packed its tree"
    );
    Ok(())
}

async fn unpack(sandbox: &Sandbox, root: &str) -> Result<(), IsolationError> {
    let started = Instant::now();
    tar(
        sandbox,
        ["-xf", ARRIVAL_ARCHIVE, "-C", root.trim_end_matches('/')],
    )
    .await
    .map_err(|error| {
        IsolationError::new(format!(
            "the sandbox could not unpack the tree it was given, which it needs a working tar to do: {error}"
        ))
    })?;
    tracing::debug!(
        sandbox_id = %sandbox.name(),
        root,
        elapsed_ms = started.elapsed().as_millis(),
        "the sandbox unpacked the tree"
    );
    Ok(())
}

// The only thing a transfer asks of the guest's programs, answered with whatever
// tar said when it went wrong.
async fn tar(
    sandbox: &Sandbox,
    arguments: impl IntoIterator<Item = impl Into<String>>,
) -> Result<(), String> {
    let arguments = arguments
        .into_iter()
        .map(Into::into)
        .collect::<Vec<String>>();
    let output = sandbox
        .exec("/usr/bin/tar", arguments)
        .await
        .map_err(|error| error.to_string())?;
    if output.status().code != 0 {
        return Err(output.stderr().unwrap_or_default().trim().to_owned());
    }
    Ok(())
}
