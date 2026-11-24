//! Typed projections of Wave API records.
//!
//! Use `Issue::try_from(&record)` and the corresponding conversions for
//! programs, waves, and repositories to read a source or cache record as a
//! typed model. Unrecognized properties survive a model round trip in `extra`.
//! Missing and null values of modeled optional fields both become `None` and
//! are omitted on serialization. The source and cache retain the original JSON.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

/// An issue tracked by Drips Wave. Its `id` is distinct from its GitHub ID and issue number.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub id: String,
    #[serde(rename = "gitHubIssueId", skip_serializing_if = "Option::is_none")]
    pub github_issue_id: Option<u64>,
    #[serde(rename = "gitHubIssueNumber", skip_serializing_if = "Option::is_none")]
    pub github_issue_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<IssueState>,
    #[serde(rename = "gitHubAuthorId", skip_serializing_if = "Option::is_none")]
    pub github_author_id: Option<u64>,
    #[serde(rename = "gitHubAuthorLogin", skip_serializing_if = "Option::is_none")]
    pub github_author_login: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<Value>>,
    #[serde(rename = "gitHubCreatedAt", skip_serializing_if = "Option::is_none")]
    pub github_created_at: Option<String>,
    #[serde(rename = "gitHubUpdatedAt", skip_serializing_if = "Option::is_none")]
    pub github_updated_at: Option<String>,
    #[serde(rename = "gitHubClosedAt", skip_serializing_if = "Option::is_none")]
    pub github_closed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<Repo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wave_program_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_applications_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_applicant: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complexity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points_multiplier: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points_earned: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_from_pending_application_limit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_in_wave: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// State from the Wave issue record.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueState {
    Open,
    Closed,
    #[serde(untagged)]
    Unknown(String),
}

/// A GitHub repository tracked by Drips Wave.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Repo {
    pub id: String,
    #[serde(rename = "gitHubRepoName", skip_serializing_if = "Option::is_none")]
    pub github_repo_name: Option<String>,
    #[serde(rename = "gitHubRepoFullName", skip_serializing_if = "Option::is_none")]
    pub github_repo_full_name: Option<String>,
    #[serde(rename = "gitHubRepoUrl", skip_serializing_if = "Option::is_none")]
    pub github_repo_url: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A Wave program resolved by slug.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Program {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A numbered Wave within a program.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Wave {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wave_program_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wave_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    /// Decimal USD value as sent by the API.
    #[serde(rename = "budgetUSD", skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<WaveStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Status from the Wave record.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WaveStatus {
    Active,
    Completed,
    #[serde(untagged)]
    Unknown(String),
}

impl TryFrom<&Value> for Issue {
    type Error = serde_json::Error;

    fn try_from(record: &Value) -> Result<Self, Self::Error> {
        Self::deserialize(record)
    }
}

impl TryFrom<&Value> for Repo {
    type Error = serde_json::Error;

    fn try_from(record: &Value) -> Result<Self, Self::Error> {
        Self::deserialize(record)
    }
}

impl TryFrom<&Value> for Program {
    type Error = serde_json::Error;

    fn try_from(record: &Value) -> Result<Self, Self::Error> {
        Self::deserialize(record)
    }
}

impl TryFrom<&Value> for Wave {
    type Error = serde_json::Error;

    fn try_from(record: &Value) -> Result<Self, Self::Error> {
        Self::deserialize(record)
    }
}
