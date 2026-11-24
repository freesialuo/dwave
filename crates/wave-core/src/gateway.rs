//! Shared request budget, per-host rate limits, and HTTP retries.

use anyhow::{ensure, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, ACCEPT, ETAG, IF_NONE_MATCH, RETRY_AFTER};
use reqwest::Url;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display};
use std::io::Read;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct HostPolicy {
    pub rate_per_second: f64,
    pub burst: u32,
    pub max_attempts: u32,
    pub timeout: Duration,
    pub retry_base: Duration,
    pub max_retry_wait: Duration,
    pub honor_retry_after: bool,
}

impl Default for HostPolicy {
    fn default() -> Self {
        Self {
            rate_per_second: 2.0,
            burst: 4,
            max_attempts: 3,
            timeout: Duration::from_secs(15),
            retry_base: Duration::from_millis(500),
            max_retry_wait: Duration::from_secs(30),
            honor_retry_after: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Metrics {
    pub requests: u32,
    pub retries: u32,
    pub not_modified: u32,
    pub bytes: u64,
}

#[derive(Debug)]
pub struct BudgetExhausted;

impl Display for BudgetExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("request budget exhausted")
    }
}

impl Error for BudgetExhausted {}

#[derive(Debug)]
pub struct HttpStatus {
    pub status: u16,
    pub host: String,
    pub retry_after: Option<Duration>,
}

impl Display for HttpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} returned HTTP {}", self.host, self.status)?;
        if let Some(delay) = self.retry_after {
            write!(f, " (retry in {}s)", delay.as_secs_f64().ceil())?;
        }
        Ok(())
    }
}

impl Error for HttpStatus {}

pub struct GetRequest {
    pub url: String,
    pub etag: Option<String>,
    pub bearer_token: Option<String>,
    pub accept: Option<String>,
}

impl GetRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            etag: None,
            bearer_token: None,
            accept: None,
        }
    }
}

#[derive(Debug)]
pub struct Fetched<T> {
    /// Absent for HTTP 304 and 404.
    pub body: Option<T>,
    pub etag: Option<String>,
    pub headers: HeaderMap,
    pub status: u16,
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct TransportError {
    pub error: anyhow::Error,
    pub retryable: bool,
}

/// Implementations send one GET attempt, with no redirects or internal retries.
pub trait HttpTransport: Send + Sync {
    fn get(
        &self,
        request: &GetRequest,
        url: &Url,
        timeout: Duration,
    ) -> std::result::Result<HttpResponse, TransportError>;
}

struct ReqwestTransport(Client);

impl HttpTransport for ReqwestTransport {
    fn get(
        &self,
        request: &GetRequest,
        url: &Url,
        timeout: Duration,
    ) -> std::result::Result<HttpResponse, TransportError> {
        let mut get = self.0.get(url.clone()).timeout(timeout).header(
            ACCEPT,
            request.accept.as_deref().unwrap_or("application/json"),
        );
        if url.host_str() == Some("api.github.com") && url.scheme() == "https" {
            get = get.header("X-GitHub-Api-Version", "2026-03-10");
        }
        if let Some(etag) = &request.etag {
            get = get.header(IF_NONE_MATCH, etag);
        }
        if let Some(token) = &request.bearer_token {
            get = get.bearer_auth(token);
        }
        let response = get.send().map_err(|error| TransportError {
            retryable: error.is_timeout() || error.is_connect(),
            error: error.without_url().into(),
        })?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut body = Vec::new();
        if status.is_success() {
            response
                .take(MAX_BODY_BYTES + 1)
                .read_to_end(&mut body)
                .map_err(|error| TransportError {
                    retryable: true,
                    error: error.into(),
                })?;
            if body.len() as u64 > MAX_BODY_BYTES {
                return Err(TransportError {
                    retryable: false,
                    error: anyhow::anyhow!("response exceeds {MAX_BODY_BYTES} bytes"),
                });
            }
        }
        Ok(HttpResponse {
            status: status.as_u16(),
            headers,
            body,
        })
    }
}

#[derive(Default)]
struct State {
    metrics: Metrics,
    hosts: HashMap<String, TokenBucket>,
}

struct TokenBucket {
    tokens: f64,
    updated: Instant,
    retry_at: Option<(Instant, u16)>,
}

impl TokenBucket {
    fn new(policy: HostPolicy, now: Instant) -> Self {
        Self {
            tokens: f64::from(policy.burst),
            updated: now,
            retry_at: None,
        }
    }

