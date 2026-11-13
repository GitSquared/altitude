use std::{
    fmt,
    fs::{self, File, FileTimes, OpenOptions},
    io,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};

use crate::config::Config;
use crate::remote::protocol::{MAX_BLOB_BYTES, read_body, read_frame, write_frame};

use super::protocol::{CacheAttr, CacheKind, CacheOp, CacheReply, CacheRequest};

// The master's half of a project's cache: a worker's FUSE mount makes these calls,
// and every path is confined to the operator's cache directory for that project.
pub(crate) struct CacheService {
    config: Config,
    token: String,
}

impl CacheService {
    pub(crate) fn new(config: &Config, token: String) -> Self {
        Self {
            config: config.clone(),
            token,
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    fn root(&self, owner: &str, project: &str) -> Result<PathBuf, io::Error> {
        let scoped = self.config.for_operator(owner).map_err(io::Error::other)?;
        let root = scoped.project_cache_dir(project);
        // The root itself is created here, so every later confinement can canonicalize.
        fs::create_dir_all(&root).map_err(io::Error::other)?;
        root.canonicalize().map_err(io::Error::other)
    }

    // Where a request's path lands, confined to `root`. `follow` canonically
    // resolves the leaf, so a symlink inside the cache cannot read or write outside
    // it; without it the parent is resolved and the leaf is only joined.
    fn resolve(root: &Path, path: &str, follow: bool) -> Result<PathBuf, io::Error> {
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return Err(io::Error::other(format!(
                "{path:?} is not a path inside the cache"
            )));
        }
        let joined = root.join(relative);
        let candidate = if follow {
            joined.canonicalize().map_err(io::Error::other)?
        } else {
            let parent = joined
                .parent()
                .ok_or_else(|| io::Error::other(format!("{path:?} has no parent")))?
                .canonicalize()
                .map_err(io::Error::other)?;
            parent.join(
                joined
                    .file_name()
                    .ok_or_else(|| io::Error::other(format!("{path:?} names nothing")))?,
            )
        };
        if !candidate.starts_with(root) {
            return Err(io::Error::other(format!("{path:?} is outside the cache")));
        }
        Ok(candidate)
    }

    pub(crate) fn answer(&self, request: &CacheRequest, body: &[u8]) -> (CacheReply, Vec<u8>) {
        match self.perform(request, body) {
            Ok((reply, bytes)) => (reply, bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (CacheReply::Missing, Vec::new())
            }
            Err(error) => (CacheReply::Failed(error.to_string()), Vec::new()),
        }
    }

    fn perform(
        &self,
        request: &CacheRequest,
        body: &[u8],
    ) -> Result<(CacheReply, Vec<u8>), io::Error> {
        let root = self.root(&request.owner, &request.project)?;
        match &request.op {
            CacheOp::Getattr { path } => {
                let target = Self::resolve(&root, path, true)?;
                Ok((attrs_or_missing(&target, true)?, Vec::new()))
            }
            CacheOp::Setattr {
                path,
                size,
                mode,
                mtime_ms,
            } => {
                let target = Self::resolve(&root, path, true)?;
                if let Some(size) = size {
                    OpenOptions::new()
                        .write(true)
                        .open(&target)
                        .and_then(|file| file.set_len(*size))
                        .map_err(io::Error::other)?;
                }
                if let Some(mode) = mode {
                    fs::set_permissions(&target, fs::Permissions::from_mode(*mode))
                        .map_err(io::Error::other)?;
                }
                if let Some(mtime_ms) = mtime_ms {
                    set_mtime(&target, *mtime_ms)?;
                }
                Ok((attrs_or_missing(&target, true)?, Vec::new()))
            }
            CacheOp::Lookup { path } => {
                let target = Self::resolve(&root, path, true)?;
                Ok((attrs_or_missing(&target, false)?, Vec::new()))
            }
            CacheOp::ReadDir { path } => {
                let target = Self::resolve(&root, path, true)?;
                let directory = match fs::read_dir(&target) {
                    Ok(directory) => directory,
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        return Ok((CacheReply::Missing, Vec::new()));
                    }
                    Err(error) => return Err(io::Error::other(error)),
                };
                let mut entries = Vec::new();
                for entry in directory {
                    let entry = entry.map_err(io::Error::other)?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if let Ok(Some(attr)) = attr_of(&entry.path(), false) {
                        entries.push((name, attr));
                    }
                }
                entries.sort_by(|left, right| left.0.cmp(&right.0));
                Ok((CacheReply::Entries(entries), Vec::new()))
            }
            CacheOp::Read { path, offset, size } => {
                let target = Self::resolve(&root, path, true)?;
                let mut file = File::open(&target).map_err(io::Error::other)?;
                file.seek(SeekFrom::Start(*offset))
                    .map_err(io::Error::other)?;
                let mut bytes = vec![0u8; *size as usize];
                let read = file.read(&mut bytes).map_err(io::Error::other)?;
                bytes.truncate(read);
                let len = bytes.len() as u32;
                Ok((CacheReply::Data { len }, bytes))
            }
            CacheOp::Write { path, offset, .. } => {
                let target = Self::resolve(&root, path, true)?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .open(&target)
                    .map_err(io::Error::other)?;
                file.seek(SeekFrom::Start(*offset))
                    .map_err(io::Error::other)?;
                file.write_all(body).map_err(io::Error::other)?;
                Ok((
                    CacheReply::Written {
                        len: body.len() as u32,
                    },
                    Vec::new(),
                ))
            }
            CacheOp::Create { path, mode } => {
                let target = Self::resolve(&root, path, false)?;
                create_file(&target, *mode)?;
                Ok((attrs_or_missing(&target, false)?, Vec::new()))
            }
            CacheOp::Mkdir { path, mode } => {
                let target = Self::resolve(&root, path, false)?;
                fs::create_dir(&target).map_err(io::Error::other)?;
                fs::set_permissions(&target, fs::Permissions::from_mode(*mode))
                    .map_err(io::Error::other)?;
                Ok((CacheReply::Ok, Vec::new()))
            }
            CacheOp::Unlink { path } => {
                let target = Self::resolve(&root, path, false)?;
                match fs::remove_file(&target) {
                    Ok(()) => Ok((CacheReply::Ok, Vec::new())),
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        Ok((CacheReply::Missing, Vec::new()))
                    }
                    Err(error) => Err(io::Error::other(error)),
                }
            }
            CacheOp::Rmdir { path } => {
                let target = Self::resolve(&root, path, false)?;
                match fs::remove_dir(&target) {
                    Ok(()) => Ok((CacheReply::Ok, Vec::new())),
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        Ok((CacheReply::Missing, Vec::new()))
                    }
                    Err(error) => Err(io::Error::other(error)),
                }
            }
            CacheOp::Rename { from, to } => {
                let source = Self::resolve(&root, from, false)?;
                let target = Self::resolve(&root, to, false)?;
                fs::rename(&source, &target).map_err(io::Error::other)?;
                Ok((CacheReply::Ok, Vec::new()))
            }
            CacheOp::Symlink { target, path } => {
                let link = Self::resolve(&root, path, false)?;
                symlink(target, &link).map_err(io::Error::other)?;
                Ok((CacheReply::Ok, Vec::new()))
            }
            CacheOp::ReadLink { path } => {
                let target = Self::resolve(&root, path, true)?;
                let link = fs::read_link(&target).map_err(io::Error::other)?;
                Ok((
                    CacheReply::Link(link.to_string_lossy().into_owned()),
                    Vec::new(),
                ))
            }
            CacheOp::Fsync { path } => {
                let target = Self::resolve(&root, path, true)?;
                File::open(&target)
                    .and_then(|file| file.sync_all())
                    .map_err(io::Error::other)?;
                Ok((CacheReply::Ok, Vec::new()))
            }
        }
    }
}

