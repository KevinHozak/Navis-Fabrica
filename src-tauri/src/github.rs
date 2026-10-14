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
const MAX_ISSUE_BODY_CHARS: usize = 12_000;
const MAX_ISSUE_TITLE_CHARS: usize = 300;
const MAX_ACCEPTANCE_ITEMS: usize = 30;
const MAX_BLOCKERS: usize = 20;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryIssue {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub body: String,
    pub acceptance_criteria: Vec<String>,
    pub blocked_by: Vec<IssueLink>,
    pub blockers_truncated: bool,
    pub blockers_available: bool,
    pub parent_epic: Option<IssueLink>,
    pub pull_request_urls: Vec<String>,
    pub selectable: bool,
    pub blocked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueLink {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueSummary {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
}

pub async fn list_issues(
    owner: &str,
    repository: &str,
    token: Option<&str>,
) -> Result<Vec<IssueSummary>, String> {
    if !valid_repository_name(owner, repository) {
        return Err("The GitHub repository identity is invalid.".into());
    }
    let client = issue_client()?;
    let url = format!("{API_ROOT}/repos/{owner}/{repository}/issues?state=open&per_page=50&page=1");
    let (items, _) = get_json(&client, &url, token, Instant::now() + TOTAL_REQUEST_BUDGET).await?;
    Ok(items.iter().filter_map(issue_summary).collect())
}

pub async fn fetch_issue(
    owner: &str,
    repository: &str,
    number: u64,
    token: Option<&str>,
) -> Result<RepositoryIssue, String> {
    if !valid_repository_name(owner, repository) || number == 0 {
        return Err("The GitHub issue reference is invalid.".into());
    }
    let client = issue_client()?;
    let deadline = Instant::now() + TOTAL_REQUEST_BUDGET;
    let (items, _) = get_json(
        &client,
        &format!("{API_ROOT}/repos/{owner}/{repository}/issues/{number}"),
        token,
        deadline,
    )
    .await?;
    let item = items
        .first()
        .ok_or_else(|| "GitHub returned no issue.".to_string())?;
    let mut issue = detailed_issue(item, owner, repository)
        .ok_or_else(|| "GitHub returned an unsupported issue record.".to_string())?;
    if issue.state != "open" {
        issue.selectable = false;
        return Ok(issue);
    }
    match get_json(&client, &format!("{API_ROOT}/repos/{owner}/{repository}/issues/{number}/dependencies/blocked_by?per_page=21"), token, deadline).await {
        Ok((blockers, headers)) => {
            issue.blockers_available = true;
            issue.blockers_truncated = blockers.len() > MAX_BLOCKERS || next_page(&headers).is_some();
            issue.blocked_by = blockers.iter().take(MAX_BLOCKERS).filter_map(|item| issue_summary(item).map(|summary| IssueLink { number: summary.number, title: summary.title, url: summary.url, state: summary.state })).collect();
            issue.blocked = issue.blockers_truncated || issue.blocked_by.iter().any(|blocker| blocker.state == "open");
        }
        Err(_) => { issue.blocked = true; }
    }
    if let Some(parent_number) =
        parent_issue_number(item.get("body").and_then(Value::as_str).unwrap_or_default())
    {
        if parent_number != number {
            if let Ok((parents, _)) = get_json(
                &client,
                &format!("{API_ROOT}/repos/{owner}/{repository}/issues/{parent_number}"),
                token,
                deadline,
            )
            .await
            {
                issue.parent_epic = parents
                    .first()
                    .and_then(|parent| issue_summary(parent))
                    .map(|summary| IssueLink {
                        number: summary.number,
                        title: summary.title,
                        url: summary.url,
                        state: summary.state,
                    });
            }
        }
    }
    if let Ok((events, _)) = get_json(
        &client,
        &format!("{API_ROOT}/repos/{owner}/{repository}/issues/{number}/timeline?per_page=100"),
        token,
        deadline,
    )
    .await
    {
        for url in timeline_pull_requests(&events, owner, repository) {
            if issue.pull_request_urls.len() >= 10 {
                break;
            }
            if !issue.pull_request_urls.contains(&url) {
                issue.pull_request_urls.push(url);
            }
        }
    }
    issue.selectable = issue.state == "open" && issue.blockers_available && !issue.blocked;
    Ok(issue)
}

fn issue_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent(concat!("Navis-Fabrica/", env!("CARGO_PKG_VERSION")))
        .default_headers(default_headers())
        .build()
        .map_err(|_| "Could not prepare the GitHub connection.".into())
}

fn issue_summary(value: &Value) -> Option<IssueSummary> {
    if value.get("pull_request").is_some() {
        return None;
    }
    let number = value.get("number")?.as_u64()?;
    let url = value.get("html_url")?.as_str()?;
    if !url.starts_with("https://github.com/") {
        return None;
    }
    Some(IssueSummary {
        number,
        title: truncate_chars(
            value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Untitled issue"),
            MAX_ISSUE_TITLE_CHARS,
        ),
        url: url.to_owned(),
        state: value.get("state")?.as_str()?.to_owned(),
    })
}

fn detailed_issue(value: &Value, owner: &str, repository: &str) -> Option<RepositoryIssue> {
    let summary = issue_summary(value)?;
    let body = truncate_chars(
        value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        MAX_ISSUE_BODY_CHARS,
    );
    let acceptance_criteria = body
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let (_, text) = trimmed.split_once("[ ]")?;
            let text = text.trim();
            (!text.is_empty()).then(|| truncate_chars(text, 500))
        })
        .take(MAX_ACCEPTANCE_ITEMS)
        .collect();
    let parent_epic = parent_issue_number(&body).map(|number| IssueLink {
        number,
        title: format!("Issue #{number}"),
        url: format!("https://github.com/{owner}/{repository}/issues/{number}"),
        state: "unknown".into(),
    });
    let pull_request_urls = body_pull_requests(&body, owner, repository);
    Some(RepositoryIssue {
        number: summary.number,
        title: summary.title,
        url: summary.url,
        state: summary.state,
        body,
        acceptance_criteria,
        blocked_by: Vec::new(),
        blockers_truncated: false,
        blockers_available: false,
        parent_epic,
        pull_request_urls,
        selectable: false,
        blocked: false,
    })
}

