use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const API_ROOT: &str = "https://api.github.com";
const MAX_PAGES: usize = 50;
const PAGE_SIZE: usize = 100;
const MAX_ITEMS: usize = MAX_PAGES * PAGE_SIZE;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const TOTAL_REQUEST_BUDGET: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubActivity {
    pub owner: String,
    pub repository: String,
    pub repository_url: String,
    pub issues_url: String,
    pub pulls_url: String,
    pub issue_count: Option<usize>,
    pub pull_request_count: Option<usize>,
    pub recent_commits: Vec<CommitSummary>,
    pub checked_at: Option<String>,
    /// unchecked, fresh, rateLimited, unauthorized, authExpired, or unavailable
    pub remote_status: String,
    /// notConfigured, or expired
    pub authentication: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitSummary {
    pub hash: String,
    pub subject: String,
    pub url: String,
}

pub fn valid_repository_name(owner: &str, repository: &str) -> bool {
    valid_owner(owner) && valid_repository(repository)
}

impl GitHubActivity {
    pub fn unchecked(owner: &str, repository: &str) -> Self {
        let base = format!("https://github.com/{owner}/{repository}");
        Self {
            owner: owner.to_owned(),
            repository: repository.to_owned(),
            repository_url: base.clone(),
            issues_url: format!("{base}/issues"),
            pulls_url: format!("{base}/pulls"),
            issue_count: None,
            pull_request_count: None,
            recent_commits: Vec::new(),
            checked_at: None,
            remote_status: "unchecked".into(),
            authentication: "notConfigured".into(),
        }
    }
}

pub async fn fetch_activity(owner: &str, repository: &str, token: Option<&str>) -> GitHubActivity {
    let mut result = GitHubActivity::unchecked(owner, repository);
    let deadline = Instant::now() + TOTAL_REQUEST_BUDGET;
    if token.is_some() {
        result.authentication = "available".into();
    }
    if !valid_repository_name(owner, repository) {
        result.remote_status = "unavailable".into();
        return result;
    }
    let client = match Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent(concat!("Navis-Fabrica/", env!("CARGO_PKG_VERSION")))
        .default_headers(default_headers())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            result.remote_status = "unavailable".into();
            return result;
        }
    };

    let repo_path = format!("repos/{owner}/{repository}");
    match get_json(
        &client,
        &format!("{API_ROOT}/{repo_path}/issues?state=open&per_page={PAGE_SIZE}&page=1"),
        token,
        deadline,
    )
    .await
    {
        Ok((items, headers)) => {
            let mut count = items
                .iter()
                .filter(|item| item.get("pull_request").is_none())
                .count();
            let mut next = next_page(&headers);
            let mut page = 1;
            let mut complete = true;
            while let Some(url) = next {
                if page >= MAX_PAGES {
                    complete = false;
                    break;
                }
                match get_json(&client, &url, token, deadline).await {
                    Ok((items, headers)) => {
                        count += items
                            .iter()
                            .filter(|item| item.get("pull_request").is_none())
                            .count();
                        page += 1;
                        next = next_page(&headers);
                    }
                    Err(status) => {
                        result.remote_status = status;
                        return result;
                    }
                }
            }
            if complete {
                result.issue_count = Some(count);
            }
        }
        Err(status) => {
            result.remote_status = status;
            if result.remote_status == "authExpired" {
                result.authentication = "expired".into();
            }
            return result;
        }
    }

    match count_collection(
        &client,
        &format!("{API_ROOT}/{repo_path}/pulls?state=open&per_page={PAGE_SIZE}&page=1"),
        token,
        deadline,
    )
    .await
    {
        Ok(count) => result.pull_request_count = count,
        Err(status) => {
            result.remote_status = status;
            if result.remote_status == "authExpired" {
                result.authentication = "expired".into();
            }
            return result;
        }
    }

    match get_json(
        &client,
        &format!("{API_ROOT}/{repo_path}/commits?per_page=5&page=1"),
        token,
        deadline,
    )
    .await
    {
        Ok((items, _)) => {
            result.recent_commits = items.iter().filter_map(commit_summary).collect();
            result.remote_status = "fresh".into();
            result.checked_at = Some(now_iso());
        }
        Err(status) => {
            if status == "emptyRepository" {
                result.remote_status = "fresh".into();
                result.checked_at = Some(now_iso());
                return result;
            }
            result.remote_status = status;
            if result.remote_status == "authExpired" {
                result.authentication = "expired".into();
            }
        }
    }
    result
}

fn default_headers() -> header::HeaderMap {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::ACCEPT,
        header::HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        "x-github-api-version",
        header::HeaderValue::from_static("2022-11-28"),
    );
    headers
}