    fn retry_after(&self, now: Instant) -> Option<(Duration, u16)> {
        let (time, status) = self.retry_at?;
        let delay = time.checked_duration_since(now)?;
        (!delay.is_zero()).then_some((delay, status))
    }

    fn acquire(&mut self, policy: HostPolicy, now: Instant) -> Duration {
        self.tokens = (self.tokens
            + now.duration_since(self.updated).as_secs_f64() * policy.rate_per_second)
            .min(f64::from(policy.burst));
        self.updated = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) / policy.rate_per_second)
                .max(Duration::from_nanos(1))
        }
    }
}

pub struct Gateway {
    cap: u32,
    wave_policy: HostPolicy,
    transport: Box<dyn HttpTransport>,
    state: Mutex<State>,
}

impl Gateway {
    pub fn new(max_requests: u32, wave_policy: HostPolicy) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("dwave/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .context("build HTTP client")?;
        Self::with_transport(max_requests, wave_policy, ReqwestTransport(client))
    }

    pub fn with_transport(
        max_requests: u32,
        wave_policy: HostPolicy,
        transport: impl HttpTransport + 'static,
    ) -> Result<Self> {
        ensure!(
            wave_policy.rate_per_second.is_finite()
                && wave_policy.rate_per_second > 0.0
                && Duration::try_from_secs_f64(1.0 / wave_policy.rate_per_second).is_ok(),
            "request rate must be positive and finite"
        );
        ensure!(wave_policy.burst > 0, "request burst must be positive");
        ensure!(
            wave_policy.max_attempts > 0,
            "retry policy must allow at least one attempt"
        );
        ensure!(
            !wave_policy.timeout.is_zero(),
            "request timeout must be positive"
        );
        Ok(Self {
            cap: max_requests,
            wave_policy,
            transport: Box::new(transport),
            state: Mutex::new(State::default()),
        })
    }

    pub fn remaining(&self) -> u32 {
        self.cap - self.metrics().requests
    }

    pub fn metrics(&self) -> Metrics {
        self.state.lock().expect("gateway state poisoned").metrics
    }

    pub fn get_json<T: DeserializeOwned>(&self, request: GetRequest) -> Result<Fetched<T>> {
        let url = Url::parse(&request.url).context("invalid HTTP URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none(),
            "GET requires an HTTP(S) URL without credentials"
        );
        let host = url.host_str().context("HTTP URL has no host")?;
        let policy = if host == "api.github.com" {
            HostPolicy {
                rate_per_second: 1.0,
                burst: 2,
                ..HostPolicy::default()
            }
        } else {
            self.wave_policy
        };

        for attempt in 0..policy.max_attempts {
            self.start_attempt(host, policy, attempt > 0)?;

            let (error, retry_after) = match self.transport.get(&request, &url, policy.timeout) {
                Ok(response) => {
                    self.record_response(&response);
                    match response.status {
                        200..=299 | 304 | 404 => return decode(host, response),
                        status => {
                            let (retryable, retry_after) =
                                retry_status(status, &response.headers, host, policy);
                            if let Some(delay) = retry_after {
                                self.defer_host(host, status, delay);
                            }
                            let error = HttpStatus {
                                status,
                                host: host.to_owned(),
                                retry_after,
                            };
                            if !retryable {
                                return Err(error.into());
                            }
                            (error.into(), retry_after)
                        }
                    }
                }
                Err(failure) => {
                    let error = failure.error.context(format!("GET {host}"));
                    if !failure.retryable {
                        return Err(error);
                    }
                    (error, None)
                }
            };

            if attempt + 1 == policy.max_attempts
                || retry_after.is_some_and(|delay| delay > policy.max_retry_wait)
            {
                return Err(error);
            }
            if self.remaining() == 0 {
                return Err(BudgetExhausted.into());
            }
            let ceiling = policy
                .retry_base
                .saturating_mul(2u32.saturating_pow(attempt))
                .min(policy.max_retry_wait);
            let jitter =
                Duration::try_from_secs_f64(ceiling.as_secs_f64() * rand::random_range(0.0..=1.0))
                    .unwrap_or(ceiling);
            thread::sleep(jitter);
        }
        unreachable!("a validated retry policy has at least one attempt")
    }