fn parent_issue_number(body: &str) -> Option<u64> {
    body.lines()
        .find(|line| line.to_ascii_lowercase().contains("parent epic"))?
        .split('#')
        .nth(1)?
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

fn timeline_pull_requests(events: &[Value], owner: &str, repository: &str) -> Vec<String> {
    let prefix = format!("https://github.com/{owner}/{repository}/pull/");
    events
        .iter()
        .filter(|event| event.get("event").and_then(Value::as_str) == Some("cross-referenced"))
        .filter_map(|event| {
            event
                .get("source")?
                .get("issue")?
                .get("pull_request")?
                .get("html_url")?
                .as_str()
        })
        .filter(|url| pull_request_number(url, &prefix).is_some())
        .take(10)
        .filter_map(|url| {
            pull_request_number(url, &prefix).map(|number| format!("{prefix}{number}"))
        })
        .collect()
}

fn body_pull_requests(body: &str, owner: &str, repository: &str) -> Vec<String> {
    let prefix = format!("https://github.com/{owner}/{repository}/pull/");
    let marker = "https://github.com/";
    let mut remaining = body;
    let mut urls = Vec::new();
    while let Some(start) = remaining.find(marker) {
        let candidate = remaining[start..]
            .split(|ch: char| ch.is_whitespace() || matches!(ch, ')' | ']' | '>' | '"' | '\''))
            .next()
            .unwrap_or_default()
            .trim_end_matches(|ch: char| matches!(ch, ',' | '.' | ';' | ':' | '!' | '?'));
        if let Some(number) = pull_request_number(candidate, &prefix) {
            let url = format!("{prefix}{number}");
            if !urls.contains(&url) {
                urls.push(url);
                if urls.len() == 10 {
                    break;
                }
            }
        }
        remaining = &remaining[start + marker.len()..];
    }
    urls
}

fn pull_request_number<'a>(url: &'a str, prefix: &str) -> Option<&'a str> {
    let tail = strip_prefix_ascii_case_insensitive(url, prefix)?;
    let number = tail.split(['?', '#']).next()?;
    (!number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())).then_some(number)
}

fn strip_prefix_ascii_case_insensitive<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

