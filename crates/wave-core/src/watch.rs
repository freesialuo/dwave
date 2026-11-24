use std::time::Duration;

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::filter::is_present;
use crate::github::{GhIssueStatus, GitHubClient};
use crate::source::{BudgetExhausted, WaveSource};
use crate::store::{Freshness, Store};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WatchEntry {
    pub issue_id: String,
    pub note: Option<String>,
    pub added_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub present: bool,
    pub wave_as_of: DateTime<Utc>,
    pub pending_applications_count: Option<u64>,
    pub assigned_applicant: Option<Value>,
    pub assignees: Vec<Value>,
    pub pr_link: Option<String>,
    pub state: Option<String>,
    pub points: Option<u64>,
    pub github: Option<GhIssueStatus>,
    pub github_as_of: Option<DateTime<Utc>>,
}

impl Snapshot {
    fn from_issue(issue: Option<&Value>, wave_as_of: DateTime<Utc>) -> Self {
        let field = |key: &str| issue.and_then(|issue| issue.get(key));
        let text = |key: &str| field(key).and_then(Value::as_str).map(str::to_owned);
        let mut assignees = field("assignees")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assignees.sort_by_cached_key(Value::to_string);
        assignees.dedup();
        Self {
            present: issue.is_some(),
            wave_as_of,
            pending_applications_count: field("pendingApplicationsCount").and_then(Value::as_u64),
            assigned_applicant: field("assignedApplicant")
                .filter(|value| is_present(value))
                .cloned(),
            assignees,
            pr_link: text("prLink").filter(|link| !link.is_empty()),
            state: text("state"),
            points: field("points").and_then(Value::as_u64),
            github: None,
            github_as_of: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChangeEvent {
    pub issue_id: String,
    pub taken_at: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: ChangeKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChangeKind {
    ApplicationsChanged {
        from: Option<u64>,
        to: Option<u64>,
    },
    Assigned {
        who: Value,
    },
    Unassigned,
    AssigneesChanged {
        from: Vec<Value>,
        to: Vec<Value>,
    },
    PrLinked {
        url: String,
    },
    PrUnlinked {
        url: String,
    },
    StateChanged {
        from: Option<String>,
        to: Option<String>,
    },
    PointsChanged {
        from: Option<u64>,
        to: Option<u64>,
    },
    GitHubChanged {
        from: GhIssueStatus,
        to: GhIssueStatus,
    },
    Disappeared,
    Reappeared,
}

#[derive(Debug, Default, Serialize)]
pub struct WatchCheck {
    pub changes: Vec<ChangeEvent>,
    pub warnings: Vec<String>,
    pub partial: bool,
    pub as_of: Option<DateTime<Utc>>,
}

impl WatchCheck {
    fn warn(&mut self, warning: String) {
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }

    fn observe(&mut self, as_of: DateTime<Utc>) {
        self.as_of = Some(self.as_of.map_or(as_of, |time| time.min(as_of)));
    }
}

#[derive(Debug, Serialize)]
pub struct WatchLogEntry {
    pub issue_id: String,
    pub taken_at: DateTime<Utc>,
    pub snapshot: Snapshot,
    pub changes: Vec<ChangeEvent>,
}

pub struct Watcher<'a> {
    store: &'a mut Store,
    source: &'a dyn WaveSource,
    now: DateTime<Utc>,
    issue_ttl: Duration,
    freshness: Freshness,
}

impl<'a> Watcher<'a> {
    pub fn new(
        store: &'a mut Store,
        source: &'a dyn WaveSource,
        now: DateTime<Utc>,
        issue_ttl: Duration,
        freshness: Freshness,
    ) -> Self {
        Self {
            store,
            source,
            now,
            issue_ttl,
            freshness,
        }
    }

    pub fn check(&mut self, github: Option<&GitHubClient>) -> Result<WatchCheck> {
        let mut result = WatchCheck::default();
        for entry in self.store.watchlist()? {
            let id = &entry.issue_id;
            let cached = self.store.cached_issue_status(id)?;
            let as_of = cached.as_ref().map(|issue| issue.fetched_at);
            let (issue, as_of) = if self.freshness.allows_fetch(as_of, self.issue_ttl, self.now) {
                match self.source.issue(id) {
                    Ok(issue) => {
                        match &issue {
                            Some(issue) => {
                                ensure!(
                                    issue["id"].as_str() == Some(id),
                                    "Wave returned a different issue for {id}"
                                );
                                self.store.upsert_issue(issue, self.now)?;
                            }
                            None => self.store.set_issue_missing(id, self.now)?,
                        }
                        (issue, self.now)
                    }
                    Err(error) => {
                        result.partial = true;
                        if error.downcast_ref::<BudgetExhausted>().is_some() {
                            result.warn("watch request budget exhausted".to_string());
                        } else {
                            result.warn(format!("unable to refresh watched issue {id}: {error}"));
                        }
                        continue;
                    }
                }
            } else {
                let Some(cached) = cached else {
                    result.partial = true;
                    result.warn(format!("no cache for watched issue {id} in offline mode"));
                    continue;
                };
                if Freshness::Default.allows_fetch(as_of, self.issue_ttl, self.now) {
                    result.warn(format!(
                        "using stale Wave cache for {id} from {}",
                        cached.fetched_at
                    ));
                }
                (cached.data, cached.fetched_at)
            };
            result.observe(as_of);
            let mut snapshot = Snapshot::from_issue(issue.as_ref(), as_of);
            if let (Some(github), Some(issue)) = (github, issue.as_ref()) {
                match github.enrich_issue(self.store, issue, self.freshness, self.now) {
                    Ok(enriched) => {
                        if !enriched.partial {
                            snapshot.github = enriched.status;
                            snapshot.github_as_of = enriched.as_of;
                            if let Some(as_of) = enriched.as_of {
                                result.observe(as_of);
                            }
                        }
                        result.partial |= enriched.partial;
                        for warning in enriched.warnings {
                            result.warn(warning);
                        }
                    }
                    Err(error) => {
                        result.partial = true;
                        result.warn(format!(
                            "GitHub enrichment failed for watched issue {id}: {error}"
                        ));
                    }
                }
            }
            result
                .changes
                .extend(self.store.record_watch_snapshot(id, self.now, snapshot)?);
        }
        Ok(result)
    }
}

impl Store {
    pub fn watch_add(&self, id: &str, note: Option<&str>, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO watchlist(issue_id,note,added_at) VALUES(?1,?2,?3)
             ON CONFLICT(issue_id) DO UPDATE SET note=COALESCE(excluded.note,watchlist.note)",
            params![id, note, db_time(now)],
        )?;
        Ok(())
    }

    pub fn watch_remove(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM watchlist WHERE issue_id=?1", [id])?
            > 0)
    }

    pub fn watchlist(&self) -> Result<Vec<WatchEntry>> {
        let mut query = self
            .conn
            .prepare("SELECT issue_id,note,added_at FROM watchlist ORDER BY added_at,issue_id")?;
        let rows = query.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, String>(2)?))
        })?;
        rows.map(|row| {
            let (issue_id, note, added_at) = row?;
            Ok(WatchEntry {
                issue_id,
                note,
                added_at: added_at.parse().context("invalid watchlist timestamp")?,
            })
        })
        .collect()
    }

    pub fn watch_log(
        &self,
        id: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<WatchLogEntry>> {
        let mut query = self.conn.prepare(
            "SELECT s.issue_id,s.taken_at,s.data,
                    (SELECT p.data FROM snapshots p
                     WHERE p.issue_id=s.issue_id AND p.id<s.id ORDER BY p.id DESC LIMIT 1)
             FROM snapshots s
             WHERE (?1 IS NULL OR s.issue_id=?1) AND (?2 IS NULL OR s.taken_at>=?2)
             ORDER BY s.id DESC",
        )?;
        let rows = query.query_map(params![id, since.map(db_time)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (issue_id, taken_at, current, previous) = row?;
            let taken_at = taken_at.parse().context("invalid snapshot timestamp")?;
            let snapshot = read_snapshot(&current)?;
            let previous = previous.as_deref().map(read_snapshot).transpose()?;
            let changes = events(&issue_id, taken_at, previous.as_ref(), &snapshot);
            Ok(WatchLogEntry {
                issue_id,
                taken_at,
                snapshot,
                changes,
            })
        })
        .collect()
    }

    fn record_watch_snapshot(
        &mut self,
        id: &str,
        now: DateTime<Utc>,
        mut snapshot: Snapshot,
    ) -> Result<Vec<ChangeEvent>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let still_watched: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM watchlist WHERE issue_id=?1)",
            [id],
            |row| row.get(0),
        )?;
        if !still_watched {
            return Ok(Vec::new());
        }
        let previous = tx
            .query_row(
                "SELECT taken_at,data FROM snapshots WHERE issue_id=?1 ORDER BY id DESC LIMIT 1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let previous = previous
            .map(|(taken_at, data)| {
                Ok::<_, anyhow::Error>((taken_at.parse::<DateTime<Utc>>()?, read_snapshot(&data)?))
            })
            .transpose()?;
        let previous = if let Some((taken_at, previous)) = &previous {
            if *taken_at > now || previous.wave_as_of > snapshot.wave_as_of {
                return Ok(Vec::new());
            }
            if previous.github_as_of > snapshot.github_as_of {
                snapshot.github.clone_from(&previous.github);
                snapshot.github_as_of = previous.github_as_of;
            }
            if previous == &snapshot {
                return Ok(Vec::new());
            }
            Some(previous)
        } else {
            None
        };
        let changes = events(id, now, previous, &snapshot);
        tx.execute(
            "INSERT INTO snapshots(issue_id,taken_at,data) VALUES(?1,?2,?3)",
            params![id, db_time(now), serde_json::to_string(&snapshot)?],
        )?;
        tx.commit()?;
        Ok(changes)
    }
}