    fn start_attempt(&self, host: &str, policy: HostPolicy, retry: bool) -> Result<()> {
        loop {
            let delay = {
                let mut state = self.state.lock().expect("gateway state poisoned");
                if state.metrics.requests == self.cap {
                    return Err(BudgetExhausted.into());
                }
                let now = Instant::now();
                let bucket = state
                    .hosts
                    .entry(host.to_owned())
                    .or_insert_with(|| TokenBucket::new(policy, now));
                if let Some((delay, status)) = bucket.retry_after(now) {
                    if delay > policy.max_retry_wait {
                        return Err(HttpStatus {
                            status,
                            host: host.to_owned(),
                            retry_after: Some(delay),
                        }
                        .into());
                    }
                    delay
                } else {
                    let delay = bucket.acquire(policy, now);
                    if delay.is_zero() {
                        state.metrics.requests += 1;
                        state.metrics.retries += u32::from(retry);
                    }
                    delay
                }
            };
            if delay.is_zero() {
                return Ok(());
            }
            thread::sleep(delay);
        }
    }

    fn defer_host(&self, host: &str, status: u16, delay: Duration) {
        let mut state = self.state.lock().expect("gateway state poisoned");
        if let Some(bucket) = state.hosts.get_mut(host) {
            let retry_at = Instant::now().checked_add(delay).map(|time| (time, status));
            bucket.retry_at = bucket.retry_at.max(retry_at);
        }
    }

    fn record_response(&self, response: &HttpResponse) {
        let mut state = self.state.lock().expect("gateway state poisoned");
        state.metrics.bytes = state
            .metrics
            .bytes
            .saturating_add(response.body.len() as u64);
        state.metrics.not_modified += u32::from(response.status == 304);
    }
}

fn decode<T: DeserializeOwned>(host: &str, response: HttpResponse) -> Result<Fetched<T>> {
    let body = match response.status {
        304 | 404 => None,
        _ => Some(
            serde_json::from_slice(&response.body)
                .with_context(|| format!("invalid JSON response from {host}"))?,
        ),
    };
    let etag = response.headers.get(ETAG).and_then(|v| v.to_str().ok());
    Ok(Fetched {
        body,
        etag: etag.map(str::to_owned),
        headers: response.headers,
        status: response.status,
    })
}

fn retry_status(
    status: u16,
    headers: &HeaderMap,
    host: &str,
    policy: HostPolicy,
) -> (bool, Option<Duration>) {
    let github_limited = host == "api.github.com"
        && matches!(status, 403 | 429)
        && headers
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v == "0");
    let github_cooldown =
        host == "api.github.com" && status == 403 && headers.contains_key(RETRY_AFTER);
    let retryable =
        github_limited || github_cooldown || status == 429 || (500..=599).contains(&status);
    let now = SystemTime::now();
    let retry_after = if policy.honor_retry_after
        && (matches!(status, 429 | 503) || github_limited || github_cooldown)
    {
        headers
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, now))
    } else {
        None
    };
    let reset_delay = if policy.honor_retry_after && github_limited {
        headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
            .map(|reset| reset.duration_since(now).unwrap_or_default())
    } else {
        None
    };
    (retryable, retry_after.max(reset_delay))
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_refills_at_configured_rate_and_caps_bursts() {
        let policy = HostPolicy {
            rate_per_second: 2.0,
            burst: 2,
            ..HostPolicy::default()
        };
        let start = Instant::now();
        let mut bucket = TokenBucket::new(policy, start);
        assert_eq!(bucket.acquire(policy, start), Duration::ZERO);
        assert_eq!(bucket.acquire(policy, start), Duration::ZERO);
        assert_eq!(bucket.acquire(policy, start), Duration::from_millis(500));

        let half_token = start + Duration::from_millis(250);
        assert_eq!(
            bucket.acquire(policy, half_token),
            Duration::from_millis(250)
        );
        let enough_time = start + Duration::from_secs(10);
        assert_eq!(bucket.acquire(policy, enough_time), Duration::ZERO);
        assert_eq!(bucket.acquire(policy, enough_time), Duration::ZERO);
        assert_eq!(
            bucket.acquire(policy, enough_time),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn retry_after_supports_dates_and_delay_seconds() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let date = httpdate::fmt_http_date(start + Duration::from_secs(42));
        assert_eq!(
            parse_retry_after(&date, start),
            Some(Duration::from_secs(42))
        );
        assert_eq!(
            parse_retry_after("42", start),
            Some(Duration::from_secs(42))
        );
        assert_eq!(
            parse_retry_after(&date, start + Duration::from_secs(43)),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("invalid", start), None);
    }
}
