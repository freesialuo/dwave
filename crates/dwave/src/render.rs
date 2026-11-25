use crate::args::Cli;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::{self, IsTerminal, Write},
};
use wave_core::{
    filter::{assigned, github_issue_number, has_pr, repo_name},
    gateway::Metrics,
};

pub enum View {
    Fields,
    Setting,
    Presets,
    Plan,
    Issues,
    Issue,
    Waves,
    WatchList,
    WatchRemoved,
    Changes,
    History,
}

#[derive(Default, Serialize)]
pub struct Meta {
    pub program: Option<String>,
    pub as_of: Option<DateTime<Utc>>,
    pub partial: bool,
    pub warnings: Vec<String>,
    #[serde(flatten)]
    pub metrics: Metrics,
}

#[derive(Serialize)]
pub struct Page {
    pub page: u32,
    pub limit: u32,
    pub total: u64,
}

pub struct Report {
    view: View,
    pub data: Value,
    pub meta: Meta,
    pub page: Option<Page>,
    pub changed: bool,
    pub failed_check: bool,
}

impl Report {
    pub fn new(view: View, data: Value, as_of: Option<DateTime<Utc>>) -> Self {
        Self {
            view,
            data,
            meta: Meta {
                as_of,
                ..Meta::default()
            },
            page: None,
            changed: false,
            failed_check: false,
        }
    }

