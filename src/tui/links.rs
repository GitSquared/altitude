//! What a sandbox may open on this machine.
//!
//! A program in a sandbox has no browser, so it hands the address to the
//! terminal Altitude streams, and Altitude opens it here. The address is the
//! sandbox's to choose, so only a page gets through: `file://`, `javascript:`
//! and anything else a launcher might treat as something other than a page are
//! refused, not escaped.

/// The longest address this will open, which is longer than any login page and
/// shorter than a payload.
const LONGEST: usize = 2_048;

/// An address a sandbox asked for, checked into something a browser may be given.
#[derive(Debug, PartialEq, Eq)]
pub struct Link {
    url: String,
}

impl Link {
    /// What a browser may be handed, or nothing.
    pub fn parse(url: &str) -> Option<Self> {
        if url.len() > LONGEST || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return None;
        }
        let (scheme, rest) = url.split_once("://")?;
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return None;
        }
        let host = rest.split(['/', '?', '#']).next()?;
        (!host.is_empty()).then(|| Self {
            url: url.to_owned(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}
