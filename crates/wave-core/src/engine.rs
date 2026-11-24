use crate::filter::{github_issue_number, issue_time, repo_name, Filter, Mode, Plan, Sort};
use crate::source::{BudgetExhausted, Page, RemoteQuery, WaveSource, PAGE_LIMIT};
use crate::store::{record_id, Cached, Scan, ScanKind, ScopeState, Store};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

const SCOPE_LOCK_WAIT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Freshness {
    Offline,
    #[default]
    Default,
    Fresh,
}

impl Freshness {
    pub fn allows_fetch(
        self,
        as_of: Option<DateTime<Utc>>,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> bool {
        match self {
            Self::Offline => false,
            Self::Default => is_stale(as_of, ttl, now),
            Self::Fresh => true,
        }
    }
}

fn is_stale(as_of: Option<DateTime<Utc>>, ttl: Duration, now: DateTime<Utc>) -> bool {
    as_of.is_none_or(|as_of| {
        now.signed_duration_since(as_of)
            .to_std()
            .is_ok_and(|age| age >= ttl)
    })
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub program_ttl: Duration,
    pub waves_ttl: Duration,
    pub listing_ttl: Duration,
    pub issue_ttl: Duration,
    pub overlap: Duration,
    pub full_interval: Duration,
    pub max_requests: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            program_ttl: Duration::from_secs(7 * 24 * 60 * 60),
            waves_ttl: Duration::from_secs(6 * 60 * 60),
            listing_ttl: Duration::from_secs(10 * 60),
            issue_ttl: Duration::from_secs(5 * 60),
            overlap: Duration::from_secs(10 * 60),
            full_interval: Duration::from_secs(24 * 60 * 60),
            max_requests: 50,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ListingOptions {
    pub freshness: Freshness,
    pub sort: Sort,
    pub limit: u32,
    pub page: u32,
    pub new_since_last: bool,
    pub full: bool,
    pub sync: bool,
}

impl Default for ListingOptions {
    fn default() -> Self {
        Self {
            freshness: Freshness::Default,
            sort: Sort::Updated,
            limit: 20,
            page: 1,
            new_since_last: false,
            full: false,
            sync: false,
        }
    }
}

impl ListingOptions {
    fn validate(self) -> Result<()> {
        if self.page == 0 || self.limit == 0 {
            bail!("page and limit must be positive");
        }
        if self.full && self.freshness == Freshness::Offline {
            bail!("a full sync requires network access");
        }
        Ok(())
    }

    fn target(self) -> u64 {
        u64::from(self.page) * u64::from(self.limit)
    }

    fn offset(self) -> usize {
        usize::try_from(u64::from(self.page - 1) * u64::from(self.limit)).unwrap_or(usize::MAX)
    }

    fn forced_scope(self) -> bool {
        self.full || self.sync || self.new_since_last
    }
}

#[derive(Debug)]
pub struct Refresh<T> {
    pub data: T,
    pub as_of: Option<DateTime<Utc>>,
    pub partial: bool,
    pub warnings: Vec<String>,
}

impl<T> Refresh<T> {
    fn fetched(data: T, as_of: DateTime<Utc>) -> Self {
        Self {
            data,
            as_of: Some(as_of),
            partial: false,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct Listing {
    pub data: Vec<Value>,
    pub as_of: Option<DateTime<Utc>>,
    pub partial: bool,
    pub warnings: Vec<String>,
    pub total: u64,
    pub plan: Plan,
}

pub struct Engine<'a, S: WaveSource + ?Sized> {
    store: &'a mut Store,
    source: &'a S,
    config: EngineConfig,
    calls: u64,
}

impl<'a, S: WaveSource + ?Sized> Engine<'a, S> {
    pub fn new(store: &'a mut Store, source: &'a S, config: EngineConfig) -> Self {
        Self {
            store,
            source,
            config,
            calls: 0,
        }
    }

    pub fn program(&mut self, slug: &str, freshness: Freshness) -> Result<Refresh<Value>> {
        let slug = slug.trim().to_ascii_lowercase();
        if slug.is_empty() {
            bail!("program slug is empty");
        }
        let cached = self.store.cached_program(&slug)?;
        let now = Utc::now();
        let ttl = self.config.program_ttl;
        let as_of = cached.as_ref().map(|value| value.fetched_at);
        if freshness.allows_fetch(as_of, ttl, now) {
            match self.request(|source| source.program(&slug)) {
                Some(result) => {
                    let data = result?;
                    record_id(&data).with_context(|| format!("Wave program {slug}"))?;
                    let now = Utc::now();
                    self.store.save_program(&slug, &data, now)?;
                    return Ok(Refresh::fetched(data, now));
                }
                None => return Ok(cached_value(cached, Value::Null, true, "program", ttl, now)),
            }
        }
        Ok(cached_value(
            cached,
            Value::Null,
            false,
            "program",
            ttl,
            now,
        ))
    }

    pub fn waves(&mut self, program_id: &str, freshness: Freshness) -> Result<Refresh<Vec<Value>>> {
        let cached = self.store.cached_waves(program_id)?;
        let now = Utc::now();
        let ttl = self.config.waves_ttl;
        let as_of = cached.as_ref().map(|value| value.fetched_at);
        if freshness.allows_fetch(as_of, ttl, now) {
            match self.request(|source| source.waves(program_id, PAGE_LIMIT)) {
                Some(result) => {
                    let data = result?;
                    let now = Utc::now();
                    self.store.save_waves(program_id, &data, now)?;
                    return Ok(Refresh::fetched(data, now));
                }
                None => return Ok(cached_value(cached, Vec::new(), true, "waves", ttl, now)),
            }
        }
        Ok(cached_value(cached, Vec::new(), false, "waves", ttl, now))
    }

    pub fn issue(
        &mut self,
        reference: &str,
        freshness: Freshness,
        program_slug: &str,
    ) -> Result<Refresh<Option<Value>>> {
        match IssueReference::parse(reference)? {
            IssueReference::Id(id) => self.issue_by_id(&id, freshness),
            IssueReference::GitHub(repo, number) => {
                if let Some(cached) = self.store.cached_issue_by_github(&repo, number)? {
                    return self.issue_by_id(record_id(&cached.data)?, freshness);
                }
                if freshness == Freshness::Offline {
                    return Ok(cached_value(
                        None,
                        None,
                        false,
                        "issue",
                        self.config.issue_ttl,
                        Utc::now(),
                    ));
                }

                self.find_github_issue(&repo, number, program_slug, freshness)
            }
        }
    }

    fn issue_by_id(&mut self, id: &str, freshness: Freshness) -> Result<Refresh<Option<Value>>> {
        let cached = self.store.cached_issue_status(id)?;
        let as_of = cached.as_ref().map(|value| value.fetched_at);
        let now = Utc::now();
        let ttl = self.config.issue_ttl;
        if freshness.allows_fetch(as_of, ttl, now) {
            match self.request(|source| source.issue(id)) {
                Some(result) => {
                    let data = result?;
                    let now = Utc::now();
                    if let Some(issue) = &data {
                        if !record_id(issue)?.eq_ignore_ascii_case(id) {
                            bail!("Wave returned a different issue ID for {id}");
                        }
                        self.store.upsert_issue(issue, now)?;
                    } else {
                        self.store.set_issue_missing(id, now)?;
                    }
                    return Ok(Refresh::fetched(data, now));
                }
                None => return Ok(cached_value(cached, None, true, "issue", ttl, now)),
            }
        }
        Ok(cached_value(cached, None, false, "issue", ttl, now))
    }

    fn find_github_issue(
        &mut self,
        repo: &str,
        number: u64,
        program_slug: &str,
        freshness: Freshness,
    ) -> Result<Refresh<Option<Value>>> {
        let mut program = self.program(program_slug, freshness)?;
        if program.data.is_null() {
            return Ok(Refresh {
                data: None,
                as_of: None,
                partial: true,
                warnings: program.warnings,
            });
        }
        let program_id = record_id(&program.data)?;
        let query = RemoteQuery {
            program_id: program_id.to_owned(),
            repo_id: self.store.repo_id(program_id, repo)?,
            sort_by: Some("updatedAt".into()),
            sort_order: Some("desc".into()),
            limit: PAGE_LIMIT,
            ..RemoteQuery::default()
        };

        // The issue number and repo are the identity. The public title is
        // only used to cheaply narrow Wave's fuzzy search.
        match self.request(|source| source.github_issue_title(repo, number)) {
            Some(Ok(Some(title))) => {
                let search = RemoteQuery {
                    search: Some(title),
                    limit: 20,
                    ..query.clone()
                };
                let (data, _) = self.lookup_pages(&search, repo, number, 3)?;
                if data.is_some() {
                    return Ok(lookup_refresh(data, program));
                }
            }
            Some(Err(error)) => program
                .warnings
                .push(format!("GitHub issue title lookup failed: {error}")),
            _ => {}
        }
        let max_pages = if query.repo_id.is_some() {
            self.remaining()
        } else {
            1
        };
        let (data, exhausted) = self.lookup_pages(&query, repo, number, max_pages)?;
        if data.is_some() || exhausted {
            return Ok(lookup_refresh(data, program));
        }
        program.warnings.push(format!(
            "could not complete lookup for {repo}#{number}; use the Wave issue UUID or narrow to its Wave program"
        ));
        Ok(Refresh {
            data: None,
            as_of: None,
            partial: true,
            warnings: program.warnings,
        })
    }

    fn lookup_pages(
        &mut self,
        query: &RemoteQuery,
        repo: &str,
        number: u64,
        max_pages: u64,
    ) -> Result<(Option<Value>, bool)> {
        let mut seen = HashSet::new();
        let mut cursor = None;
        for _ in 0..max_pages {
            let Some(result) = self.request(|source| source.issues_page(query, cursor.as_deref()))
            else {
                return Ok((None, false));
            };
            let page = result?;
            for issue in page.data {
                let matches = repo_name(&issue).is_some_and(|name| name.eq_ignore_ascii_case(repo))
                    && github_issue_number(&issue) == Some(number)
                    && issue["waveProgramId"]
                        .as_str()
                        .is_none_or(|id| id == query.program_id);
                if matches {
                    self.store.upsert_issue(&issue, Utc::now())?;
                    return Ok((Some(issue), true));
                }
            }
            match page.next_cursor {
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                Some(_) => bail!("Wave returned a repeated cursor during issue lookup"),
                None => return Ok((None, true)),
            }
        }
        Ok((None, false))
    }

    pub fn explain(
        &self,
        program_slug: &str,
        filter: &Filter,
        options: ListingOptions,
    ) -> Result<Plan> {
        options.validate()?;
        let slug = program_slug.trim().to_ascii_lowercase();
        let program = self.store.cached_program(&slug)?;
        let program_id = program
            .as_ref()
            .and_then(|cached| cached.data["id"].as_str());
        let mut plan = self.plan(program_id.unwrap_or(&slug), filter, options)?;
        let state = self.store.scope_state(&scope_key(&plan)?)?;
        plan.estimated_requests = match plan.mode {
            Mode::ScopedSync => state.and_then(|state| state.estimated_requests),
            Mode::Passthrough => Some(options.target().div_ceil(u64::from(PAGE_LIMIT))),
        };
        if plan.mode == Mode::Passthrough {
            plan.remote.limit = options.target().min(u64::from(PAGE_LIMIT)) as u32;
        }
        Ok(plan)
    }

    pub fn list(
        &mut self,
        program_slug: &str,
        filter: &Filter,
        options: ListingOptions,
    ) -> Result<Listing> {
        options.validate()?;
        let program = self.program(program_slug, options.freshness)?;
        if program.data.is_null() {
            return Ok(Listing {
                data: Vec::new(),
                as_of: None,
                partial: true,
                warnings: program.warnings,
                total: 0,
                plan: self.explain(program_slug, filter, options)?,
            });
        }
        let program_id = record_id(&program.data)?;
        let mut plan = self.plan(program_id, filter, options)?;
        let key = scope_key(&plan)?;
        let initial = self.store.scope_state(&key)?.unwrap_or_default();
        let mut state = initial.clone();
        let mut partial = program.partial;
        let mut warnings = program.warnings;
        let now = Utc::now();

        if options.freshness != Freshness::Offline
            && self.needs_sync(&key, &plan, &state, filter, options, now)?
        {
            if let Some(_guard) = self.store.lock_scope(&key, SCOPE_LOCK_WAIT)? {
                state = self.store.scope_state(&key)?.unwrap_or_default();
                let changed_while_waiting = state != initial;
                let effective = if changed_while_waiting {
                    ListingOptions {
                        freshness: Freshness::Default,
                        full: false,
                        ..options
                    }
                } else {
                    options
                };
                if self.needs_sync(&key, &plan, &state, filter, effective, now)? {
                    partial |= match plan.mode {
                        Mode::Passthrough => {
                            !self.passthrough(&key, &plan.remote, &mut state, filter, effective)?
                        }
                        Mode::ScopedSync => {
                            !self.sync_scope(&key, &plan.remote, &mut state, effective)?
                        }
                    };
                    if partial {
                        warnings.push("request budget reached; the next run can continue from the cached cursor".into());
                    }
                }
            } else {
                state = self.store.scope_state(&key)?.unwrap_or_default();
                partial = true;
                warnings
                    .push("another process is syncing this scope; serving cached results".into());
            }
        }

        let as_of = state.visible_as_of();
        if options.freshness == Freshness::Offline && is_stale(as_of, self.config.listing_ttl, now)
        {
            warnings.push(if as_of.is_none() {
                "no cached listing for this query".into()
            } else {
                "cached listing is stale".into()
            });
        }
        let mut matches = self.matches(&key, &state, filter, now)?;
        partial |= as_of.is_none()
            || match plan.mode {
                Mode::ScopedSync => state.run.is_some(),
                Mode::Passthrough => {
                    state.run.is_some() && (matches.len() as u64) < options.target()
                }
            };

        if plan.mode == Mode::ScopedSync {
            matches.sort_by(|left, right| options.sort.compare(left, right));
            plan.estimated_requests = state.estimated_requests;
        } else {
            plan.estimated_requests = Some(options.target().div_ceil(u64::from(PAGE_LIMIT)));
            plan.remote.limit = options.target().min(u64::from(PAGE_LIMIT)) as u32;
        }

        let (data, total) = if options.new_since_last {
            if partial {
                warnings.push("discovery was deferred until the scope sync completes".into());
                (Vec::new(), 0)
            } else {
                // A learned repo UUID can tighten the remote scope. The
                // discovery baseline still represents the same user filter.
                let name = serde_json::to_string(&(program_id, filter.normalized()))?;
                let ids = matches.iter().map(record_id).collect::<Result<Vec<_>>>()?;
                let (selected, total) =
                    self.store
                        .discover(&name, &ids, options.offset(), options.limit, now)?;
                matches
                    .retain(|issue| issue["id"].as_str().is_some_and(|id| selected.contains(id)));
                (matches, total)
            }
        } else {
            let count = matches.len() as u64;
            let total = if plan.mode == Mode::Passthrough && state.run.is_some() {
                state.total.unwrap_or(count).max(count)
            } else {
                count
            };
            let data = matches
                .into_iter()
                .skip(options.offset())
                .take(options.limit as usize)
                .collect();
            (data, total)
        };
        Ok(Listing {
            data,
            as_of,
            partial,
            warnings,
            total,
            plan,
        })
    }

    fn plan(&self, program_id: &str, filter: &Filter, options: ListingOptions) -> Result<Plan> {
        let mut names = filter.repos.iter().map(|name| name.to_ascii_lowercase());
        let first = names.next();
        let repo_id = match first {
            Some(first) if names.all(|name| name == first) => {
                self.store.repo_id(program_id, &first)?
            }
            _ => None,
        };
        let planned = filter.plan(
            program_id.into(),
            repo_id,
            options.sort,
            options.forced_scope(),
        );
        let current = self.store.scope_state(&scope_key(&planned)?)?;
        if options.full
            || current
                .as_ref()
                .is_some_and(|state| state.run.is_some() || state.as_of.is_some())
        {
            return Ok(planned);
        }

        // A completed superset is just as authoritative offline and can also
        // preserve a zero-request repeat just after learning a repo UUID.
        let supersets = [
            filter.plan(
                program_id.into(),
                planned.remote.repo_id.clone(),
                options.sort,
                true,
            ),
            filter.plan(program_id.into(), None, options.sort, true),
        ];
        for candidate in supersets {
            if candidate.mode == planned.mode && candidate.remote == planned.remote {
                continue;
            }
            let Some(state) = self.store.scope_state(&scope_key(&candidate)?)? else {
                continue;
            };
            let resuming_repo_scope = !filter.repos.is_empty()
                && candidate.remote.repo_id.is_none()
                && state.run.is_some();
            let usable = resuming_repo_scope
                || state.as_of.is_some_and(|as_of| {
                    !options.freshness.allows_fetch(
                        Some(as_of),
                        self.config.listing_ttl,
                        Utc::now(),
                    )
                });
            if usable {
                return Ok(candidate);
            }
        }
        Ok(planned)
    }

    fn needs_sync(
        &self,
        key: &str,
        plan: &Plan,
        state: &ScopeState,
        filter: &Filter,
        options: ListingOptions,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let is_old =
            options
                .freshness
                .allows_fetch(state.visible_as_of(), self.config.listing_ttl, now);
        Ok(match plan.mode {
            Mode::ScopedSync => options.full || is_old || state.run.is_some(),
            Mode::Passthrough => {
                is_old
                    || state.run.as_ref().is_some_and(|run| run.paused)
                    || (state.run.is_some()
                        && (self.matches(key, state, filter, now)?.len() as u64) < options.target())
            }
        })
    }

    fn matches(
        &self,
        key: &str,
        state: &ScopeState,
        filter: &Filter,
        now: DateTime<Utc>,
    ) -> Result<Vec<Value>> {
        let mut issues = self.store.scope_issues(key, state.visible_generation())?;
        issues.retain(|issue| filter.matches(issue, now));
        Ok(issues)
    }

    fn passthrough(
        &mut self,
        key: &str,
        remote: &RemoteQuery,
        state: &mut ScopeState,
        filter: &Filter,
        options: ListingOptions,
    ) -> Result<bool> {
        let now = Utc::now();
        let stale =
            options
                .freshness
                .allows_fetch(state.visible_as_of(), self.config.listing_ttl, now);
        let had_budget_pause = state.run.as_ref().is_some_and(|run| run.paused);
        if state.run.is_none() || (stale && !had_budget_pause) {
            state.run = Some(Scan::new(ScanKind::Pass, state));
            state.total = None;
        }
        loop {
            let generation = state
                .run
                .as_ref()
                .map_or(state.generation, |run| run.generation);
            let found = self
                .store
                .scope_issues(key, generation)?
                .iter()
                .filter(|issue| filter.matches(issue, now))
                .count() as u64;
            if found >= options.target() || state.run.is_none() {
                if let Some(run) = &mut state.run {
                    run.paused = false;
                    self.store.save_scope_state(key, state, Utc::now())?;
                }
                return Ok(true);
            }
            let run = state.run.as_ref().expect("passthrough scan");
            let q = RemoteQuery {
                limit: (options.target() - found).min(u64::from(PAGE_LIMIT)) as u32,
                ..remote.clone()
            };
            let Some(result) = self.request(|source| source.issues_page(&q, run.cursor.as_deref()))
            else {
                if let Some(run) = &mut state.run {
                    run.paused = true;
                }
                self.store.save_scope_state(key, state, Utc::now())?;
                return Ok(false);
            };
            self.apply_page(key, remote, result?, state, None)?;
        }
    }

    fn sync_scope(
        &mut self,
        key: &str,
        remote: &RemoteQuery,
        state: &mut ScopeState,
        options: ListingOptions,
    ) -> Result<bool> {
        let now = Utc::now();
        let replace_incremental = options.full
            && state
                .run
                .as_ref()
                .is_some_and(|run| run.kind == ScanKind::Incremental);
        if state.run.is_none() || replace_incremental {
            let mandatory_full = state.full_at.is_none() || options.full;
            let auto_full = is_stale(state.full_at, self.config.full_interval, now)
                && state
                    .estimated_requests
                    .is_none_or(|estimate| estimate < self.remaining());
            let mut kind = ScanKind::Incremental;
            if mandatory_full || auto_full {
                let probe = RemoteQuery {
                    limit: 1,
                    ..remote.clone()
                };
                let Some(result) = self.request(|source| source.issues_page(&probe, None)) else {
                    return Ok(false);
                };
                let page = result?;
                state.estimated_requests = page
                    .total
                    .map(|total| total.div_ceil(u64::from(PAGE_LIMIT)));
                let remaining = self.remaining();
                match state.estimated_requests {
                    Some(estimate) if estimate > remaining && mandatory_full => {
                        self.store.save_scope_state(key, state, now)?;
                        bail!(
                            "scope needs about {estimate} page requests after the probe, with {remaining} remaining; narrow filters (for example, --repo or --no-pr) or raise --max-requests"
                        );
                    }
                    Some(estimate) if estimate > remaining => {}
                    _ => kind = ScanKind::Full,
                }
                if kind == ScanKind::Full
                    && page.total == Some(0)
                    && page.data.is_empty()
                    && page.next_cursor.is_none()
                {
                    state.run = Some(Scan::new(kind, state));
                    self.apply_page(key, remote, page, state, None)?;
                    return Ok(true);
                }
            }
            state.run = Some(Scan::new(kind, state));
            self.store.save_scope_state(key, state, now)?;
        }

        let cutoff = state.high_water.and_then(|watermark| {
            TimeDelta::from_std(self.config.overlap)
                .ok()
                .and_then(|overlap| watermark.checked_sub_signed(overlap))
        });
        while let Some(run) = &state.run {
            let Some(result) =
                self.request(|source| source.issues_page(remote, run.cursor.as_deref()))
            else {
                return Ok(false);
            };
            self.apply_page(key, remote, result?, state, cutoff)?;
        }
        Ok(true)
    }

    fn apply_page(
        &mut self,
        key: &str,
        remote: &RemoteQuery,
        mut page: Page<Value>,
        state: &mut ScopeState,
        cutoff: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let now = Utc::now();
        let run = state.run.as_mut().context("missing scope scan")?;
        let end_of_overlap = run.kind == ScanKind::Incremental
            && cutoff.is_some_and(|cutoff| {
                !page.data.is_empty()
                    && page.data.iter().all(|issue| {
                        issue_time(issue, "updatedAt").is_some_and(|updated| updated < cutoff)
                    })
            });
        if end_of_overlap {
            page.data.clear();
        }
        page.data.retain(|issue| {
            issue["waveProgramId"]
                .as_str()
                .is_none_or(|program| program == remote.program_id)
        });
        let newest = page
            .data
            .iter()
            .filter_map(|issue| issue_time(issue, "updatedAt"))
            .max();
        run.high_water = run.high_water.max(newest);
        run.fetched_at.get_or_insert(now);
        let generation = run.generation;
        let position = run.position;
        let next = page.next_cursor.filter(|_| !end_of_overlap);
        if let Some(next) = &next {
            if run.cursor.as_ref() == Some(next) || !run.seen_cursors.insert(next.clone()) {
                bail!("Wave returned a repeated cursor for this scope");
            }
        }
        if run.kind == ScanKind::Pass {
            state.total = page.total.or(state.total);
        }
        if next.is_none() {
            state.generation = generation;
            state.as_of = if run.kind == ScanKind::Pass {
                run.fetched_at
            } else {
                Some(now)
            };
            if run.kind == ScanKind::Full {
                state.full_at = Some(now);
                state.high_water = run.high_water;
            } else if run.kind == ScanKind::Incremental {
                state.high_water = state.high_water.max(run.high_water);
            }
            state.run = None;
        } else {
            run.cursor = next;
            run.position += page.data.len() as i64;
        }
        self.store.commit_page(
            key,
            &remote.program_id,
            &page.data,
            generation,
            position,
            state,
            now,
        )
    }

    fn remaining(&self) -> u64 {
        self.config
            .max_requests
            .saturating_sub(self.calls)
            .min(self.source.remaining_requests().unwrap_or(u64::MAX))
    }

    fn request<T>(&mut self, run: impl FnOnce(&S) -> Result<T>) -> Option<Result<T>> {
        if self.remaining() == 0 {
            return None;
        }
        self.calls += 1;
        match run(self.source) {
            Err(error) if error.downcast_ref::<BudgetExhausted>().is_some() => None,
            result => Some(result),
        }
    }
}

fn lookup_refresh(data: Option<Value>, program: Refresh<Value>) -> Refresh<Option<Value>> {
    Refresh {
        data,
        as_of: Some(Utc::now()),
        partial: program.partial,
        warnings: program.warnings,
    }
}

fn scope_key(plan: &Plan) -> Result<String> {
    let key = serde_json::to_string(&(plan.mode, &plan.remote))?;
    Ok(key)
}

fn cached_value<T>(
    cached: Option<Cached<T>>,
    missing: T,
    budget: bool,
    resource: &str,
    ttl: Duration,
    now: DateTime<Utc>,
) -> Refresh<T> {
    let mut warnings = Vec::new();
    if budget {
        warnings.push(format!(
            "request budget reached before refreshing {resource}"
        ));
    }
    let partial = budget || cached.is_none();
    if let Some(cached) = cached {
        if is_stale(Some(cached.fetched_at), ttl, now) {
            warnings.push(format!("cached {resource} is stale"));
        }
        Refresh {
            data: cached.data,
            as_of: Some(cached.fetched_at),
            partial,
            warnings,
        }
    } else {
        warnings.push(format!("no cached {resource} data"));
        Refresh {
            data: missing,
            as_of: None,
            partial,
            warnings,
        }
    }
}

enum IssueReference {
    Id(String),
    GitHub(String, u64),
}

impl IssueReference {
    fn parse(reference: &str) -> Result<Self> {
        let reference = reference.trim();
        if is_uuid(reference) {
            return Ok(Self::Id(reference.to_ascii_lowercase()));
        }
        let repo_number = [
            "https://github.com/",
            "http://github.com/",
            "https://www.github.com/",
        ]
        .iter()
        .find_map(|prefix| reference.strip_prefix(prefix))
        .and_then(|path| path.split(['?', '#']).next())
        .map(|path| path.trim_end_matches('/'))
        .and_then(|path| path.rsplit_once("/issues/"))
        .or_else(|| reference.rsplit_once('#'));
        if let Some((repo, number)) = repo_number {
            if let Some((owner, name)) = repo.split_once('/') {
                if valid_repo_part(owner) && valid_repo_part(name) {
                    if let Ok(number) = number.parse::<u64>() {
                        if number > 0 {
                            return Ok(Self::GitHub(repo.to_owned(), number));
                        }
                    }
                }
            }
        }
        bail!("issue reference must be a Wave UUID, owner/repo#number, or GitHub issue URL")
    }
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn valid_repo_part(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
