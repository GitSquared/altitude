#[cfg(target_os = "linux")]
pub(crate) mod mount;
pub(crate) mod protocol;
pub(crate) mod service;

#[cfg(target_os = "linux")]
pub(crate) use mount::{CacheFs, mount};
pub(crate) use protocol::CACHE_ALPN;
pub(crate) use service::{CacheHandler, CacheService};
pub(crate) mod share;
