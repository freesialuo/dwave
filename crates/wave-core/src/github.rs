use std::collections::BTreeSet;
use std::env;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::Url;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::filter::{github_issue_number, repo_name};
use crate::gateway::{Gateway, GetRequest};
use crate::store::{Freshness, Store};

const HOST: &str = "api.github.com";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GhIssueStatus {
    pub state: String,
    pub assignees: Vec<String>,
    pub linked_prs: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct GitHubResult {
    pub status: Option<GhIssueStatus>,
    pub as_of: Option<DateTime<Utc>>,
    pub warnings: Vec<String>,
    pub partial: bool,
}

impl GitHubResult {
    fn warn(&mut self, warning: String) {
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }
}

pub trait GitHubSource {
    fn issue_status(
        &self,
        store: &Store,
        repo: &str,
        number: u64,
        freshness: Freshness,
        now: DateTime<Utc>,
    ) -> Result<GitHubResult>;
}

pub struct GitHubClient {
    gateway: Arc<Gateway>,
    ttl: Duration,
    token: OnceLock<Option<String>>,
}

#[derive(Debug)]
struct CachedPage {
    body: Value,
    etag: Option<String>,
    fetched_at: DateTime<Utc>,
    next: Option<String>,
}

impl GitHubClient {
    pub fn new(gateway: Arc<Gateway>, ttl: Duration) -> Self {
        Self {
            gateway,
            ttl,
            token: OnceLock::new(),
        }
    }

    pub fn enrich_issue(
        &self,
        store: &Store,
        issue: &Value,
        freshness: Freshness,
        now: DateTime<Utc>,
    ) -> Result<GitHubResult> {
        match (repo_name(issue), github_issue_number(issue)) {
            (Some(repo), Some(number)) => self.enrich(store, repo, number, freshness, now),
            _ => Ok(GitHubResult {
                partial: true,
                warnings: vec!["Wave issue has no GitHub repository or issue number".to_string()],
                ..GitHubResult::default()
            }),
        }
    }

    pub fn enrich(
        &self,
        store: &Store,
        repo: &str,
        number: u64,
        freshness: Freshness,
        now: DateTime<Utc>,
    ) -> Result<GitHubResult> {
        self.issue_status(store, repo, number, freshness, now)
    }

    fn page(
        &self,
        store: &Store,
        url: &str,
        freshness: Freshness,
        now: DateTime<Utc>,
        result: &mut GitHubResult,
    ) -> Result<Option<CachedPage>> {
        let cached = read_cache(store, url)?;
        let as_of = cached.as_ref().map(|page| page.fetched_at);
        if !freshness.allows_fetch(as_of, self.ttl, now) {
            match &cached {
                Some(page) if Freshness::Default.allows_fetch(as_of, self.ttl, now) => {
                    result.warn(format!("using stale GitHub cache from {}", page.fetched_at));
                }
                None => {
                    result.partial = true;
                    result.warn("no cached GitHub response in offline mode".to_string());
                }
                _ => {}
            }
            return Ok(cached);
        }

        let token = self.token.get_or_init(github_token);
        if token.is_none() {
            result.warn("GitHub is unauthenticated (60 requests/hour)".to_string());
        }
        let mut request = GetRequest::new(url);
        request.bearer_token.clone_from(token);
        request.accept = Some("application/vnd.github+json".into());
        request.etag = cached.as_ref().and_then(|page| page.etag.clone());
        let response = match self.gateway.get_json::<Value>(request) {
            Ok(response) => response,
            Err(error) => {
                result.partial = true;
                result.warn(format!("GitHub request failed for {url}: {error}"));
                return Ok(cached);
            }
        };
        let page = match response.status {
            304 => {
                let Some(mut page) = cached else {
                    result.partial = true;
                    result.warn(format!(
                        "GitHub returned 304 without cached content for {url}"
                    ));
                    return Ok(None);
                };
                page.fetched_at = now;
                if response.etag.is_some() {
                    page.etag = response.etag;
                }
                page
            }
            404 => CachedPage {
                body: Value::Null,
                etag: None,
                fetched_at: now,
                next: None,
            },
            _ => {
                let Some(body) = response.body else {
                    result.partial = true;
                    result.warn(format!("GitHub returned no content for {url}"));
                    return Ok(cached);
                };
                let timeline = Url::parse(url)?.path().ends_with("/timeline");
                if !valid_page(&body, timeline) {
                    result.partial = true;
                    result.warn(format!("GitHub returned unexpected content for {url}"));
                    return Ok(cached);
                }
                let next = response
                    .headers
                    .get_all(reqwest::header::LINK)
                    .iter()
                    .filter_map(|header| header.to_str().ok())
                    .find_map(next_link)
                    .map(str::to_owned);
                CachedPage {
                    body,
                    etag: response.etag,
                    fetched_at: now,
                    next,
                }
            }
        };
        write_cache(store, url, &page)?;
        Ok(Some(page))
    }
}

impl GitHubSource for GitHubClient {
    fn issue_status(
        &self,
        store: &Store,
        repo: &str,
        number: u64,
        freshness: Freshness,
        now: DateTime<Utc>,
    ) -> Result<GitHubResult> {
        let mut result = GitHubResult::default();
        let issue_url = issue_url(repo, number)?;
        let Some(issue) = self.page(store, &issue_url, freshness, now, &mut result)? else {
            return Ok(result);
        };
        result.as_of = Some(issue.fetched_at);
        let Some(state) = issue.body.get("state").and_then(Value::as_str) else {
            result.partial = true;
            result.warn(format!("GitHub issue {repo}#{number} is unavailable"));
            return Ok(result);
        };
        let mut assignees = issue.body["assignees"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|user| user["login"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        if let Some(assignee) = issue.body["assignee"]["login"].as_str() {
            assignees.insert(assignee.to_owned());
        }

        let mut next = Some(format!("{issue_url}/timeline?per_page=100"));
        let mut visited = BTreeSet::new();
        let mut linked_prs = BTreeSet::new();
        while let Some(url) = next.take() {
            if !safe_api_url(&url) || !visited.insert(url.clone()) {
                result.partial = true;
                result.warn("GitHub supplied an invalid timeline pagination link".to_string());
                return Ok(result);
            }
            let Some(page) = self.page(store, &url, freshness, now, &mut result)? else {
                return Ok(result);
            };
            result.as_of = result.as_of.map(|time| time.min(page.fetched_at));
            let Some(events) = page.body.as_array() else {
                result.partial = true;
                result.warn(format!(
                    "GitHub timeline for {repo}#{number} is unavailable"
                ));
                return Ok(result);
            };
            for event in events {
                apply_pr_event(event, &mut linked_prs);
            }
            next = page.next;
        }
        result.status = Some(GhIssueStatus {
            state: state.into(),
            assignees: assignees.into_iter().collect(),
            linked_prs: linked_prs.into_iter().collect(),
        });
        Ok(result)
    }
}

fn valid_page(body: &Value, timeline: bool) -> bool {
    if timeline {
        body.is_array()
    } else {
        body.get("state").is_some_and(Value::is_string)
    }
}

fn issue_url(repo: &str, number: u64) -> Result<String> {
    let Some((owner, name)) = repo.split_once('/') else {
        bail!("GitHub repository must be owner/name");
    };
    let valid_segment = |segment: &str| {
        !segment.is_empty()
            && !matches!(segment, "." | "..")
            && segment
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    };
    ensure!(
        valid_segment(owner) && valid_segment(name),
        "invalid GitHub repository name"
    );
    ensure!(number > 0, "GitHub issue number must be positive");
    Ok(format!(
        "https://{HOST}/repos/{}/issues/{number}",
        repo.to_ascii_lowercase()
    ))
}

fn safe_api_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some(HOST)
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.path().ends_with("/timeline")
    })
}

