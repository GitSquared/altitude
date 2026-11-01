const LONGEST: usize = 2_048;

#[derive(Debug, PartialEq, Eq)]
pub struct Link {
    url: String,
}

impl Link {
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