fn db_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn read_snapshot(data: &str) -> Result<Snapshot> {
    serde_json::from_str(data).context("invalid watch snapshot")
}

fn events(
    id: &str,
    taken_at: DateTime<Utc>,
    previous: Option<&Snapshot>,
    current: &Snapshot,
) -> Vec<ChangeEvent> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    let mut changes = Vec::new();
    let mut push = |kind| {
        changes.push(ChangeEvent {
            issue_id: id.to_owned(),
            taken_at,
            kind,
        })
    };
    match (previous.present, current.present) {
        (true, false) => push(ChangeKind::Disappeared),
        (false, true) => push(ChangeKind::Reappeared),
        _ => {}
    }
    if !previous.present || !current.present {
        return changes;
    }
    if previous.pending_applications_count != current.pending_applications_count {
        push(ChangeKind::ApplicationsChanged {
            from: previous.pending_applications_count,
            to: current.pending_applications_count,
        });
    }
    if previous.assigned_applicant != current.assigned_applicant {
        match &current.assigned_applicant {
            Some(who) => push(ChangeKind::Assigned { who: who.clone() }),
            None => push(ChangeKind::Unassigned),
        }
    }
    if previous.assignees != current.assignees {
        push(ChangeKind::AssigneesChanged {
            from: previous.assignees.clone(),
            to: current.assignees.clone(),
        });
    }
    if previous.pr_link != current.pr_link {
        match (&previous.pr_link, &current.pr_link) {
            (_, Some(url)) => push(ChangeKind::PrLinked { url: url.clone() }),
            (Some(url), None) => push(ChangeKind::PrUnlinked { url: url.clone() }),
            (None, None) => {}
        }
    }
    if previous.state != current.state {
        push(ChangeKind::StateChanged {
            from: previous.state.clone(),
            to: current.state.clone(),
        });
    }
    if previous.points != current.points {
        push(ChangeKind::PointsChanged {
            from: previous.points,
            to: current.points,
        });
    }
    match (&previous.github, &current.github) {
        (Some(from), Some(to)) if from != to => push(ChangeKind::GitHubChanged {
            from: from.clone(),
            to: to.clone(),
        }),
        _ => {}
    }
    changes
}
