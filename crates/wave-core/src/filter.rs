use crate::source::RemoteQuery;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bounds {
    pub min: Option<u32>,
    pub max: Option<u32>,
}

impl Bounds {
    fn contains(self, value: u64) -> bool {
        self.min.is_none_or(|min| value >= u64::from(min))
            && self.max.is_none_or(|max| value <= u64::from(max))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Filter {
    pub state: Option<String>,
    pub pending: Option<Bounds>,
    pub never_applied: Option<bool>,
    pub assigned: Option<bool>,
    pub has_pr: Option<bool>,
    pub points: Option<Bounds>,
    pub repos: Vec<String>,
    pub labels: Vec<String>,
    pub search: Option<String>,
    /// Maximum age of an issue's update, in seconds.
    pub updated_since: Option<u64>,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            state: Some("open".into()),
            pending: None,
            never_applied: None,
            assigned: None,
            has_pr: None,
            points: None,
            repos: Vec::new(),
            labels: Vec::new(),
            search: None,
            updated_since: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Updated,
    Created,
    Points,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Passthrough,
    ScopedSync,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub mode: Mode,
    pub remote: RemoteQuery,
    pub estimated_requests: Option<u64>,
}

impl Filter {
    pub(crate) fn plan(
        &self,
        program_id: String,
        repo_id: Option<String>,
        sort: Sort,
        forced_scope: bool,
    ) -> Plan {
        let mode = if forced_scope
            || sort != Sort::Updated
            || self.pending.is_some()
            || self.assigned.is_some()
            || self.points.is_some()
            || !self.labels.is_empty()
            || self.updated_since.is_some()
            || (!self.repos.is_empty() && repo_id.is_none())
        {
            Mode::ScopedSync
        } else {
            Mode::Passthrough
        };
        Plan {
            mode,
            remote: RemoteQuery {
                program_id,
                state: self.constrained_state().map(str::to_ascii_lowercase),
                has_applications: self.never_applied.map(|never| !never),
                has_pr: self.has_pr,
                search: self.search.clone(),
                repo_id,
                sort_by: Some("updatedAt".into()),
                sort_order: Some("desc".into()),
                limit: 100,
            },
            estimated_requests: None,
        }
    }

    pub(crate) fn normalized(&self) -> Self {
        let mut filter = self.clone();
        filter.state = self.constrained_state().map(str::to_ascii_lowercase);
        filter.repos = self
            .repos
            .iter()
            .map(|repo| repo.to_ascii_lowercase())
            .collect();
        filter.labels = self
            .labels
            .iter()
            .map(|label| label.to_lowercase())
            .collect();
        filter.repos.sort_unstable();
        filter.repos.dedup();
        filter.labels.sort_unstable();
        filter.labels.dedup();
        filter
    }

    pub(crate) fn matches(&self, issue: &Value, now: DateTime<Utc>) -> bool {
        let state = issue["state"].as_str();
        let pending = issue["pendingApplicationsCount"].as_u64();
        let points = issue["points"].as_u64();
        let label_rows = issue["labels"].as_array();
        self.constrained_state()
            .is_none_or(|value| state.is_some_and(|state| state.eq_ignore_ascii_case(value)))
            && self
                .pending
                .is_none_or(|range| pending.is_some_and(|value| range.contains(value)))
            && self
                .points
                .is_none_or(|range| points.is_some_and(|value| range.contains(value)))
            && self
                .assigned
                .is_none_or(|expected| assigned(issue) == Some(expected))
            && self
                .has_pr
                .is_none_or(|expected| has_pr(issue) == Some(expected))
            && self.never_applied.is_none_or(|never| {
                // A scope filtered by hasApplications supplies evidence when the
                // payload has no explicit application history.
                has_applications(issue).is_none_or(|actual| actual != never)
            })
            && (self.repos.is_empty()
                || repo_name(issue).is_some_and(|name| {
                    self.repos
                        .iter()
                        .any(|repo| repo.eq_ignore_ascii_case(name))
                }))
            && self.labels.iter().all(|wanted| {
                label_rows.is_some_and(|labels| {
                    labels.iter().any(|label| {
                        label
                            .as_str()
                            .or_else(|| label["name"].as_str())
                            .is_some_and(|name| name.to_lowercase() == wanted.to_lowercase())
                    })
                })
            })
            && self.search.as_ref().is_none_or(|wanted| {
                issue["title"]
                    .as_str()
                    .is_some_and(|title| title.to_lowercase().contains(&wanted.to_lowercase()))
            })
            && self.updated_since.is_none_or(|seconds| {
                let cutoff = i64::try_from(seconds)
                    .ok()
                    .and_then(Duration::try_seconds)
                    .and_then(|age| now.checked_sub_signed(age));
                issue_time(issue, "updatedAt")
                    .is_some_and(|updated| cutoff.is_none_or(|cutoff| updated >= cutoff))
            })
    }

    fn constrained_state(&self) -> Option<&str> {
        self.state
            .as_deref()
            .filter(|state| !state.eq_ignore_ascii_case("all"))
    }
}

impl Sort {
    pub(crate) fn compare(self, left: &Value, right: &Value) -> Ordering {
        let result = match self {
            Self::Updated => issue_time(right, "updatedAt").cmp(&issue_time(left, "updatedAt")),
            Self::Created => issue_time(right, "createdAt").cmp(&issue_time(left, "createdAt")),
            Self::Points => right["points"].as_u64().cmp(&left["points"].as_u64()),
        };
        result.then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    }
}

pub fn repo_name(issue: &Value) -> Option<&str> {
    issue["repo"]["gitHubRepoFullName"].as_str()
}

pub fn github_issue_number(issue: &Value) -> Option<u64> {
    issue["gitHubIssueNumber"].as_u64()
}

pub(crate) fn issue_time(issue: &Value, field: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(issue[field].as_str()?)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

pub fn assigned(issue: &Value) -> Option<bool> {
    let applicant = issue.get("assignedApplicant").map(is_present);
    let assignees = issue.get("assignees").map(is_present);
    match (applicant, assignees) {
        (None, None) => None,
        _ => Some(applicant.unwrap_or_default() || assignees.unwrap_or_default()),
    }
}

pub fn has_pr(issue: &Value) -> Option<bool> {
    issue
        .get("hasPr")
        .and_then(Value::as_bool)
        .or_else(|| issue.get("prLink").map(is_present))
}

fn has_applications(issue: &Value) -> Option<bool> {
    issue["hasApplications"]
        .as_bool()
        .or_else(|| issue["applicationsCount"].as_u64().map(|number| number > 0))
        .or_else(|| {
            issue["applications"]
                .as_array()
                .map(|applications| !applications.is_empty())
        })
        .or_else(|| (issue["pendingApplicationsCount"].as_u64()? > 0).then_some(true))
        .or_else(|| {
            issue
                .get("assignedApplicant")
                .filter(|value| is_present(value))
                .map(|_| true)
        })
}

pub(crate) fn is_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        Value::Number(_) => true,
    }
}