fn valid_owner(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

fn valid_repository(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 100
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
}

async fn get_json(
    client: &Client,
    url: &str,
    token: Option<&str>,
    deadline: Instant,
) -> Result<(Vec<Value>, header::HeaderMap), String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("unavailable".into());
    }
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request = request.timeout(remaining.min(Duration::from_secs(12)));
    let response = request.send().await.map_err(|_| "unavailable".to_owned())?;
    if !response.status().is_success() {
        let remaining = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok());
        let retry_after = response.headers().contains_key("retry-after");
        return Err(status_for(response.status(), remaining, retry_after));
    }
    let headers = response.headers().clone();
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "unavailable".to_owned())?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("unavailable".into());
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| "unavailable".to_owned())?;
    let Some(items) = value.as_array() else {
        return Err("unavailable".into());
    };
    Ok((items.clone(), headers))
}

async fn count_collection(
    client: &Client,
    first_url: &str,
    token: Option<&str>,
    deadline: Instant,
) -> Result<Option<usize>, String> {
    let mut count = 0;
    let mut url = Some(first_url.to_owned());
    let mut page = 0;
    while let Some(next_url) = url {
        if page >= MAX_PAGES {
            return Ok(None);
        }
        let (items, headers) = get_json(client, &next_url, token, deadline).await?;
        count += items.len();
        if count >= MAX_ITEMS && next_page(&headers).is_some() {
            return Ok(None);
        }
        url = next_page(&headers);
        page += 1;
    }
    Ok(Some(count))
}

fn next_page(headers: &header::HeaderMap) -> Option<String> {
    let link = headers.get(header::LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let mut pieces = part.split(';');
        let url = pieces.next()?.trim().strip_prefix('<')?.strip_suffix('>')?;
        if !url.starts_with(&format!("{API_ROOT}/")) {
            return None;
        }
        pieces
            .any(|piece| piece.trim() == "rel=\"next\"")
            .then(|| url.to_owned())
    })
}

fn status_for(status: StatusCode, rate_remaining: Option<&str>, retry_after: bool) -> String {
    if status == StatusCode::CONFLICT {
        return "emptyRepository".into();
    }
    if status == StatusCode::UNAUTHORIZED {
        return "authExpired".into();
    }
    if status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN {
        if rate_remaining == Some("0") || retry_after {
            return "rateLimited".into();
        }
        return "unauthorized".into();
    }
    if status.as_u16() == 429 {
        return "rateLimited".into();
    }
    "unavailable".into()
}

fn commit_summary(value: &Value) -> Option<CommitSummary> {
    let hash = value.get("sha")?.as_str()?;
    let commit = value.get("commit")?;
    let message = commit.get("message")?.as_str()?;
    let url = value.get("html_url")?.as_str()?;
    if !url.starts_with("https://github.com/") {
        return None;
    }
    Some(CommitSummary {
        hash: hash.chars().take(7).collect(),
        subject: message.lines().next().unwrap_or_default().to_owned(),
        url: url.to_owned(),
    })
}

fn now_iso() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_the_next_link_from_github_pagination_headers() {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::LINK, header::HeaderValue::from_static("<https://api.github.com/repos/a/b/issues?page=2>; rel=\"next\", <https://api.github.com/repos/a/b/issues?page=7>; rel=\"last\""));
        assert_eq!(
            next_page(&headers).as_deref(),
            Some("https://api.github.com/repos/a/b/issues?page=2")
        );
    }

    #[test]
    fn maps_rate_limit_and_access_denial_without_exposing_response_bodies() {
        assert_eq!(
            status_for(StatusCode::FORBIDDEN, Some("0"), false),
            "rateLimited"
        );
        assert_eq!(status_for(StatusCode::FORBIDDEN, None, true), "rateLimited");
        assert_eq!(
            status_for(StatusCode::NOT_FOUND, None, false),
            "unauthorized"
        );
        assert_eq!(
            status_for(StatusCode::UNAUTHORIZED, None, false),
            "authExpired"
        );
        assert_eq!(
            status_for(StatusCode::CONFLICT, None, false),
            "emptyRepository"
        );
    }

    #[test]
    fn parses_commit_summaries_and_tolerates_missing_fields() {
        let commit = serde_json::json!({"sha":"abcdef123456", "html_url":"https://github.com/a/b/commit/abcdef123456", "commit":{"message":"first line\nbody"}});
        let summary = commit_summary(&commit).unwrap();
        assert_eq!(summary.hash, "abcdef1");
        assert_eq!(summary.subject, "first line");
        assert!(commit_summary(&serde_json::json!({"sha":"abc"})).is_none());
    }

    #[test]
    fn validates_owner_and_repository_segments_before_url_construction() {
        assert!(valid_repository_name("Kevin-Hozak", "Navis_Fabrica.repo"));
        assert!(!valid_repository_name("../private", "repo"));
        assert!(!valid_repository_name("owner/repository", "repo"));
        assert!(!valid_repository_name("owner", ".."));
    }
}
