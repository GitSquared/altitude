use std::time::Duration;

use reqwest::{Method, Response, StatusCode, header};
use serde_json::Value;

use crate::tracker::TrackerError;

const API_URL: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PAGES: usize = 20;

// The REST and GraphQL calls both halves of the GitHub tracker make. Who it speaks
// as is the caller's: an operator's own sign-in on a client, the app's installation
// on a remote.
pub(crate) struct GitHubApi {
    client: reqwest::Client,
}

impl GitHubApi {
    pub(crate) fn new() -> Result<Self, TrackerError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent("altitude")
            .build()
            .map_err(|_| TrackerError::new("could not build the GitHub HTTP client"))?;
        Ok(Self { client })
    }

    pub(crate) async fn get(&self, token: &str, path: &str) -> Result<Value, TrackerError> {
        self.send(token, Method::GET, path, None).await
    }

    pub(crate) async fn send(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, TrackerError> {
        let response = self.request(token, method, &url_of(path), body).await?;
        read_json(response).await
    }

    // A call whose only answer is that it happened: a label that was never there
    // is as removed as one that was, so a 404 is the same outcome as a 204.
    pub(crate) async fn send_tolerating_missing(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(), TrackerError> {
        let response = self.request(token, method, &url_of(path), body).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        read_json(response).await.map(|_| ())
    }

    // Every page of a list, following GitHub's own next links rather than counting
    // pages, so a list that grows while it is read is not read twice.
    pub(crate) async fn pages(&self, token: &str, path: &str) -> Result<Vec<Value>, TrackerError> {
        let mut items = Vec::new();
        let mut next = Some(url_of(path));
        for _ in 0..MAX_PAGES {
            let Some(url) = next.take() else {
                return Ok(items);
            };
            let response = self.request(token, Method::GET, &url, None).await?;
            next = next_page(&response);
            match read_json(response).await? {
                Value::Array(page) => items.extend(page),
                _ => {
                    return Err(TrackerError::new(
                        "GitHub returned a list that was not a list",
                    ));
                }
            }
        }
        if next.is_some() {
            return Err(TrackerError::new(
                "this list is longer than altitude reads from GitHub in one pass",
            ));
        }
        Ok(items)
    }

    pub(crate) async fn request(
        &self,
        token: &str,
        method: Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<Response, TrackerError> {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(token)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION);
        if let Some(body) = body {
            request = request.json(&body);
        }
        request
            .send()
            .await
            .map_err(|_| TrackerError::new("GitHub did not answer"))
    }
}

pub(crate) fn url_of(path: &str) -> String {
    if path.starts_with("https://") {
        path.to_owned()
    } else {
        format!("{API_URL}{path}")
    }
}

pub(crate) async fn read_json(response: Response) -> Result<Value, TrackerError> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|_| TrackerError::new("GitHub's answer could not be read"))?;
    if !status.is_success() {
        // The body carries the reason, and a status alone sends whoever reads the
        // log to guess at it.
        let reason = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|body| body.get("message").and_then(Value::as_str).map(clipped))
            .unwrap_or_else(|| clipped(&text));
        return Err(TrackerError::new(format!(
            "GitHub refused the request with HTTP {status}: {reason}"
        )));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|_| TrackerError::new("GitHub returned a response that was not JSON"))
}

fn next_page(response: &Response) -> Option<String> {
    let links = response.headers().get(header::LINK)?.to_str().ok()?;
    links.split(',').find_map(|link| {
        let (target, relation) = link.split_once(';')?;
        relation.contains("rel=\"next\"").then(|| {
            target
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_owned()
        })
    })
}

fn clipped(text: &str) -> String {
    text.trim().chars().take(200).collect()
}
