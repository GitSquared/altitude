use std::{
    collections::HashMap,
    ffi::OsStr,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use iroh::EndpointAddr;
use tokio::runtime::Handle;

use crate::remote::LinkClient;

use super::protocol::{CacheAttr, CacheKind, CacheOp, CacheReply, CacheRequest};

// errno values, which are the same on the systems altitude builds for.
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EINVAL: i32 = 22;

// No attribute caching: the cache is written through to the master, and a stale
// size or mtime would make a build read the wrong thing.
const TTL: Duration = Duration::from_millis(0);

// A project's cache, mounted on the host where the sandbox expects it. Every
// callback is a call to the master that owns the cache; the mount is a window onto
// the master's directory, not a copy of it.
pub(crate) struct CacheFs {
    client: LinkClient,
    peer: iroh::EndpointAddr,
    token: String,
    owner: String,
    project: String,
    handle: Handle,
    // The filesystem's own runtime, kept alive as long as the session is.
    runtime: Option<tokio::runtime::Runtime>,
    paths: HashMap<u64, String>,
    inos: HashMap<String, u64>,
    next: u64,
}

impl CacheFs {
    pub(crate) fn new(
        client: LinkClient,
        peer: EndpointAddr,
        token: String,
        owner: String,
        project: String,
        handle: Handle,
    ) -> Self {
        let mut paths = HashMap::new();
        let mut inos = HashMap::new();
        paths.insert(1, String::new());
        inos.insert(String::new(), 1);
        Self {
            client,
            peer,
            token,
            runtime: None,
            owner,
            project,
            handle,
            paths,
            inos,
            next: 2,
        }
    }

    fn ino_of(&mut self, path: &str) -> u64 {
        if let Some(ino) = self.inos.get(path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.paths.insert(ino, path.to_owned());
        self.inos.insert(path.to_owned(), ino);
        ino
    }

    fn path_of(&self, ino: u64) -> Option<String> {
        self.paths.get(&ino).cloned()
    }

    fn child(&self, parent: u64, name: &OsStr) -> Option<String> {
        let name = name.to_str()?;
        if name.is_empty() || name.contains('/') {
            return None;
        }
        let parent = self.path_of(parent)?;
        Some(if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}/{name}")
        })
    }

    fn call(&self, op: CacheOp, body: &[u8]) -> Result<(CacheReply, Vec<u8>), i32> {
        let request = CacheRequest {
            token: self.token.clone(),
            owner: self.owner.clone(),
            project: self.project.clone(),
            op,
        };
        let answered =
            self.handle
                .block_on(self.client.cache_call(self.peer.clone(), &request, body));
        match &answered {
            Ok((CacheReply::Failed(reason), _)) => {
                tracing::warn!(%reason, ?op, "the master refused a cache call")
            }
            Err(error) => tracing::warn!(%error, ?op, "a cache call did not reach the master"),
            Ok(_) => {}
        }
        answered.map_err(|_| EIO)
    }

    fn attr(&mut self, path: &str, attr: &CacheAttr, uid: u32, gid: u32) -> FileAttr {
        let ino = self.ino_of(path);
        FileAttr {
            ino,
            size: attr.size,
            blocks: attr.size.div_ceil(512),
            atime: time_of(attr.atime_ms),
            mtime: time_of(attr.mtime_ms),
            ctime: time_of(attr.ctime_ms),
            crtime: time_of(attr.ctime_ms),
            kind: kind_of(attr.kind),
            perm: (attr.mode & 0o7777) as u16,
            nlink: attr.nlink,
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn attr_for(&mut self, path: &str, uid: u32, gid: u32) -> Result<FileAttr, i32> {
        match self.call(
            CacheOp::Getattr {
                path: path.to_owned(),
            },
            &[],
        ) {
            Ok((CacheReply::Attr(attr), _)) => Ok(self.attr(path, &attr, uid, gid)),
            Ok((CacheReply::Missing, _)) => Err(ENOENT),
            _ => Err(EIO),
        }
    }
}

impl Filesystem for CacheFs {
    fn lookup(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(child) = self.child(parent, name) else {
            reply.error(ENOENT);
            return;
        };
        match self.attr_for(&child, req.uid(), req.gid()) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(code) => reply.error(code),
        }
    }

    fn getattr(&mut self, req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(path) = self.path_of(ino) else {
            reply.error(ENOENT);
            return;
        };
        match self.attr_for(&path, req.uid(), req.gid()) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(code) => reply.error(code),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.path_of(ino) else {
            reply.error(ENOENT);
            return;
        };
        let mtime_ms = match mtime {
            Some(TimeOrNow::SpecificTime(time)) => Some(ms_of(time)),
            _ => None,
        };
        let op = CacheOp::Setattr {
            path: path.clone(),
            size,
            mode,
            mtime_ms,
        };
        match self.call(op, &[]) {
            Ok((CacheReply::Attr(_), _)) => match self.attr_for(&path, req.uid(), req.gid()) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(code) => reply.error(code),
            },
            _ => reply.error(EIO),
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let Some(path) = self.path_of(ino) else {
            reply.error(ENOENT);
            return;
        };
        match self.call(CacheOp::ReadLink { path }, &[]) {
            Ok((CacheReply::Link(target), _)) => reply.data(target.as_bytes()),
            _ => reply.error(EIO),
        }
    }

    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(child) = self.child(parent, name) else {
            reply.error(EINVAL);
            return;
        };
        let op = CacheOp::Mkdir {
            path: child.clone(),
            mode: mode & !umask,
        };
        match self.call(op, &[]) {
            Ok((CacheReply::Ok, _)) => match self.attr_for(&child, req.uid(), req.gid()) {
                Ok(attr) => reply.entry(&TTL, &attr, 0),
                Err(code) => reply.error(code),
            },
            Ok((CacheReply::Failed(_), _)) => reply.error(EIO),
            _ => reply.error(EIO),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, false, reply);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, true, reply);
    }

    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let Some(child) = self.child(parent, link_name) else {
            reply.error(EINVAL);
            return;
        };
        let op = CacheOp::Symlink {
            target: target.to_string_lossy().into_owned(),
            path: child.clone(),
        };
        match self.call(op, &[]) {
            Ok((CacheReply::Ok, _)) => match self.attr_for(&child, req.uid(), req.gid()) {
                Ok(attr) => reply.entry(&TTL, &attr, 0),
                Err(code) => reply.error(code),
            },
            _ => reply.error(EIO),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let (Some(from), Some(to)) = (self.child(parent, name), self.child(newparent, newname))
        else {
            reply.error(EINVAL);
            return;
        };
        match self.call(CacheOp::Rename { from, to }, &[]) {
            Ok((CacheReply::Ok, _)) => reply.ok(),
            _ => reply.error(EIO),
        }
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(path) = self.path_of(ino) else {
            reply.error(ENOENT);
            return;
        };
        let op = CacheOp::Read {
            path,
            offset: offset.max(0) as u64,
            size,
        };
        match self.call(op, &[]) {
            Ok((CacheReply::Data { .. }, bytes)) => reply.data(&bytes),
            _ => reply.error(EIO),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some(path) = self.path_of(ino) else {
            reply.error(ENOENT);
            return;
        };
        let op = CacheOp::Write {
            path,
            offset: offset.max(0) as u64,
            len: data.len() as u32,
        };
        match self.call(op, data) {
            Ok((CacheReply::Written { len }, _)) => reply.written(len),
            _ => reply.error(EIO),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let Some(path) = self.path_of(ino) else {
            reply.error(ENOENT);
            return;
        };
        match self.call(CacheOp::Fsync { path }, &[]) {
            Ok((CacheReply::Ok, _)) => reply.ok(),
            _ => reply.error(EIO),
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(path) = self.path_of(ino) else {
            reply.error(ENOENT);
            return;
        };
        let entries = match self.call(CacheOp::ReadDir { path: path.clone() }, &[]) {
            Ok((CacheReply::Entries(entries), _)) => entries,
            Ok((CacheReply::Missing, _)) => {
                reply.error(ENOENT);
                return;
            }
            _ => {
                reply.error(EIO);
                return;
            }
        };
        let mut listed: Vec<(String, u64, FileType)> = vec![
            (".".to_owned(), ino, FileType::Directory),
            ("..".to_owned(), 1, FileType::Directory),
        ];
        for (name, attr) in entries {
            let child = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let child_ino = self.ino_of(&child);
            listed.push((name, child_ino, kind_of(attr.kind)));
        }
        for (index, (name, entry_ino, kind)) in listed.iter().enumerate().skip(offset as usize) {
            if reply.add(*entry_ino, (index + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(child) = self.child(parent, name) else {
            reply.error(EINVAL);
            return;
        };
        let op = CacheOp::Create {
            path: child.clone(),
            mode: mode & !umask,
        };
        match self.call(op, &[]) {
            Ok((CacheReply::Attr(_), _)) => match self.attr_for(&child, req.uid(), req.gid()) {
                Ok(attr) => reply.created(&TTL, &attr, 0, 0, 0),
                Err(code) => reply.error(code),
            },
            _ => reply.error(EIO),
        }
    }

    fn access(&mut self, _req: &Request<'_>, _ino: u64, _mask: i32, reply: ReplyEmpty) {
        reply.ok();
    }
}

impl CacheFs {
    fn remove(&mut self, parent: u64, name: &OsStr, directory: bool, reply: ReplyEmpty) {
        let Some(child) = self.child(parent, name) else {
            reply.error(ENOENT);
            return;
        };
        let op = if directory {
            CacheOp::Rmdir { path: child }
        } else {
            CacheOp::Unlink { path: child }
        };
        match self.call(op, &[]) {
            Ok((CacheReply::Ok, _)) => reply.ok(),
            Ok((CacheReply::Missing, _)) => reply.error(ENOENT),
            _ => reply.error(EIO),
        }
    }
}

// Mount the cache at a path the sandbox already mounts. The session unmounts when
// the returned handle drops.
//
// The filesystem runs itself: its calls to the master happen on a runtime of its own,
// created here and moved into the session. Borrowing this process's runtime would mean
// that whichever thread is waiting on a cache read — the one that has to answer the FUSE
// request — is the same thread the request needs, which is a deadlock with no timeout.
pub(crate) fn mount(mut fs: CacheFs, mountpoint: &Path) -> Result<BackgroundSession, String> {
    // Its own thread, not a runtime the FUSE thread would have to drive: the caller
    // blocks in a syscall that only this filesystem can answer, so the answer has to be
    // made somewhere the caller is not.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    fs.handle = runtime.handle().clone();
    fs.runtime = Some(runtime);
    let options = [
        MountOption::FSName("altitude-cache".to_owned()),
        MountOption::DefaultPermissions,
    ];
    fuser::spawn_mount2(fs, mountpoint, &options).map_err(|error| error.to_string())
}

fn kind_of(kind: CacheKind) -> FileType {
    match kind {
        CacheKind::File => FileType::RegularFile,
        CacheKind::Dir => FileType::Directory,
        CacheKind::Symlink => FileType::Symlink,
    }
}

fn time_of(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

fn ms_of(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}
