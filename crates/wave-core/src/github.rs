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

use crate::engine::Freshness;
use crate::filter::{github_issue_number, repo_name};
use crate::gateway::{Gateway, GetRequest};
use crate::store::Store;

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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use chrono::TimeDelta;
    use reqwest::header::{HeaderMap, ETAG, LINK};
    use serde_json::json;

    use super::*;
    use crate::gateway::{HostPolicy, HttpResponse, HttpTransport, TransportError};

    const ISSUE: &str = "https://api.github.com/repos/o/r/issues/17";
    const TIMELINE: &str = "https://api.github.com/repos/o/r/issues/17/timeline?per_page=100";
    const PAGE_TWO: &str = "https://api.github.com/repositories/123/issues/17/timeline?page=2";
    const TTL: Duration = Duration::from_secs(1800);

    struct Exchange {
        url: &'static str,
        conditional: Option<&'static str>,
        response: HttpResponse,
    }

    struct Transport(Mutex<VecDeque<Exchange>>);

    impl HttpTransport for Transport {
        fn get(
            &self,
            request: &GetRequest,
            url: &Url,
            _: Duration,
        ) -> std::result::Result<HttpResponse, TransportError> {
            let exchange = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected GitHub request");
            assert_eq!(url.as_str(), exchange.url);
            assert_eq!(request.etag.as_deref(), exchange.conditional);
            assert_eq!(request.bearer_token.as_deref(), Some("fixture-token"));
            assert_eq!(
                request.accept.as_deref(),
                Some("application/vnd.github+json")
            );
            Ok(exchange.response)
        }
    }

    fn exchange(
        url: &'static str,
        status: u16,
        conditional: Option<&'static str>,
        etag: Option<&'static str>,
        link: Option<String>,
        body: Value,
    ) -> Exchange {
        let mut headers = HeaderMap::new();
        if let Some(etag) = etag {
            headers.insert(ETAG, etag.parse().unwrap());
        }
        if let Some(link) = link {
            headers.insert(LINK, link.parse().unwrap());
        }
        Exchange {
            url,
            conditional,
            response: HttpResponse {
                status,
                headers,
                body: serde_json::to_vec(&body).unwrap(),
            },
        }
    }

    fn client(exchanges: impl Into<VecDeque<Exchange>>) -> Result<GitHubClient> {
        let gateway = Arc::new(Gateway::with_transport(
            20,
            HostPolicy::default(),
            Transport(Mutex::new(exchanges.into())),
        )?);
        let client = GitHubClient::new(gateway, TTL);
        client.token.set(Some("fixture-token".into())).unwrap();
        Ok(client)
    }

    fn time() -> DateTime<Utc> {
        "2026-11-24T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn github_pagination_and_conditional_cache_preserve_prs() -> Result<()> {
        let store = Store::memory()?;
        let first = "\"issue\"";
        let second = "W/\"timeline-1\"";
        let third = "\"timeline-2\"";
        let link = format!("<{PAGE_TWO}>; rel=\"next\", <{PAGE_TWO}>; rel=\"last\"");
        let cross_reference = json!({
            "event": "cross-referenced",
            "source": {"issue": {"pull_request": {"html_url": "https://github.com/o/r/pull/25"}}}
        });
        let older_pr = json!({
            "event": "connected",
            "subject": {"pull_request": {"html_url": "https://github.com/o/r/pull/24"}}
        });
        let recent_pr = json!({
            "event": "cross-referenced",
            "source": {"issue": {"pull_request": {"html_url": "https://github.com/other/r/pull/55"}}}
        });
        let client = client([
            exchange(
                ISSUE,
                200,
                None,
                Some(first),
                None,
                json!({"state": "open", "assignees": [{"login":"b"}, {"login":"a"}], "assignee":{"login":"a"}}),
            ),
            exchange(
                TIMELINE,
                200,
                None,
                Some(second),
                Some(link),
                json!([cross_reference, older_pr]),
            ),
            exchange(
                PAGE_TWO,
                200,
                None,
                Some(third),
                None,
                json!([
                    {"event":"disconnected","subject":older_pr["subject"]},
                    recent_pr, cross_reference,
                    {"event":"cross-referenced","source":{"issue":{"html_url":"https://github.com/o/r/issues/18"}}}
                ]),
            ),
            exchange(ISSUE, 304, Some(first), None, None, Value::Null),
            exchange(TIMELINE, 304, Some(second), None, None, Value::Null),
            exchange(PAGE_TWO, 304, Some(third), None, None, Value::Null),
        ])?;
        let start = time();
        let first = client.enrich(&store, "O/R", 17, Freshness::Default, start)?;
        let expected = GhIssueStatus {
            state: "open".into(),
            assignees: vec!["a".into(), "b".into()],
            linked_prs: vec![
                "https://github.com/o/r/pull/25".into(),
                "https://github.com/other/r/pull/55".into(),
            ],
        };
        assert_eq!(first.status.as_ref(), Some(&expected));
        assert_eq!(first.as_of, Some(start));
        assert!(!first.partial);
        assert!(first.warnings.is_empty());

        let ten_minutes = start + TimeDelta::minutes(10);
        let cached = client.enrich(&store, "o/r", 17, Freshness::Default, ten_minutes)?;
        assert_eq!(cached.status, first.status);
        assert_eq!(cached.as_of, first.as_of);
        assert_eq!(client.gateway.metrics().requests, 3);

        let forced = client.enrich(&store, "o/r", 17, Freshness::Fresh, ten_minutes)?;
        assert_eq!(forced.status, first.status);
        assert_eq!(forced.as_of, Some(ten_minutes));
        assert_eq!(client.gateway.metrics().not_modified, 3);

        let older = start + TimeDelta::hours(1);
        let offline = client.enrich(&store, "o/r", 17, Freshness::Offline, older)?;
        assert_eq!(offline.status, first.status);
        assert_eq!(offline.as_of, forced.as_of);
        assert!(!offline.partial);
        assert!(offline.warnings[0].contains("stale GitHub cache"));
        assert_eq!(client.gateway.metrics().requests, 6);
        Ok(())
    }

    #[test]
    fn github_does_not_follow_cross_origin_pagination() -> Result<()> {
        let store = Store::memory()?;
        let client = client([
            exchange(ISSUE, 200, None, None, None, json!({"state":"open"})),
            exchange(
                TIMELINE,
                200,
                None,
                None,
                Some("<https://example.com/untrusted/timeline>; rel=\"next\"".into()),
                json!([]),
            ),
        ])?;
        let result = client.enrich(&store, "o/r", 17, Freshness::Default, time())?;
        assert!(result.partial);
        assert!(result.status.is_none());
        assert_eq!(
            result.warnings,
            ["GitHub supplied an invalid timeline pagination link"]
        );
        assert_eq!(client.gateway.metrics().requests, 2);
        Ok(())
    }

    #[test]
    fn github_caches_confirmed_missing_issue_without_timeline_request() -> Result<()> {
        let store = Store::memory()?;
        let client = client([exchange(ISSUE, 404, None, None, None, Value::Null)])?;
        let start = time();
        let result = client.enrich(&store, "o/r", 17, Freshness::Default, start)?;
        assert!(result.partial);
        assert!(result.status.is_none());
        assert!(result.warnings[0].contains("o/r#17 is unavailable"));
        let cached = client.enrich(
            &store,
            "o/r",
            17,
            Freshness::Default,
            start + TimeDelta::seconds(1),
        )?;
        assert_eq!(cached.warnings, result.warnings);
        assert_eq!(cached.as_of, result.as_of);
        assert_eq!(client.gateway.metrics().requests, 1);
        Ok(())
    }
}