fn next_link(header: &str) -> Option<&str> {
    header.split(',').find_map(|part| {
        let (url, parameters) = part.trim().strip_prefix('<')?.split_once('>')?;
        parameters
            .split(';')
            .any(|parameter| {
                parameter
                    .trim()
                    .split_once('=')
                    .is_some_and(|(key, value)| {
                        key.trim() == "rel"
                            && value
                                .trim()
                                .trim_matches('"')
                                .split_whitespace()
                                .any(|rel| rel == "next")
                    })
            })
            .then_some(url)
    })
}

fn apply_pr_event(event: &Value, prs: &mut BTreeSet<String>) {
    let source = &event["source"]["issue"];
    let target = &event["subject"];
    let pull_request = [source, target].into_iter().find_map(|issue| {
        let pr = issue.get("pull_request")?.as_object()?;
        pr.get("html_url")
            .and_then(Value::as_str)
            .or_else(|| issue["html_url"].as_str())
    });
    let Some(url) = pull_request else {
        return;
    };
    match event["event"].as_str() {
        Some("cross-referenced" | "connected") => {
            prs.insert(url.into());
        }
        Some("disconnected") => {
            prs.remove(url);
        }
        _ => {}
    }
}

fn github_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Some(token) = env::var(key).ok().and_then(nonempty) {
            return Some(token);
        }
    }
    let output = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        String::from_utf8(output.stdout).ok().and_then(nonempty)
    } else {
        None
    }
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn read_cache(store: &Store, url: &str) -> Result<Option<CachedPage>> {
    let row = store
        .conn
        .query_row(
            "SELECT body,etag,fetched_at,next FROM github_cache WHERE url=?1",
            [url],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get(1)?,
                    row.get::<_, String>(2)?,
                    row.get(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(body, etag, fetched_at, next)| {
        Ok(CachedPage {
            body: serde_json::from_str(&body).context("invalid cached GitHub response")?,
            etag,
            fetched_at: fetched_at
                .parse()
                .context("invalid GitHub cache timestamp")?,
            next,
        })
    })
    .transpose()
}

fn write_cache(store: &Store, url: &str, page: &CachedPage) -> Result<()> {
    store.conn.execute(
        "INSERT INTO github_cache(url,etag,body,fetched_at,next) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(url) DO UPDATE SET etag=excluded.etag,body=excluded.body,
         fetched_at=excluded.fetched_at,next=excluded.next
         WHERE excluded.fetched_at>=github_cache.fetched_at",
        params![
            url,
            page.etag,
            serde_json::to_string(&page.body)?,
            page.fetched_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
            page.next
        ],
    )?;
    Ok(())
}
