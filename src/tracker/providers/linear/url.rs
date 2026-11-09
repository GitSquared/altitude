use crate::tracker::IssueSummary;

pub(super) fn rank(results: &mut [IssueSummary], term: &str) {
    let term = term.to_ascii_lowercase();
    results.sort_by_cached_key(|summary| {
        let identifier = summary.reference.identifier.to_ascii_lowercase();
        let title = summary.title.to_ascii_lowercase();
        if identifier == term {
            0
        } else if title == term {
            1
        } else if title.starts_with(&term) {
            2
        } else if title.contains(&term) {
            3
        } else {
            4
        }
    });
}

pub(super) fn workspace_slug_from_url(value: &str) -> Option<String> {
    let url = ::url::Url::parse(value).ok()?;
    url.path_segments()?.next().map(str::to_owned)
}

pub(super) fn issue_identifier_from_url(value: &str) -> Option<String> {
    let url = ::url::Url::parse(value).ok()?;
    if url.scheme() != "https" || url.host_str() != Some("linear.app") {
        return None;
    }
    let parts: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    let [_workspace, "issue", identifier, ..] = parts.as_slice() else {
        return None;
    };
    looks_like_identifier(identifier).then(|| identifier.to_ascii_uppercase())
}

pub(super) fn looks_like_identifier(value: &str) -> bool {
    let Some((key, number)) = value.rsplit_once('-') else {
        return false;
    };
    !key.is_empty()
        && key.len() <= 10
        && key.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && key.chars().all(|c| c.is_ascii_alphanumeric())
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

pub(crate) fn looks_like_issue(value: &str) -> bool {
    issue_identifier_from_url(value).is_some() || looks_like_identifier(value)
}
