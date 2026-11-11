use serde::{Deserialize, Serialize};

// The ALPN a worker dials its master on to reach a project's cache. It is neither
// the link (clients and their tasks) nor the worker ALPN (the master asking a host
// for sandboxes): it is a host asking one master for one project's cache.
pub(crate) const CACHE_ALPN: &[u8] = b"altitude/cache/1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CacheRequest {
    pub token: String,
    pub owner: String,
    pub project: String,
    pub op: CacheOp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum CacheOp {
    Getattr {
        path: String,
    },
    Setattr {
        path: String,
        size: Option<u64>,
        mode: Option<u32>,
        mtime_ms: Option<i64>,
    },
    Lookup {
        path: String,
    },
    ReadDir {
        path: String,
    },
    Read {
        path: String,
        offset: u64,
        size: u32,
    },
    // The bytes follow the request: `len` of them, written where `offset` says.
    Write {
        path: String,
        offset: u64,
        len: u32,
    },
    Create {
        path: String,
        mode: u32,
    },
    Mkdir {
        path: String,
        mode: u32,
    },
    Unlink {
        path: String,
    },
    Rmdir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Symlink {
        target: String,
        path: String,
    },
    ReadLink {
        path: String,
    },
    Fsync {
        path: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum CacheKind {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CacheAttr {
    pub ino: u64,
    pub kind: CacheKind,
    pub size: u64,
    pub mode: u32,
    pub nlink: u32,
    pub mtime_ms: i64,
    pub atime_ms: i64,
    pub ctime_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum CacheReply {
    Attr(CacheAttr),
    Entries(Vec<(String, CacheAttr)>),
    // The bytes follow the reply: `len` of them.
    Data { len: u32 },
    Written { len: u32 },
    Link(String),
    Ok,
    Missing,
    Failed(String),
}
