//! Read-only Wave endpoint adapter and wire records.

use crate::gateway::{Gateway, GetRequest};
use anyhow::{bail, ensure, Context, Result};
use reqwest::Url;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub use crate::gateway::BudgetExhausted;

pub const WAVE_API: &str = "https://wave-api.drips.network";
pub const PAGE_LIMIT: u32 = 100;

/// Wave's undocumented JSON is retained verbatim, including unknown fields and enum values.
pub type Issue = Value;
pub type Program = Value;
pub type Wave = Value;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteQuery {
    pub program_id: String,
    pub state: Option<String>,
    pub has_applications: Option<bool>,
    pub has_pr: Option<bool>,
    pub search: Option<String>,
    pub repo_id: Option<String>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
    pub limit: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub total: Option<u64>,
    pub next_cursor: Option<String>,
}

pub trait WaveSource {
    fn program(&self, slug: &str) -> Result<Program>;
    fn waves(&self, program_id: &str, limit: u32) -> Result<Vec<Wave>>;
    fn issues_page(&self, query: &RemoteQuery, cursor: Option<&str>) -> Result<Page<Issue>>;
    fn issue(&self, id: &str) -> Result<Option<Issue>>;

    fn remaining_requests(&self) -> Option<u64> {
        None
    }
}

pub struct HttpWaveSource {
    gateway: Arc<Gateway>,
    base_url: Url,
}

impl HttpWaveSource {
    pub fn new(gateway: Arc<Gateway>) -> Self {
        Self {
            gateway,
            base_url: Url::parse(WAVE_API).expect("valid Wave API URL"),
        }
    }

    pub fn with_base_url(gateway: Arc<Gateway>, base_url: &str) -> Result<Self> {
        let base_url = Url::parse(base_url).context("invalid Wave API base URL")?;
        ensure!(
            matches!(base_url.scheme(), "http" | "https")
                && base_url.has_host()
                && base_url.username().is_empty()
                && base_url.password().is_none()
                && base_url.query().is_none()
                && base_url.fragment().is_none(),
            "Wave API base must be an HTTP(S) URL without credentials, a query, or a fragment"
        );
        Ok(Self { gateway, base_url })
    }

    fn endpoint(&self, parts: &[&str]) -> Url {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("HTTP URL supports path segments")
            .pop_if_empty()
            .push("api")
            .extend(parts);
        url
    }

    fn get<T: DeserializeOwned>(&self, url: Url) -> Result<Option<T>> {
        let response = self.gateway.get_json(GetRequest::new(url.as_str()))?;
        match response.status {
            404 => Ok(None),
            304 => bail!("unexpected HTTP 304 from Wave API"),
            _ => Ok(response.body),
        }
    }
}

impl WaveSource for HttpWaveSource {
    fn program(&self, slug: &str) -> Result<Program> {
        validate_segment(slug)?;
        let program: Program = self
            .get(self.endpoint(&["wave-programs", slug]))?
            .with_context(|| format!("Wave program {slug} was not found"))?;
        ensure_record(&program, "program")?;
        Ok(program)
    }

    fn waves(&self, program_id: &str, limit: u32) -> Result<Vec<Wave>> {
        validate_segment(program_id)?;
        let mut url = self.endpoint(&["wave-programs", program_id, "waves"]);
        url.query_pairs_mut()
            .append_pair("limit", &limit.clamp(1, PAGE_LIMIT).to_string());
        let result: WirePage<Wave> = self.get(url)?.context("Wave listing was not found")?;
        for wave in &result.data {
            ensure_record(wave, "wave")?;
        }
        Ok(result.data)
    }

    fn issues_page(&self, query: &RemoteQuery, cursor: Option<&str>) -> Result<Page<Issue>> {
        ensure!(
            !query.program_id.trim().is_empty(),
            "Wave program ID is required"
        );
        let mut url = self.endpoint(&["issues"]);
        let has_applications = query.has_applications.map(|value| value.to_string());
        let has_pr = query.has_pr.map(|value| value.to_string());
        {
            let mut query_pairs = url.query_pairs_mut();
            query_pairs
                .append_pair("waveProgramId", &query.program_id)
                .append_pair("limit", &query.limit.clamp(1, PAGE_LIMIT).to_string());
            for (key, value) in [
                ("state", query.state.as_deref()),
                ("hasApplications", has_applications.as_deref()),
                ("hasPr", has_pr.as_deref()),
                ("search", query.search.as_deref()),
                ("repoId", query.repo_id.as_deref()),
                ("sortBy", query.sort_by.as_deref()),
                ("sortOrder", query.sort_order.as_deref()),
                ("cursor", cursor),
            ] {
                if let Some(value) = value {
                    query_pairs.append_pair(key, value);
                }
            }
        }

        let result: WirePage<Issue> = self.get(url)?.context("Wave issue listing was not found")?;
        for issue in &result.data {
            ensure_record(issue, "issue")?;
        }
        let next_cursor = result
            .pagination
            .next_cursor
            .filter(|cursor| !cursor.is_empty());
        ensure!(
            result.pagination.has_next_page != Some(true) || next_cursor.is_some(),
            "Wave pagination indicates another page but omitted its cursor"
        );
        Ok(Page {
            data: result.data,
            total: result.pagination.total,
            next_cursor,
        })
    }

    fn issue(&self, id: &str) -> Result<Option<Issue>> {
        validate_segment(id)?;
        let issue: Option<Issue> = self.get(self.endpoint(&["issues", id]))?;
        if let Some(issue) = &issue {
            let actual = ensure_record(issue, "issue")?;
            ensure!(
                actual.eq_ignore_ascii_case(id),
                "Wave returned an unexpected issue ID"
            );
        }
        Ok(issue)
    }

    fn remaining_requests(&self) -> Option<u64> {
        Some(self.gateway.remaining().into())
    }
}

#[derive(Deserialize)]
struct WirePage<T> {
    data: Vec<T>,
    #[serde(default)]
    pagination: WirePagination,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePagination {
    total: Option<u64>,
    has_next_page: Option<bool>,
    next_cursor: Option<String>,
}

fn ensure_record<'a>(record: &'a Value, kind: &str) -> Result<&'a str> {
    record
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .with_context(|| format!("Wave {kind} omitted its ID"))
}

fn validate_segment(segment: &str) -> Result<()> {
    ensure!(
        !segment.trim().is_empty() && !matches!(segment, "." | ".."),
        "invalid Wave endpoint identifier"
    );
    Ok(())
}
