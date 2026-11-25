# dwave — Drips Wave issue tracker CLI (design)

Status: draft for approval · 2026-11-24

## 1. Intent

Contributors to Drips Wave programs need to find issues worth applying to and
follow the issues they care about (pending applications, assignment, linked PR)
without hammering an undocumented API or re-scanning 30k+ issues by hand.

`dwave` is an unofficial, general-purpose CLI plus a reusable Rust library. The
"low-competition" scan (zero applications, unassigned, no PR) was the author's
original need; it ships as one example preset among several and gets no special
status in the code. Anyone should be able to express their own queries,
track their own watchlist, and script it via JSON output. A companion Claude
skill (English) drives the CLI through its JSON interface.

## 2. Goals / non-goals / anti-goals

Goals
- Paginated, filterable issue listing for any Wave program (not only Stellar).
- Watchlist tracking: detect changes in pending applications, assignment, PR
  link, state, points between runs.
- Request economy: local cache, freshness policy, incremental sync, server-side
  filter push-down, rate limiting. A repeat query within TTL makes zero requests.
- Stable machine output (`--json`, NDJSON) with a versioned schema, for the
  skill and for scripts.
- Optional GitHub cross-check (issue state, assignees, linked PRs).

Non-goals
- No daemon, no push notifications. Tracking is on-demand (`watch check`);
  scheduling is the user's cron/launchd. Exit codes make that easy.
- No writes to Wave (applying, commenting). Read-only.
- No web UI / TUI.
- No hosted or shared cache, proxy, or server component. The cache is a local
  SQLite file per user; nothing leaves the machine except requests to
  wave-api and (optionally) api.github.com.
- No full mirror of a program. Mirroring all open issues (~33.6k in Stellar,
  ≈340 requests, minutes of wall time at a polite rate) is slow and of little
  value: most rows never match anyone's filter, and the copy is stale within
  minutes because `updatedAt` churns every few seconds. Sync is always scoped
  to what a query needs (§5.5).

Anti-goals (failure even if it "works")
- Causing load on wave-api that a normal browser session would not
  (e.g. naive full scans on every invocation).
- Breaking on harmless API changes (new fields, new enum values).
- Looking like an official Drips product.

## 3. Verified API facts (probed 2026-11-24)

Base `https://wave-api.drips.network`, anonymous GET, no docs, no stability promise.

| Fact | Consequence |
|---|---|
| `GET /api/wave-programs/{slug}` → program (`id`). | Resolve slug once, cache for days. |
| `GET /api/wave-programs/{id}/waves?limit=N` → `data[]` with `waveNumber,status,startDate,endDate`. | Current wave derived, cached hours. |
| `GET /api/issues?waveProgramId=&state=open&sortBy=updatedAt&limit=` | Main listing. ~33.6k open issues in Stellar. |
| `limit` max **100** (200 → empty `data`). | Full scan ≈ 340 requests → must be rare. |
| Pagination: `pagination{page,limit,total,totalPages,hasNextPage,nextCursor}`; **`cursor=<nextCursor>`** works (keyset; base64url JSON `{s,o,v,id}`); `page=N` also works (offset). | Use `cursor`; offset pages shift under churn. |
| Cursor `v` is truncated to whole seconds. | Possible duplicates across pages → dedupe by `id`. |
| `sortBy=updatedAt` is **not strictly monotonic** within a page. `sortOrder=asc` works. | Incremental sync needs an overlap window, not an exact watermark. |
| `updatedAt` churns every few seconds program-wide. | Incremental sync volume is non-trivial but bounded. |
| `hasApplications=false` → server filter (4,079 vs 33,573). Sample: all `pendingApplicationsCount==0`, unassigned. `hasApplications=true` still contains pending=0 rows. | "Never had an application" ⊂ "zero pending". Pushable only for that predicate. |
| `hasPr=false` → server filter (12,142). | Pushable. |
| `hasApplications=false&hasPr=false` combine as AND (1,403; sample all pending 0, unassigned, no PR). | Low-competition full sync ≈ 15 requests. |
| `search=` → server title search (fuzzy; must re-match by `repo.gitHubRepoFullName` + `gitHubIssueNumber`). | Used for `owner/repo#n` lookup. |
| `repoId=` exists (rejects non-UUID). | Pushable once repo UUIDs are known from cache. |
| Ignored (silently): `pendingApplicationsCount`, `assigned`, `minPoints`, `repoFullName`, `isAssigned`, … | Must be evaluated locally. |
| No `ETag`, `Last-Modified`, `Cache-Control`, or rate-limit headers. | No conditional requests on Wave; our own TTL cache + client-side rate limit. |
| `GET /api/issues/{id}` → single issue. | Cheap refresh path for watched issues. |