pub fn parse_issue_reference(owner: &str, repository: &str, input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    let number = if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        trimmed
    } else {
        let prefix = format!("https://github.com/{owner}/{repository}/issues/");
        let Some(path) = strip_prefix_ascii_case_insensitive(trimmed, &prefix) else {
            return Err("Enter an issue number or a URL for this repository.".into());
        };
        path.split(['?', '#']).next().unwrap_or_default()
    };
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| "Enter a valid GitHub issue number.".to_string())?;
    Ok(number)
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
    let items = if let Some(items) = value.as_array() {
        items.clone()
    } else if value.is_object() {
        vec![value]
    } else {
        return Err("unavailable".into());
    };
    Ok((items, headers))
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

    #[test]
    fn issue_references_are_limited_to_selected_repository() {
        assert_eq!(parse_issue_reference("owner", "repo", "42").unwrap(), 42);
        assert_eq!(
            parse_issue_reference("owner", "repo", "https://github.com/owner/repo/issues/7")
                .unwrap(),
            7
        );
        assert_eq!(
            parse_issue_reference(
                "owner",
                "repo",
                "https://GitHub.com/OWNER/REPO/ISSUES/9?x=1"
            )
            .unwrap(),
            9
        );
        assert!(parse_issue_reference(
            "owner",
            "repo",
            "https://github.com/elsewhere/repo/issues/7"
        )
        .is_err());
        assert!(
            parse_issue_reference("owner", "repo", "https://github.com/owner/repo/pull/7").is_err()
        );
        assert!(parse_issue_reference("owner", "repo", "0").is_err());
    }

    #[test]
    fn issue_context_extracts_parent_and_acceptance_and_bounds_body() {
        let issue = detailed_issue(
            &serde_json::json!({
                "number": 8,
                "title": "A scoped issue",
                "html_url": "https://github.com/owner/repo/issues/8",
                "state": "open",
                "body": "Parent Epic: #3\n\n- [ ] First criterion\n- [ ] Second criterion"
            }),
            "owner",
            "repo",
        )
        .unwrap();
        assert_eq!(issue.parent_epic.unwrap().number, 3);
        assert_eq!(
            issue.acceptance_criteria,
            vec!["First criterion", "Second criterion"]
        );
        let huge = "x".repeat(MAX_ISSUE_BODY_CHARS + 20);
        let bounded = detailed_issue(
            &serde_json::json!({
                "number": 9,
                "title": "x".repeat(MAX_ISSUE_TITLE_CHARS + 20),
                "html_url": "https://github.com/owner/repo/issues/9",
                "state": "closed",
                "body": huge
            }),
            "owner",
            "repo",
        )
        .unwrap();
        assert!(!bounded.selectable);
        assert_eq!(bounded.title.chars().count(), MAX_ISSUE_TITLE_CHARS + 1);
        assert_eq!(bounded.body.chars().count(), MAX_ISSUE_BODY_CHARS + 1);
    }

    #[test]
    fn issue_context_extracts_markdown_wrapped_pull_request_links() {
        let issue = detailed_issue(
            &serde_json::json!({
                "number": 8,
                "title": "A scoped issue",
                "html_url": "https://github.com/owner/repo/issues/8",
                "state": "open",
                "body": "See [the fix](https://github.com/owner/repo/pull/12), (https://github.com/OWNER/REPO/pull/13). Also https://github.com/other/repo/pull/14"
            }),
            "owner",
            "repo",
        )
        .unwrap();
        assert_eq!(
            issue.pull_request_urls,
            vec![
                "https://github.com/owner/repo/pull/12",
                "https://github.com/owner/repo/pull/13"
            ]
        );
    }

    #[test]
    fn issue_timeline_includes_only_same_repository_cross_referenced_prs() {
        let events = vec![
            serde_json::json!({"event":"cross-referenced","source":{"issue":{"pull_request":{"html_url":"https://github.com/owner/repo/pull/12"}}}}),
            serde_json::json!({"event":"cross-referenced","source":{"issue":{"pull_request":{"html_url":"https://github.com/other/repo/pull/13"}}}}),
            serde_json::json!({"event":"closed","source":{"issue":{"pull_request":{"html_url":"https://github.com/owner/repo/pull/14"}}}}),
        ];
        assert_eq!(
            timeline_pull_requests(&events, "owner", "repo"),
            vec!["https://github.com/owner/repo/pull/12"]
        );
    }
}