    pub fn program(mut self, program: &str) -> Self {
        self.meta.program = Some(program.to_owned());
        self
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum EnvelopeData<'a> {
    Whole(&'a Value),
    Record([&'a Value; 1]),
}

#[derive(Serialize)]
struct Envelope<'a> {
    schema: &'static str,
    meta: &'a Meta,
    data: EnvelopeData<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<&'a Page>,
}

pub fn write(report: &Report, cli: &Cli) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    if cli.json || cli.ndjson {
        let records = report
            .data
            .as_array()
            .filter(|records| cli.ndjson && !records.is_empty());
        if let Some(records) = records {
            for record in records {
                machine(&mut stdout, report, EnvelopeData::Record([record]))?;
            }
        } else {
            machine(&mut stdout, report, EnvelopeData::Whole(&report.data))?;
        }
    } else {
        let color = stdout.is_terminal() && !cli.no_color && std::env::var_os("NO_COLOR").is_none();
        human(&mut stdout, report, color)?;
        if let Some(as_of) = report.meta.as_of {
            writeln!(stderr, "As of {}", as_of.to_rfc3339())?;
        } else {
            writeln!(stderr, "As of unknown")?;
        }
        if report.meta.partial {
            writeln!(stderr, "Partial results")?;
        }
        for warning in &report.meta.warnings {
            writeln!(stderr, "Warning: {}", clean(warning))?;
        }
    }
    if cli.verbose > 0 {
        let metrics = report.meta.metrics;
        writeln!(
            stderr,
            "Requests: {} ({} retries, {} unchanged, {} bytes)",
            metrics.requests, metrics.retries, metrics.not_modified, metrics.bytes
        )?;
    }
    stdout.flush()?;
    Ok(())
}

fn machine(output: &mut impl Write, report: &Report, data: EnvelopeData<'_>) -> anyhow::Result<()> {
    serde_json::to_writer(
        &mut *output,
        &Envelope {
            schema: "dwave/v1",
            meta: &report.meta,
            data,
            page: report.page.as_ref(),
        },
    )?;
    writeln!(output)?;
    Ok(())
}

fn human(out: &mut impl Write, report: &Report, color: bool) -> io::Result<()> {
    let data = &report.data;
    let rows = data.as_array().map(Vec::as_slice).unwrap_or_default();
    match report.view {
        View::Fields => fields(out, data),
        View::Setting => writeln!(out, "{}", cell(&data["value"])),
        View::Presets => {
            heading(
                out,
                color,
                &format!("{:<18} {:<21} FILTER", "PRESET", "COST"),
            )?;
            for row in rows {
                let mut filters = Vec::new();
                if let Some(parts) = row["filter"].as_object() {
                    for (key, value) in parts {
                        if !value.is_null()
                            && value.as_array().is_none_or(|values| !values.is_empty())
                        {
                            filters.push(format!("{key}={}", cell(value)));
                        }
                    }
                }
                if row["requires_repo"].as_bool() == Some(true) {
                    filters.push("--repo required".into());
                }
                writeln!(
                    out,
                    "{:<18} {:<21} {}",
                    cell(&row["name"]),
                    cell(&row["estimated_cost"]),
                    filters.join(", ")
                )?;
            }
            Ok(())
        }
        View::Plan => {
            let plan = data.get("plan").unwrap_or(data);
            writeln!(out, "Mode: {}", cell(&plan["mode"]))?;
            writeln!(out, "Remote query: {}", cell(&plan["remote"]))?;
            writeln!(out, "Local filters: {}", cell(&plan["local"]))?;
            writeln!(
                out,
                "Estimated requests: {}",
                plan["estimated_requests"]
                    .as_u64()
                    .map_or_else(|| "unknown".to_owned(), |estimate| estimate.to_string())
            )?;
            if let Some(total) = data["total"].as_u64() {
                writeln!(out, "Matching issues: {total}")?;
            }
            Ok(())
        }
        View::Issues => {
            heading(
                out,
                color,
                &format!(
                    "{:>7} {:>7}  {:<15} {:<3} {:<32} {:<8} {}",
                    "POINTS", "PENDING", "ASSIGNEE", "PR", "REPO#ISSUE", "ID", "TITLE"
                ),
            )?;
            for row in rows {
                writeln!(
                    out,
                    "{:>7} {:>7}  {:<15} {:<3} {:<32} {:<8} {}{}",
                    cell(&row["points"]),
                    cell(&row["pendingApplicationsCount"]),
                    fit(&assignee(row), 15),
                    pr_label(row),
                    fit(&issue_ref(row), 32),
                    fit(&cell(&row["id"]), 8),
                    fit(&cell(&row["title"]), 80),
                    github_label(row),
                )?;
            }
            if let Some(page) = &report.page {
                writeln!(
                    out,
                    "Page {} · {} of {} issue(s)",
                    page.page,
                    rows.len(),
                    page.total
                )?;
            }
            Ok(())
        }
        View::Issue => {
            heading(out, color, &cell(&data["title"]))?;
            writeln!(out, "Issue: {}", issue_ref(data))?;
            writeln!(out, "Wave ID: {}", cell(&data["id"]))?;
            writeln!(out, "State: {}", cell(&data["state"]))?;
            writeln!(out, "Points: {}", cell(&data["points"]))?;
            writeln!(
                out,
                "Pending applications: {}",
                cell(&data["pendingApplicationsCount"])
            )?;
            writeln!(out, "Assignee: {}", assignee(data))?;
            writeln!(
                out,
                "Wave PR: {}",
                data.get("prLink")
                    .and_then(Value::as_str)
                    .map_or_else(|| pr_label(data).to_owned(), clean,)
            )?;
            if let (Some(repo), Some(number)) = (repo_name(data), github_issue_number(data)) {
                writeln!(
                    out,
                    "GitHub issue: https://github.com/{}/issues/{number}",
                    clean(repo)
                )?;
            }
            if data.get("github").is_some() {
                writeln!(out, "GitHub status: {}", cell(&data["github"]))?;
                writeln!(out, "GitHub as of: {}", cell(&data["github_as_of"]))?;
            }
            Ok(())
        }
        View::Waves => {
            heading(
                out,
                color,
                &format!("{:<7} {:<14} {:<25} END", "WAVE", "STATUS", "START"),
            )?;
            for row in rows {
                writeln!(
                    out,
                    "{:<7} {:<14} {:<25} {}",
                    cell(&row["waveNumber"]),
                    cell(&row["status"]),
                    cell(&row["startDate"]),
                    cell(&row["endDate"])
                )?;
            }
            if rows.is_empty() {
                writeln!(out, "No matching waves")?;
            }
            Ok(())
        }
        View::WatchList => {
            heading(
                out,
                color,
                &format!("{:<36} {:<25} NOTE", "ISSUE ID", "ADDED"),
            )?;
            for row in rows {
                writeln!(
                    out,
                    "{:<36} {:<25} {}",
                    cell(&row["issue_id"]),
                    cell(&row["added_at"]),
                    cell(&row["note"])
                )?;
            }
            if rows.is_empty() {
                writeln!(out, "No watched issues")?;
            }
            Ok(())
        }
        View::WatchRemoved => {
            for row in rows {
                let action = if row["removed"].as_bool() == Some(true) {
                    "Removed"
                } else {
                    "Was not watched:"
                };
                writeln!(out, "{action} {}", cell(&row["issue_id"]))?;
            }
            Ok(())
        }
        View::Changes => {
            if rows.is_empty() && !report.meta.partial {
                writeln!(out, "No changes")?;
            }
            for row in rows {
                change(out, row)?;
            }
            Ok(())
        }
        View::History => {
            if rows.is_empty() {
                writeln!(out, "No snapshots")?;
            }
            for row in rows {
                writeln!(
                    out,
                    "{}  {}  {}",
                    cell(&row["taken_at"]),
                    cell(&row["issue_id"]),
                    cell(&row["snapshot"])
                )?;
                if let Some(changes) = row["changes"].as_array() {
                    for row in changes {
                        change(out, row)?;
                    }
                }
            }
            Ok(())
        }
    }
}

fn fields(out: &mut impl Write, data: &Value) -> io::Result<()> {
    if let Some(values) = data.as_object() {
        for (key, value) in values {
            writeln!(out, "{}: {}", clean(key), cell(value))?;
        }
        Ok(())
    } else {
        writeln!(out, "{}", cell(data))
    }
}

fn change(out: &mut impl Write, row: &Value) -> io::Result<()> {
    write!(
        out,
        "{}  {}  {}",
        cell(&row["taken_at"]),
        cell(&row["issue_id"]),
        cell(&row["type"]).replace('_', " ")
    )?;
    if row.get("from").is_some() || row.get("to").is_some() {
        write!(out, "  {} → {}", cell(&row["from"]), cell(&row["to"]))?;
    }
    for key in ["who", "url"] {
        if let Some(value) = row.get(key) {
            write!(out, "  {}", cell(value))?;
        }
    }
    writeln!(out)
}

fn assignee(issue: &Value) -> String {
    fn username(value: &Value) -> Option<&str> {
        value.as_str().or_else(|| {
            [
                "gitHubUsername",
                "githubUsername",
                "username",
                "login",
                "name",
            ]
            .into_iter()
            .find_map(|key| value[key].as_str())
        })
    }
    let assignees: BTreeSet<_> = std::iter::once(&issue["assignedApplicant"])
        .chain(issue["assignees"].as_array().into_iter().flatten())
        .filter_map(username)
        .map(clean)
        .filter(|name| !name.is_empty())
        .collect();
    if assignees.is_empty() {
        match assigned(issue) {
            Some(true) => "assigned".into(),
            Some(false) => "-".into(),
            None => "?".into(),
        }
    } else {
        assignees.into_iter().collect::<Vec<_>>().join(", ")
    }
}

fn pr_label(issue: &Value) -> &'static str {
    match has_pr(issue) {
        Some(true) => "yes",
        Some(false) => "no",
        None => "?",
    }
}

fn issue_ref(issue: &Value) -> String {
    match (repo_name(issue), github_issue_number(issue)) {
        (Some(repo), Some(number)) => format!("{}#{number}", clean(repo)),
        _ => "-".into(),
    }
}

fn github_label(issue: &Value) -> String {
    match issue.get("github") {
        Some(Value::Null) => " [GitHub unavailable]".into(),
        Some(status) => format!(
            " [GitHub: {}, {} assignee(s), {} PR(s)]",
            cell(&status["state"]),
            status["assignees"].as_array().map_or(0, Vec::len),
            status["linked_prs"].as_array().map_or(0, Vec::len),
        ),
        None => String::new(),
    }
}

pub fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn cell(value: &Value) -> String {
    match value {
        Value::String(text) => clean(text),
        Value::Null => "-".to_owned(),
        other => clean(&other.to_string()),
    }
}

fn fit(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max.saturating_sub(1)).collect();
    match (chars.next(), chars.next()) {
        (Some(last), None) => format!("{prefix}{last}"),
        (Some(_), Some(_)) => format!("{prefix}…"),
        _ => prefix,
    }
}

fn heading(out: &mut impl Write, color: bool, text: &str) -> io::Result<()> {
    if color {
        writeln!(out, "\x1b[1m{text}\x1b[0m")
    } else {
        writeln!(out, "{text}")
    }
}