Unverified (builder must probe before relying on it): whether a change in
`pendingApplicationsCount` / `assignedApplicant` bumps `updatedAt`. Design does
not depend on it for watched issues (see §6.4).

Identifiers: Drips `id` (UUID, primary key everywhere), `gitHubIssueId`,
`gitHubIssueNumber` are distinct. GitHub URL = `repo.gitHubRepoUrl + "/issues/" + gitHubIssueNumber`.

## 4. Approaches considered

| Option | Pros | Cons |
|---|---|---|
| **Rust (chosen)** | Single static binary, `cargo install`/release assets; strong typed models via serde; lib+bin split gives a reusable crate; matches the user's other Rust work. | Slower iteration/compile; more ceremony around async. |
| Go | Single binary, fast to write. | Weaker modelling of optional/unknown fields; less natural lib reuse for this user. |
| TypeScript/Node | Fastest iteration, npm distribution. | Users need Node; weaker guarantees on API drift. |

Tracking mode: **on-demand diff + cron** (user's choice) over a long-running
daemon — simpler, no process management, composes with any scheduler.

## 5. Architecture

Cargo workspace:

```
dwave/
  crates/wave-core/   # library: everything except argument parsing and rendering
  crates/dwave/       # binary: clap CLI, output rendering
  skills/dwave/       # Claude skill (SKILL.md, English)
```

```
dwave CLI ─► Query planner ─► Store (SQLite) ◄─ SyncEngine ─► WaveSource ─► Gateway(rate limit, retry, budget)
          └► Watcher ──────► Store ◄──────────┘                 GitHubSource ─► Gateway (optional, ETag/304)
```

### 5.1 Core abstractions (wave-core)

Signatures are indicative; the builder may adjust names.

```rust
/// Read-only access to a Wave backend. Real impl = HTTP; tests = fixtures.
#[async_trait]
pub trait WaveSource {
    async fn program(&self, slug: &str) -> Result<Program>;
    async fn waves(&self, program: ProgramId, limit: u32) -> Result<Vec<Wave>>;
    async fn issues_page(&self, q: &RemoteQuery, cursor: Option<&Cursor>) -> Result<Page<Issue>>;
    async fn issue(&self, id: IssueId) -> Result<Option<Issue>>;
}

/// Persisted cache + watchlist + snapshot history.
pub trait Store {
    fn upsert_issues(&mut self, issues: &[Issue], fetched_at: Timestamp) -> Result<()>;
    fn query(&self, q: &LocalQuery, page: PageReq) -> Result<PageResult<IssueRow>>;
    fn sync_state(&self, scope: &SyncScope) -> Result<Option<SyncState>>;
    fn set_sync_state(&mut self, s: &SyncState) -> Result<()>;
    fn watch_add(&mut self, id: IssueId, note: Option<String>) -> Result<()>;
    fn watch_remove(&mut self, id: IssueId) -> Result<()>;
    fn watchlist(&self) -> Result<Vec<WatchEntry>>;
    fn record_snapshot(&mut self, s: &Snapshot) -> Result<()>;
    fn last_snapshot(&self, id: IssueId) -> Result<Option<Snapshot>>;
}

/// Optional enrichment; never required for core commands.
#[async_trait]
pub trait GitHubSource {
    async fn issue_status(&self, repo: &RepoName, number: u64) -> Result<GhIssueStatus>; // state, assignees, linked PRs (timeline)
}
```

Models: `Issue`, `Program`, `Wave`, `Repo` are serde structs with
`Option<_>` for every non-identity field and `#[serde(flatten)] extra:
serde_json::Map` so unknown fields survive round-trip into the cache (raw JSON is
also stored, see §5.3). Enums (`state`, wave `status`) have an `Unknown(String)`
fallback.

### 5.2 Query model and push-down

A single `Filter` value is built from CLI flags or a preset:

```rust
pub struct Filter {
    pub state: Option<IssueState>,          // default Open
    pub pending: Option<Range<u32>>,        // --pending 0, --pending ..=2
    pub never_applied: Option<bool>,        // server: hasApplications
    pub assigned: Option<bool>,             // --unassigned / --assigned
    pub has_pr: Option<bool>,               // server: hasPr
    pub points: Option<Range<u32>>,         // --min-points / --max-points
    pub repos: Vec<RepoName>,               // --repo owner/name (repeatable)
    pub labels: Vec<String>,
    pub search: Option<String>,             // server: search
    pub updated_since: Option<Duration>,
}
```

The planner splits a `Filter` into `RemoteQuery` (predicates the server honours,
from a static capability table: `state`, `hasApplications`, `hasPr`, `search`,
`repoId`, `sortBy`, `sortOrder`) and `LocalQuery` (everything, re-applied
locally for correctness). Remote predicates only narrow what is fetched; local
evaluation is always authoritative. Adding a newly discovered server param is a
one-line capability-table change.

Presets are named `Filter`s and carry no special status in code: `dwave
issues` with no preset lists open issues sorted by `updated`.

The planner picks one of two execution modes:

- **Passthrough** — every predicate is server-pushable (e.g. no filter,
  `--search`, `--never-applied`, `--no-pr`, `--repo`): fetch only the pages the
  user asked for (`--limit 20` → 1 request), cache those rows, done. No scope
  sync, so cheap for everyone regardless of how broad the result set is.
- **Scoped sync** — at least one predicate is local-only (pending count,
  assignee, points, labels): sync the narrowest remote scope (§5.5), then
  filter and paginate locally. Cost is estimated first and bounded by the
  budget.

`--explain` prints the chosen mode, the remote query, the local-only filters,
and estimated requests.

Built-in presets are plain data shipped as
examples for different kinds of users; each lists its estimated sync cost in
`dwave preset list`:

| Preset | Filter | Who it serves | Cost |
|---|---|---|---|
| `untouched` | open, never applied, no PR | contributors wanting zero competition (the original need) | cheap (pushable) |
| `open-pending-0` | open, pending 0, unassigned, no PR | same, wider: includes issues whose earlier applications were rejected/withdrawn | broad (`hasPr=false` ≈12k rows); needs `--repo` or raised budget |
| `unassigned` | open, unassigned, no PR | contributors happy to compete on quality | broad |
| `high-value` | open, unassigned, points ≥ configurable threshold | contributors optimising points | broad |
| `repo` | open, `--repo` required | maintainers / repo-focused contributors watching their own repos | cheap once repo UUID cached (`repoId`) |

Users save their own in the config file (`dwave preset save <name>
<flags…>`) and can override or hide built-ins. Thresholds in built-ins are
config values, not constants.

### 5.3 Store

SQLite (rusqlite, bundled) at the platform data dir
(`directories` crate; e.g. `~/Library/Application Support/dwave/dwave.db`),
overridable via `--db` / `DWAVE_DB`.

Tables (logical): `programs`, `waves`, `repos`, `issues` (typed indexed columns
for filterable fields + `raw_json` + `fetched_at`), `sync_state` (per scope:
high-water `updatedAt`, last full-sync time), `watchlist`, `snapshots`
(issue id, taken_at, tracked fields), `github_cache` (url, etag, body,
fetched_at). Schema versioned with forward-only migrations.

### 5.4 Freshness policy

Every read goes through a `Freshness` policy:

- `--offline`: never fetch; use cache, warn if stale.
- default: fetch only if the relevant data is older than its TTL.
- `--fresh`: ignore TTL, refresh what the command needs.

Default TTLs (configurable): program 7 d, waves 6 h, issue listing scope 10 min,
single issue (watch) 5 min, GitHub 30 min. Every output includes `as_of`
timestamps so staleness is visible.

### 5.5 Sync engine

- **Scoped**: a sync scope = (program, RemoteQuery). Narrow scopes such as
  `untouched` (`hasApplications=false&hasPr=false`, ≈15 requests) are far
  cheaper than "all open". Before a scope's first full sync the engine reads
  `pagination.total` from a `limit=1` probe and estimates cost
  (`ceil(total/100)` requests); if the estimate exceeds the per-run budget it
  refuses unless `--max-requests` is raised explicitly, and suggests narrowing
  filters (e.g. `--repo`, a pushable predicate).
- **Incremental**: page `sortBy=updatedAt` desc with `cursor`; stop when a full
  page is older than `high_water − overlap` (overlap default 10 min, absorbs
  the non-monotonic ordering). Dedupe by `id`. Persist new high-water.
- **Full resync** of a scope when there is no state, on `--full`, or when the
  last full sync is older than a threshold (default 24 h) — this catches rows
  that left the scope (got an application, got assigned) and the unverified
  `updatedAt` question. The automatic 24 h resync applies only to scopes whose
  estimated cost fits the budget; broad scopes resync only on explicit
  `--full`. Rows not seen in a full scope sync are marked `stale_in_scope`,
  not deleted.
- **Cross-process safety**: cron runs can overlap. A sync takes an advisory
  file lock on the database (per scope); a second process waits briefly, then
  serves from cache with `meta.partial` / a warning instead of fetching twice.
- **Budget**: `--max-requests N` hard cap per invocation (default e.g. 50);
  sync reports `partial: true` when it hits it, and resumes from the saved
  cursor next time.

### 5.6 Request gateway (in-process)

Yes, one unified gateway — but as a library layer, not a separate proxy or
service. Every outbound request from `WaveSource` and `GitHubSource` goes
through a single `Gateway` value owned by the process; sources never hold
their own HTTP client. Reasons: wave-api publishes no rate-limit headers, so
politeness must be enforced client-side in one place; GitHub has strict
quotas; and budgets/counters/`--explain` need one choke point. A standalone
proxy would add a process to run and buys nothing for a single-user CLI.

```rust
pub struct Gateway { /* reqwest::Client + per-host policies + budget + metrics */ }

pub struct HostPolicy {
    pub rate: Rate,              // token bucket; wave-api default 2 req/s, burst 4
    pub retry: RetryPolicy,      // exp. backoff + full jitter; max 3 attempts
    pub timeout: Duration,       // per attempt
    pub honor_retry_after: bool, // 429/503 Retry-After; GitHub x-ratelimit-reset
}

impl Gateway {
    pub async fn get_json<T: DeserializeOwned>(&self, req: GetRequest) -> Result<Fetched<T>>;
    // GetRequest: url, optional If-None-Match; Fetched: body | NotModified, etag, status
    pub fn budget(&self) -> &Budget;   // per-invocation cap (--max-requests); Err(BudgetExhausted)
    pub fn metrics(&self) -> Metrics;  // requests, retries, 304s, bytes → JSON meta / -v
}
```

Responsibilities, in order per request: budget check → per-host token bucket →
send with timeout → classify (`2xx`, `304`, retryable `429/5xx/timeout/conn`,
fatal `4xx`) → retry with backoff (counts against budget) → decode. Retries
are GET-only (the tool never writes). User-agent `dwave/<version> (+repo url)`,
no personal data.

Scope limits: the token bucket is per process. Cross-process coordination is
limited to the sync file lock (§5.5); with on-demand + cron that is enough, and
a shared limiter across processes is explicitly out of scope.

The `Gateway` sits behind a small trait (`HttpTransport`) so tests inject a
fixture transport and assert request counts, retries, and budget behaviour
without network.

### 5.7 Watcher (tracking)

`watch check`:
1. Refresh each watched issue via `GET /api/issues/{id}` if older than its TTL
   (watchlists are small; this avoids depending on `updatedAt` semantics).
2. Optionally (`--github`) refresh GitHub status with ETag conditional requests.
3. Diff against the last snapshot over tracked fields: `pendingApplicationsCount`,
   `assignedApplicant`, `assignees`, `prLink`, `state`, `points`, GitHub
   state/assignees/linked PRs.
4. Emit `ChangeEvent`s (`ApplicationsChanged{from,to}`, `Assigned{who}`,
   `Unassigned`, `PrLinked{url}`, `StateChanged`, `PointsChanged`,
   `GitHubChanged{…}`, `Disappeared`), record the new snapshot.

Exit codes: `0` no changes, `10` changes found, `1` error, `2` usage error —
so cron can act only on changes. `dwave watch log` shows history from snapshots.

Discovery tracking ("new matches since last run", for any preset or flags) is
e.g. `dwave issues --preset untouched --new-since-last` using sync-state
first-seen timestamps; same exit-code convention.

### 5.8 GitHub enrichment

Optional. Token from `GITHUB_TOKEN`/`GH_TOKEN`, else `gh auth token` if `gh` is
installed, else anonymous (60 req/h, warn). Uses
`GET /repos/{o}/{r}/issues/{n}` and `/timeline` with `If-None-Match`; 304s do not
count against GitHub's primary rate limit. Results cached in `github_cache`.

## 6. CLI surface (English)

```
dwave program show [<slug>]                 # resolve + cache; default from config (stellar)
dwave program use <slug>                    # set default program
dwave waves [--current]
dwave sync [--scope <preset>|<flags>] [--full] [--max-requests N]
dwave issues [filters] [--preset NAME] [--sort points|updated|created] [--limit N] [--page N]
             [--new-since-last] [--github] [--fresh|--offline]
dwave issue show <uuid | owner/repo#N | github-url> [--github]
dwave watch add <ref>... [--note TEXT]
dwave watch rm <ref>...
dwave watch list
dwave watch check [--github] [--fresh]
dwave watch log [<ref>] [--since 7d]
dwave preset list | save <name> [filters] | rm <name>
dwave cache stats | prune [--older-than 30d]
dwave config get|set <key> [value]
```

Global: `--program <slug>`, `--json`, `--ndjson`, `--db PATH`, `-v`, `--no-color`.

`issues` paginates over the local result set (`--limit` default 20, `--page`),
after an implicit scoped sync governed by the freshness policy.

Output: human table by default (points, pending, assignee, PR?, repo#n, title,
short id). JSON envelope:

```json
{ "schema": "dwave/v1", "meta": { "program": "stellar", "as_of": "…", "requests": 3, "partial": false },
  "data": [ … ], "page": { "page": 1, "limit": 20, "total": 412 } }
```

The JSON schema is the contract the skill depends on; changes bump `schema`.

Config: TOML at the platform config dir (`default_program`, TTLs, rate limit,
presets).

## 7. Claude skill (English)

`skills/dwave/SKILL.md`: when to use (finding/triaging Wave issues, tracking
applications), install check, always use `--json`, recipes (`untouched` and other presets,
scan, inspect one issue with `--github`, manage watchlist, interpret
`watch check` events and exit codes), request-economy rules (prefer cache,
don't loop `--fresh`), and the caveat that Wave `prLink == null` ≠ no PR —
confirm with `--github`.

## 8. Repo & licensing

Public GitHub repo (name `dwave`, confirm with user before creating), dual
license **MIT OR Apache-2.0**, README with "unofficial, not affiliated with
Drips; API undocumented and may change" disclaimer. CI: fmt, clippy, tests
(fixture-based `WaveSource`, no live calls in CI).

## 9. Testing strategy

- Recorded JSON fixtures from the probes → `FixtureSource: WaveSource`.
- Unit: planner split, filter evaluation, diff engine, sync stop condition with
  non-monotonic pages and duplicate ids, freshness decisions.
- Store tests on in-memory SQLite.
- One opt-in live smoke test (`--ignored`) against the real API.

## 10. Left to the builder

- Exact crate choices beyond: clap, tokio, reqwest (rustls), serde, rusqlite
  (bundled), directories, thiserror/anyhow, comfy-table.
- Concrete default numbers (TTLs, overlap, budgets) — tune after a few real runs.
- Probe whether application/assignment changes bump `updatedAt`; if yes,
  relax the 24 h full-resync default.

## 11. Delivery order

1. Abstract skeleton: workspace, models, traits, Filter/planner types, error
   types, CLI command tree with stub handlers (compiles, `--help` works).
2. HTTP `WaveSource` + SQLite `Store` + freshness.
3. Sync engine, `issues`, `issue show`.
4. Watcher + exit codes; presets/config.
5. GitHub enrichment.
6. Skill, README, license, CI, GitHub repo.
