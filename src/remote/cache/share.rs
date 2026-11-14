// The path a project's cache answers on. A share serves one tree — every project's
// cache, under this pseudo root — and the path a host mounts is the subdirectory it was
// given, so the machine that hands out the path is the only one that decides which.
const CACHE_ROOT: &str = "/projects";

pub(crate) fn cache_path(project_id: &str) -> String {
    format!("{CACHE_ROOT}/{project_id}/cache")
}

// The machine an export is granted to, from the address it joined with.
pub(crate) fn client_ip(peer: &iroh::EndpointAddr) -> Option<String> {
    peer.addrs.iter().find_map(|addr| match addr {
        iroh::TransportAddr::Ip(socket) => Some(socket.ip().to_string()),
        _ => None,
    })
}
