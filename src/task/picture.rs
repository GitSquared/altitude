use std::{
    fs, io,
    path::{Path, PathBuf},
};

// A picture in a signal body is named by the digest of its bytes, so the body means
// the same picture on every machine that holds them. This module is the only place
// that knows that: the interface draws them, the tool layer stores them, a remote
// fetches them, and a tracker uploads them — all through these five calls.
pub const PREFIX: &str = "altitude-image:";

// A picture is read whole into memory, so there is a ceiling: a signal is not a way to
// move a video.
pub const LARGEST: u64 = 8 * 1024 * 1024;

// A digest is the whole name, so nothing else may appear in one: this is also what
// keeps a request from naming a file outside the store.
pub fn is_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub struct Picture {
    pub id: String,
    pub alt: String,
}

// The pictures a body names, and the body with them taken out. The alt text stays
// with the picture: it is the only name a digest has.
pub fn take(body: &str) -> (String, Vec<Picture>) {
    let mut text = String::with_capacity(body.len());
    let mut pictures = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("![") {
        let after = &rest[start..];
        let Some(close) = after.find("](") else {
            break;
        };
        let Some(end) = after[close..].find(')').map(|at| close + at) else {
            break;
        };
        let target = &after[close + 2..end];
        text.push_str(&rest[..start]);
        match target.strip_prefix(PREFIX) {
            Some(id) => pictures.push(Picture {
                id: id.to_owned(),
                alt: after[2..close].trim().to_owned(),
            }),
            None => text.push_str(&after[..=end]),
        }
        rest = &after[end + 1..];
    }
    text.push_str(rest);
    (text, pictures)
}

// Content addressed: storing the same bytes twice is one file, and the digest is the
// whole reference, so nothing else has to be recorded about it.
pub fn store(directory: &Path, bytes: &[u8]) -> io::Result<String> {
    let id = blake3::hash(bytes).to_hex().to_string();
    let path = directory.join(&id);
    if path.is_file() {
        return Ok(id);
    }
    fs::create_dir_all(directory)?;
    let staging = directory.join(format!(".{id}.staged"));
    fs::write(&staging, bytes)?;
    fs::rename(&staging, &path)?;
    Ok(id)
}

pub fn path(directory: &Path, id: &str) -> PathBuf {
    directory.join(id)
}

pub fn read(directory: &Path, id: &str) -> io::Result<Vec<u8>> {
    fs::read(path(directory, id))
}

// What a picture weighs, which is all a reader needs to know about one it cannot see.
pub fn bytes(directory: &Path, id: &str) -> Option<u64> {
    fs::metadata(path(directory, id))
        .ok()
        .map(|entry| entry.len())
}
