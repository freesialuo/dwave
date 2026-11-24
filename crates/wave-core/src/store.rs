use crate::filter::{assigned, github_issue_number, has_pr, issue_time, repo_name};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const SCHEMA: &str = "
CREATE TABLE programs (
    slug TEXT PRIMARY KEY NOT NULL,
    raw_json TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);
CREATE TABLE waves (
    program_id TEXT PRIMARY KEY NOT NULL,
    raw_json TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);
CREATE TABLE repos (
    program_id TEXT NOT NULL,
    name TEXT NOT NULL COLLATE NOCASE,
    id TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (program_id, name)
);
CREATE TABLE issues (
    id TEXT PRIMARY KEY NOT NULL,
    program_id TEXT,
    repo_name TEXT COLLATE NOCASE,
    issue_number INTEGER,
    state TEXT,
    pending INTEGER,
    assigned INTEGER,
    has_pr INTEGER,
    points INTEGER,
    updated_at INTEGER,
    created_at INTEGER,
    raw_json TEXT,
    fetched_at INTEGER NOT NULL
);
CREATE INDEX issues_github ON issues(repo_name, issue_number, fetched_at);
CREATE INDEX issues_query ON issues(program_id, state, has_pr, pending, assigned, points);
CREATE INDEX issues_updated ON issues(program_id, updated_at);
CREATE INDEX issues_created ON issues(program_id, created_at);
CREATE INDEX issues_fetched ON issues(fetched_at);
CREATE TABLE sync_state (
    scope TEXT PRIMARY KEY NOT NULL,
    state_json TEXT NOT NULL,
    touched_at INTEGER NOT NULL
);
CREATE TABLE scope_membership (
    scope TEXT NOT NULL REFERENCES sync_state(scope) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    stale INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (scope, generation, issue_id)
);
CREATE INDEX membership_issue ON scope_membership(issue_id, stale);
CREATE INDEX membership_order ON scope_membership(scope, generation, stale, position);
CREATE TABLE discoveries (
    name TEXT PRIMARY KEY NOT NULL,
    observed_at INTEGER NOT NULL
);
CREATE TABLE discovery_matches (
    name TEXT NOT NULL REFERENCES discoveries(name) ON DELETE CASCADE,
    issue_id TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    PRIMARY KEY (name, issue_id)
);
CREATE TABLE watchlist (
    issue_id TEXT PRIMARY KEY NOT NULL,
    note TEXT,
    added_at TEXT NOT NULL
);
CREATE TABLE snapshots (
    id INTEGER PRIMARY KEY,
    issue_id TEXT NOT NULL,
    taken_at TEXT NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX snapshots_issue ON snapshots(issue_id, id);
CREATE TABLE github_cache (
    url TEXT PRIMARY KEY NOT NULL,
    etag TEXT,
    body TEXT NOT NULL,
    fetched_at TEXT NOT NULL,
    next TEXT
);";

const DISCOVERY_RESULTS: &str = "
CREATE TABLE discovery_results (
    name TEXT NOT NULL REFERENCES discoveries(name) ON DELETE CASCADE,
    issue_id TEXT NOT NULL,
    position INTEGER,
    delivered INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (name, issue_id)
);
CREATE INDEX discovery_results_order ON discovery_results(name, position);";

const MIGRATIONS: &[&str] = &[SCHEMA, DISCOVERY_RESULTS];

pub struct Store {
    pub(crate) conn: Connection,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Cached<T> {
    pub data: T,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ScopeState {
    pub generation: i64,
    pub as_of: Option<DateTime<Utc>>,
    pub full_at: Option<DateTime<Utc>>,
    pub high_water: Option<DateTime<Utc>>,
    pub estimated_requests: Option<u64>,
    pub total: Option<u64>,
    pub run: Option<Scan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ScanKind {
    Pass,
    Full,
    Incremental,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Scan {
    pub kind: ScanKind,
    pub generation: i64,
    pub cursor: Option<String>,
    pub position: i64,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub seen_cursors: BTreeSet<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub high_water: Option<DateTime<Utc>>,
}

impl Scan {
    pub fn new(kind: ScanKind, state: &ScopeState) -> Self {
        let generation = match (&state.run, kind) {
            (_, ScanKind::Incremental) => state.generation,
            (Some(previous), _) => state.generation.max(previous.generation) + 1,
            (None, _) => state.generation + 1,
        };
        Self {
            generation,
            kind,
            cursor: None,
            position: 0,
            paused: false,
            seen_cursors: BTreeSet::new(),
            fetched_at: None,
            high_water: None,
        }
    }
}

impl ScopeState {
    pub fn visible_generation(&self) -> i64 {
        self.run
            .as_ref()
            .filter(|run| run.fetched_at.is_some())
            .map_or(self.generation, |run| run.generation)
    }

    pub fn visible_as_of(&self) -> Option<DateTime<Utc>> {
        self.run
            .as_ref()
            .and_then(|run| run.fetched_at)
            .or(self.as_of)
    }
}

#[derive(Debug, Serialize)]
pub struct CacheStats {
    pub programs: u64,
    pub waves: u64,
    pub repos: u64,
    pub issues: u64,
    pub sync_scopes: u64,
    pub scope_memberships: u64,
    pub stale_memberships: u64,
    pub watchlist: u64,
    pub snapshots: u64,
    pub github_entries: u64,
    pub discoveries: u64,
}

#[derive(Debug, Serialize)]
pub struct PruneStats {
    pub programs: usize,
    pub waves: usize,
    pub repos: usize,
    pub issues: usize,
    pub sync_scopes: usize,
    pub stale_memberships: usize,
    pub github_entries: usize,
    pub discoveries: usize,
}

pub(crate) struct ScopeLock {
    _file: Option<File>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path == Path::new(":memory:") {
            return Self::memory();
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create cache directory {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open cache {}", path.display()))?;
        let path = path.canonicalize().context("resolve cache database path")?;
        Self::initialize(conn, Some(path))
    }

    pub fn memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?, None)
    }

    fn initialize(mut conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(3))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        if path.is_some() {
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let version = version as usize;
        if version > MIGRATIONS.len() {
            bail!(
                "cache schema version {version} is newer than supported version {}",
                MIGRATIONS.len()
            );
        }
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
            tx.execute_batch(migration)
                .with_context(|| format!("migrate cache to schema version {}", index + 1))?;
            tx.pragma_update(None, "user_version", index + 1)?;
        }
        tx.commit()?;
        Ok(Self { conn, path })
    }

    pub(crate) fn lock_scope(&self, scope: &str, max_wait: Duration) -> Result<Option<ScopeLock>> {
        let Some(path) = &self.path else {
            return Ok(Some(ScopeLock { _file: None }));
        };
        // A stable hash makes the lock name consistent across processes and
        // avoids embedding search strings or path separators in filenames.
        let hash = scope.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".scope-{hash:016x}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&name)
            .with_context(|| format!("open scope lock {}", Path::new(&name).display()))?;
        let start = Instant::now();
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Some(ScopeLock { _file: Some(file) })),
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    if start.elapsed() >= max_wait {
                        return Ok(None);
                    }
                    thread::sleep(
                        Duration::from_millis(25).min(max_wait.saturating_sub(start.elapsed())),
                    );
                }
                Err(error) => return Err(error).context("acquire cache scope lock"),
            }
        }
    }

    pub(crate) fn cached_program(&self, slug: &str) -> Result<Option<Cached<Value>>> {
        self.cached_metadata(
            "SELECT raw_json, fetched_at FROM programs WHERE slug = ?1",
            slug,
        )
    }

    pub(crate) fn save_program(&self, slug: &str, data: &Value, now: DateTime<Utc>) -> Result<()> {
        self.save_metadata(
            "INSERT INTO programs VALUES (?1, ?2, ?3)
             ON CONFLICT(slug) DO UPDATE SET raw_json=excluded.raw_json, fetched_at=excluded.fetched_at
             WHERE programs.fetched_at <= excluded.fetched_at",
            slug, data, now,
        )
    }

    pub(crate) fn cached_waves(&self, program_id: &str) -> Result<Option<Cached<Vec<Value>>>> {
        self.cached_metadata(
            "SELECT raw_json, fetched_at FROM waves WHERE program_id = ?1",
            program_id,
        )
    }

    pub(crate) fn save_waves(
        &self,
        program_id: &str,
        data: &[Value],
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.save_metadata(
            "INSERT INTO waves VALUES (?1, ?2, ?3)
             ON CONFLICT(program_id) DO UPDATE SET raw_json=excluded.raw_json, fetched_at=excluded.fetched_at
             WHERE waves.fetched_at <= excluded.fetched_at",
            program_id, data, now,
        )
    }

    fn cached_metadata<T: DeserializeOwned>(
        &self,
        sql: &str,
        key: &str,
    ) -> Result<Option<Cached<T>>> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(sql, [key], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        row.map(|(raw, fetched_at)| read_cached(&raw, fetched_at))
            .transpose()
    }

    fn save_metadata<T: Serialize + ?Sized>(
        &self,
        sql: &str,
        key: &str,
        data: &T,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute(
            sql,
            params![key, serde_json::to_string(data)?, now.timestamp_millis()],
        )?;
        Ok(())
    }

    pub fn cached_issue_status(&self, id: &str) -> Result<Option<Cached<Option<Value>>>> {
        let row: Option<(Option<String>, i64)> = self
            .conn
            .query_row(
                "SELECT raw_json, fetched_at FROM issues WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(raw, value)| {
            Ok(Cached {
                data: raw.as_deref().map(serde_json::from_str).transpose()?,
                fetched_at: timestamp(value)?,
            })
        })
        .transpose()
    }

    pub fn cached_issue(&self, id: &str) -> Result<Option<Cached<Value>>> {
        Ok(self.cached_issue_status(id)?.and_then(|cached| {
            cached.data.map(|issue| Cached {
                data: issue,
                fetched_at: cached.fetched_at,
            })
        }))
    }

    pub fn cached_issue_by_github(&self, repo: &str, number: u64) -> Result<Option<Cached<Value>>> {
        let number = i64::try_from(number).context("GitHub issue number is too large")?;
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT raw_json, fetched_at FROM issues
                 WHERE repo_name = ?1 AND issue_number = ?2 AND raw_json IS NOT NULL
                 ORDER BY fetched_at DESC, id ASC LIMIT 1",
                params![repo, number],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(raw, value)| read_cached(&raw, value)).transpose()
    }

    pub fn upsert_issue(&mut self, issue: &Value, fetched_at: DateTime<Utc>) -> Result<()> {
        let tx = self.conn.transaction()?;
        write_issue(&tx, issue, None, fetched_at)?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_issue_missing(&mut self, id: &str, fetched_at: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO issues (id, fetched_at) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET raw_json=NULL, fetched_at=excluded.fetched_at
             WHERE issues.fetched_at <= excluded.fetched_at",
            params![id, fetched_at.timestamp_millis()],
        )?;
        Ok(())
    }

    pub(crate) fn repo_id(&self, program_id: &str, name: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM repos WHERE program_id = ?1 AND name = ?2",
                params![program_id, name],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn scope_state(&self, scope: &str) -> Result<Option<ScopeState>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT state_json FROM sync_state WHERE scope = ?1",
                [scope],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.as_deref().map(serde_json::from_str).transpose()?)
    }

    pub(crate) fn save_scope_state(
        &self,
        scope: &str,
        state: &ScopeState,
        now: DateTime<Utc>,
    ) -> Result<()> {
        write_scope_state(&self.conn, scope, state, now)
    }

    // A page, the cursor following it, and any generation rollover commit together.
    pub(crate) fn commit_page(
        &mut self,
        scope: &str,
        program_id: &str,
        issues: &[Value],
        generation: i64,
        position: i64,
        state: &ScopeState,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_scope_state(&tx, scope, state, now)?;
        for (offset, issue) in issues.iter().enumerate() {
            let id = write_issue(&tx, issue, Some(program_id), now)?;
            tx.execute(
                "INSERT INTO scope_membership(scope, generation, issue_id, position, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(scope, generation, issue_id) DO UPDATE SET last_seen=excluded.last_seen",
                params![scope, generation, id, position + offset as i64, now.timestamp_millis()],
            )?;
        }
        // "Generation" holds the last completed replacement. Leave its membership
        // intact during a partial scan, and mark departures only on completion.
        if state.run.is_none() && generation == state.generation {
            tx.execute(
                "DELETE FROM scope_membership AS previous
                 WHERE previous.scope = ?1 AND previous.generation <> ?2
                   AND EXISTS (SELECT 1 FROM scope_membership AS current
                               WHERE current.scope = ?1 AND current.generation = ?2
                                 AND current.issue_id = previous.issue_id)",
                params![scope, generation],
            )?;
            tx.execute(
                "UPDATE scope_membership SET stale = 1 WHERE scope = ?1 AND generation <> ?2",
                params![scope, generation],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn scope_issues(&self, scope: &str, generation: i64) -> Result<Vec<Value>> {
        let mut statement = self.conn.prepare(
            "SELECT issues.raw_json FROM scope_membership AS member
             JOIN issues ON issues.id = member.issue_id
             WHERE member.scope = ?1 AND member.generation = ?2 AND member.stale = 0
               AND issues.raw_json IS NOT NULL
             ORDER BY member.position, member.issue_id",
        )?;
        let rows =
            statement.query_map(params![scope, generation], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub(crate) fn discover(
        &mut self,
        name: &str,
        ordered_matches: &[&str],
        offset: usize,
        limit: u32,
        now: DateTime<Utc>,
    ) -> Result<(HashSet<String>, u64)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existed = tx
            .query_row("SELECT 1 FROM discoveries WHERE name = ?1", [name], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        tx.execute(
            "INSERT INTO discoveries VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET observed_at=excluded.observed_at",
            params![name, now.timestamp_millis()],
        )?;
        let previous = {
            let mut statement =
                tx.prepare("SELECT issue_id FROM discovery_matches WHERE name = ?1")?;
            let rows = statement.query_map([name], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<HashSet<_>>>()?
        };
        let matches: HashSet<_> = ordered_matches.iter().copied().collect();
        for id in ordered_matches.iter().filter(|id| !previous.contains(**id)) {
            tx.execute(
                "INSERT INTO discovery_matches VALUES (?1, ?2, ?3)",
                params![name, id, now.timestamp_millis()],
            )?;
            if existed {
                tx.execute(
                    "INSERT INTO discovery_results (name, issue_id) VALUES (?1, ?2)
                     ON CONFLICT(name, issue_id) DO UPDATE SET position=NULL, delivered=0",
                    params![name, id],
                )?;
            }
        }
        for id in previous.iter().filter(|id| !matches.contains(id.as_str())) {
            tx.execute(
                "DELETE FROM discovery_matches WHERE name = ?1 AND issue_id = ?2",
                params![name, id],
            )?;
            tx.execute(
                "UPDATE discovery_results SET delivered = 1 WHERE name = ?1 AND issue_id = ?2",
                params![name, id],
            )?;
        }

        // Page one begins a new report over undelivered matches. Keep the
        // positions of delivered entries while users read subsequent pages.
        if offset == 0 {
            tx.execute(
                "DELETE FROM discovery_results WHERE name = ?1 AND delivered = 1",
                [name],
            )?;
            tx.execute(
                "UPDATE discovery_results SET position = NULL WHERE name = ?1",
                [name],
            )?;
        }
        let mut position: i64 = tx.query_row(
            "SELECT coalesce(max(position) + 1, 0) FROM discovery_results WHERE name = ?1",
            [name],
            |row| row.get(0),
        )?;
        let queued = {
            let mut statement = tx.prepare(
                "SELECT issue_id FROM discovery_results
                 WHERE name = ?1 AND position IS NULL AND delivered = 0",
            )?;
            let rows = statement.query_map([name], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<HashSet<_>>>()?
        };
        for id in ordered_matches.iter().filter(|id| queued.contains(**id)) {
            tx.execute(
                "UPDATE discovery_results SET position = ?3 WHERE name = ?1 AND issue_id = ?2",
                params![name, id, position],
            )?;
            position += 1;
        }

        let start = i64::try_from(offset).unwrap_or(i64::MAX);
        let end = start.saturating_add(i64::from(limit));
        let range = params![name, start, end];
        let selected = {
            let mut statement = tx.prepare(
                "SELECT issue_id FROM discovery_results
                 WHERE name = ?1 AND position >= ?2 AND position < ?3 AND delivered = 0",
            )?;
            let rows = statement.query_map(range, |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<HashSet<_>>>()?
        };
        tx.execute(
            "UPDATE discovery_results SET delivered = 1
             WHERE name = ?1 AND position >= ?2 AND position < ?3",
            range,
        )?;
        tx.commit()?;
        Ok((selected, u64::try_from(position)?))
    }

    pub fn stats(&self) -> Result<CacheStats> {
        let count = |table: &str| -> Result<u64> {
            let sql = format!("SELECT count(*) FROM {table}");
            let count: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
            Ok(u64::try_from(count)?)
        };
        Ok(CacheStats {
            programs: count("programs")?,
            waves: count("waves")?,
            repos: count("repos")?,
            issues: count("issues WHERE raw_json IS NOT NULL")?,
            sync_scopes: count("sync_state")?,
            scope_memberships: count("scope_membership")?,
            stale_memberships: count("scope_membership WHERE stale = 1")?,
            watchlist: count("watchlist")?,
            snapshots: count("snapshots")?,
            github_entries: count("github_cache")?,
            discoveries: count("discoveries")?,
        })
    }

    pub fn prune(&mut self, older_than: DateTime<Utc>) -> Result<PruneStats> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cutoff = [older_than.timestamp_millis()];
        let stats = PruneStats {
            programs: tx.execute("DELETE FROM programs WHERE fetched_at < ?1", cutoff)?,
            waves: tx.execute("DELETE FROM waves WHERE fetched_at < ?1", cutoff)?,
            repos: tx.execute("DELETE FROM repos WHERE fetched_at < ?1", cutoff)?,
            stale_memberships: tx.execute(
                "DELETE FROM scope_membership WHERE stale = 1 AND last_seen < ?1",
                cutoff,
            )?,
            sync_scopes: tx.execute("DELETE FROM sync_state WHERE touched_at < ?1", cutoff)?,
            issues: tx.execute(
                "DELETE FROM issues WHERE fetched_at < ?1
                   AND NOT EXISTS (SELECT 1 FROM watchlist WHERE watchlist.issue_id = issues.id)
                   AND NOT EXISTS (SELECT 1 FROM scope_membership
                                   WHERE scope_membership.issue_id = issues.id AND stale = 0)",
                cutoff,
            )?,
            github_entries: tx.execute(
                "DELETE FROM github_cache WHERE julianday(fetched_at) < julianday(?1)",
                [older_than.to_rfc3339()],
            )?,
            discoveries: tx.execute("DELETE FROM discoveries WHERE observed_at < ?1", cutoff)?,
        };
        tx.commit()?;
        Ok(stats)
    }
}

fn timestamp(value: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(value).ok_or_else(|| anyhow!("invalid cache timestamp {value}"))
}

fn read_cached<T: DeserializeOwned>(raw: &str, value: i64) -> Result<Cached<T>> {
    Ok(Cached {
        data: serde_json::from_str(raw)?,
        fetched_at: timestamp(value)?,
    })
}

fn write_scope_state(
    conn: &Connection,
    scope: &str,
    state: &ScopeState,
    now: DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_state VALUES (?1, ?2, ?3)
         ON CONFLICT(scope) DO UPDATE SET state_json=excluded.state_json, touched_at=excluded.touched_at",
        params![scope, serde_json::to_string(state)?, now.timestamp_millis()],
    )?;
    Ok(())
}

pub(crate) fn record_id(value: &Value) -> Result<&str> {
    value["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("Wave record has no ID")
}

fn write_issue<'a>(
    conn: &Connection,
    issue: &'a Value,
    program_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<&'a str> {
    let id = record_id(issue)?;
    let program_id = issue["waveProgramId"].as_str().or(program_id);
    let repo_name = repo_name(issue);
    let repo_id = issue["repo"]["id"]
        .as_str()
        .or_else(|| issue["repoId"].as_str());
    let millis = |field| issue_time(issue, field).map(|value| value.timestamp_millis());
    let number = |field: &str| {
        issue[field]
            .as_u64()
            .and_then(|value| i64::try_from(value).ok())
    };
    conn.execute(
        "INSERT INTO issues VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(id) DO UPDATE SET program_id=coalesce(excluded.program_id, issues.program_id),
            repo_name=coalesce(excluded.repo_name, issues.repo_name),
            issue_number=coalesce(excluded.issue_number, issues.issue_number), state=excluded.state,
            pending=excluded.pending, assigned=excluded.assigned, has_pr=excluded.has_pr,
            points=excluded.points, updated_at=excluded.updated_at, created_at=excluded.created_at,
            raw_json=excluded.raw_json, fetched_at=excluded.fetched_at
         WHERE issues.fetched_at <= excluded.fetched_at",
        params![
            id,
            program_id,
            repo_name,
            github_issue_number(issue).and_then(|n| i64::try_from(n).ok()),
            issue["state"].as_str(),
            number("pendingApplicationsCount"),
            assigned(issue),
            has_pr(issue),
            number("points"),
            millis("updatedAt"),
            millis("createdAt"),
            serde_json::to_string(issue)?,
            now.timestamp_millis(),
        ],
    )?;
    if let (Some(program_id), Some(name), Some(repo_id)) = (program_id, repo_name, repo_id) {
        conn.execute(
            "INSERT INTO repos VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(program_id, name) DO UPDATE SET id=excluded.id, fetched_at=excluded.fetched_at
             WHERE repos.fetched_at <= excluded.fetched_at",
            params![program_id, name, repo_id, now.timestamp_millis()],
        )?;
    }
    Ok(id)
}