fn create_file(path: &Path, mode: u32) -> Result<(), io::Error> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map(|_| ())
        .or_else(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(io::Error::other)
}

fn set_mtime(path: &Path, mtime_ms: i64) -> Result<(), io::Error> {
    let times =
        FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_millis(mtime_ms.max(0) as u64));
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_times(times))
        .map_err(io::Error::other)
}

// A path's attributes, or `None` when nothing is there: the mount turns the second
// into ENOENT rather than an I/O error.
fn attr_of(path: &Path, follow: bool) -> Result<Option<CacheAttr>, io::Error> {
    let metadata = match if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    } {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io::Error::other(error)),
    };
    let kind = if metadata.is_dir() {
        CacheKind::Dir
    } else if metadata.file_type().is_symlink() {
        CacheKind::Symlink
    } else {
        CacheKind::File
    };
    Ok(Some(CacheAttr {
        ino: metadata.ino(),
        kind,
        size: metadata.len(),
        mode: metadata.mode(),
        nlink: metadata.nlink() as u32,
        mtime_ms: seconds_ms(metadata.mtime(), metadata.mtime_nsec()),
        atime_ms: seconds_ms(metadata.atime(), metadata.atime_nsec()),
        ctime_ms: seconds_ms(metadata.ctime(), metadata.ctime_nsec()),
    }))
}

fn attrs_or_missing(path: &Path, follow: bool) -> Result<CacheReply, io::Error> {
    Ok(match attr_of(path, follow)? {
        Some(attr) => CacheReply::Attr(attr),
        None => CacheReply::Missing,
    })
}

fn seconds_ms(seconds: i64, nanos: i64) -> i64 {
    seconds.saturating_mul(1_000) + nanos / 1_000_000
}

// The handler that answers cache calls on a master's endpoint.
#[derive(Clone)]
pub(crate) struct CacheHandler {
    service: Arc<CacheService>,
}

impl CacheHandler {
    pub(crate) fn new(service: Arc<CacheService>) -> Self {
        Self { service }
    }
}

impl fmt::Debug for CacheHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CacheHandler")
    }
}

impl ProtocolHandler for CacheHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // A mount keeps this connection for its whole life and sends every read, stat
        // and write on it, so each stream is answered as it arrives.
        loop {
            let Ok((send, recv)) = connection.accept_bi().await else {
                return Ok(());
            };
            let handler = self.clone();
            tokio::spawn(async move { handler.answer_cache(send, recv).await });
        }
    }
}

impl CacheHandler {
    async fn answer_cache(&self, mut send: SendStream, mut recv: RecvStream) {
        let request: CacheRequest = match read_frame(&mut recv).await {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "an unreadable cache call arrived");
                return;
            }
        };
        if request.token != self.service.token() {
            let _ = write_frame(
                &mut send,
                &CacheReply::Failed("this host does not hold the master's token".into()),
            )
            .await;
            return;
        }
        let body = match &request.op {
            CacheOp::Write { len, .. } => {
                match read_body(&mut recv, u64::from(*len), MAX_BLOB_BYTES).await {
                    Ok(body) => body,
                    Err(error) => {
                        let _ =
                            write_frame(&mut send, &CacheReply::Failed(error.to_string())).await;
                        return;
                    }
                }
            }
            _ => Vec::new(),
        };
        let (reply, bytes) = self.service.answer(&request, &body);
        if write_frame(&mut send, &reply).await.is_err() {
            return;
        }
        if !bytes.is_empty() && send.write_all(&bytes).await.is_err() {
            return;
        }
        let _ = send.finish();
    }
}
