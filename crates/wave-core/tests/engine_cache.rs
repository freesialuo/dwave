use anyhow::Result;
use chrono::{TimeDelta, Utc};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::Duration;
use wave_core::engine::{Engine, EngineConfig, Freshness, Listing, ListingOptions};
use wave_core::filter::{Bounds, Filter, Mode, Sort};
use wave_core::source::{Page, RemoteQuery, WaveSource};
use wave_core::store::Store;

const PROGRAM: &str = "program";
const REPO: &str = "owner/repo";

enum Reply {
    Program,
    Title(String),
    Listing {
        limit: u32,
        cursor: Option<&'static str>,
        search: Option<&'static str>,
        page: Page<Value>,
    },
    Issue(Option<Value>),
}

struct Fixture(RefCell<VecDeque<Reply>>);

impl Fixture {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self(RefCell::new(replies.into_iter().collect()))
    }

    fn next(&self) -> Reply {
        self.0
            .borrow_mut()
            .pop_front()
            .expect("unexpected API request")
    }

    fn complete(&self) {
        assert!(self.0.borrow().is_empty(), "missing expected API request");
    }
}

impl WaveSource for Fixture {
    fn program(&self, slug: &str) -> Result<Value> {
        assert_eq!(slug, "stellar");
        assert!(matches!(self.next(), Reply::Program));
        Ok(json!({"id": PROGRAM, "futureField": "preserved"}))
    }

    fn waves(&self, _: &str, _: u32) -> Result<Vec<Value>> {
        panic!("unexpected waves API request");
    }

    fn issues_page(&self, query: &RemoteQuery, cursor: Option<&str>) -> Result<Page<Value>> {
        let Reply::Listing {
            limit,
            cursor: expected,
            search,
            page,
        } = self.next()
        else {
            panic!("expected a different API request");
        };
        assert_eq!(query.program_id, PROGRAM);
        assert_eq!(query.limit, limit);
        assert_eq!(query.search.as_deref(), search);
        assert_eq!(cursor, expected);
        Ok(page)
    }

    fn issue(&self, _: &str) -> Result<Option<Value>> {
        let Reply::Issue(issue) = self.next() else {
            panic!("expected a different API request");
        };
        Ok(issue)
    }

    fn github_issue_title(&self, repo: &str, number: u64) -> Result<Option<String>> {
        assert!(repo.eq_ignore_ascii_case(REPO));
        assert_eq!(number, 7);
        let Reply::Title(title) = self.next() else {
            panic!("expected a different API request");
        };
        Ok(Some(title))
    }
}

fn page(
    limit: u32,
    cursor: Option<&'static str>,
    next: Option<&str>,
    total: u64,
    data: Vec<Value>,
) -> Reply {
    Reply::Listing {
        limit,
        cursor,
        search: None,
        page: Page {
            data,
            total: Some(total),
            next_cursor: next.map(str::to_owned),
        },
    }
}

fn issue(id: &str, points: u32, pr: bool, updated_at: &str) -> Value {
    json!({
        "id": id,
        "waveProgramId": PROGRAM,
        "state": "open",
        "points": points,
        "title": "Test issue",
        "updatedAt": updated_at,
        "createdAt": updated_at,
        "assignedApplicant": null,
        "assignees": [],
        "prLink": pr.then_some("https://github.com/owner/repo/pull/2"),
        "pendingApplicationsCount": 0,
        "repo": {"gitHubRepoFullName": REPO},
        "gitHubIssueNumber": 7,
        "unknownServerField": {"kept": true}
    })
}

const RECENT: &str = "2026-11-24T16:00:00Z";

fn ids(list: &Listing) -> Vec<&str> {
    list.data
        .iter()
        .map(|issue| issue["id"].as_str().unwrap())
        .collect()
}

#[test]
fn passthrough_extends_cached_prefix_and_repeats_without_requests() -> Result<()> {
    let mut store = Store::memory()?;
    let a = issue("a", 10, false, RECENT);
    let b = issue("b", 10, false, RECENT);
    let c = issue("c", 10, false, RECENT);
    let d = issue("d", 10, false, RECENT);
    let source = Fixture::new([
        Reply::Program,
        page(2, None, Some("page2"), 4, vec![a, b.clone()]),
        page(2, Some("page2"), Some("page3"), 4, vec![b, c]),
        page(1, Some("page3"), None, 4, vec![d]),
    ]);
    let mut engine = Engine::new(&mut store, &source, EngineConfig::default());
    let filters = Filter::default();
    let mut options = ListingOptions {
        limit: 2,
        ..ListingOptions::default()
    };
    let first = engine.list("stellar", &filters, options)?;
    assert_eq!(first.plan.mode, Mode::Passthrough);
    assert_eq!(first.total, 4);
    assert_eq!(ids(&first), ["a", "b"]);
    assert_eq!(ids(&engine.list("stellar", &filters, options)?), ["a", "b"]);
    options.page = 2;
    assert_eq!(ids(&engine.list("stellar", &filters, options)?), ["c", "d"]);
    options.freshness = Freshness::Offline;
    let offline = engine.list("stellar", &filters, options)?;
    assert!(!offline.partial);
    assert_eq!(ids(&offline), ["c", "d"]);
    assert_eq!(offline.data[0]["unknownServerField"]["kept"], true);
    source.complete();
    Ok(())
}

#[test]
fn completed_full_sync_marks_departures_only_in_its_scope() -> Result<()> {
    let mut store = Store::memory()?;
    let a = issue("a", 5, false, RECENT);
    let b = issue("b", 5, false, RECENT);
    let a_with_pr = issue("a", 5, true, RECENT);
    let source = Fixture::new([
        Reply::Program,
        page(1, None, None, 2, vec![a.clone()]),
        page(100, None, None, 2, vec![a, b.clone()]),
        page(1, None, None, 1, vec![a_with_pr.clone()]),
        page(100, None, None, 1, vec![a_with_pr]),
        page(1, None, None, 1, vec![b.clone()]),
        page(100, None, None, 1, vec![b]),
    ]);
    let absent = Filter {
        has_pr: Some(false),
        ..Filter::default()
    };
    let present = Filter {
        has_pr: Some(true),
        ..Filter::default()
    };
    let scope = ListingOptions {
        sync: true,
        ..ListingOptions::default()
    };
    let mut engine = Engine::new(&mut store, &source, EngineConfig::default());
    assert_eq!(ids(&engine.list("stellar", &absent, scope)?), ["a", "b"]);
    assert_eq!(ids(&engine.list("stellar", &present, scope)?), ["a"]);
    assert_eq!(ids(&engine.list("stellar", &absent, scope)?), ["b"]);
    assert_eq!(
        ids(&engine.list(
            "stellar",
            &absent,
            ListingOptions {
                full: true,
                ..scope
            }
        )?),
        ["b"]
    );
    assert_eq!(ids(&engine.list("stellar", &present, scope)?), ["a"]);
    source.complete();
    assert_eq!(store.stats()?.stale_memberships, 1);
    assert!(store.cached_issue("a")?.is_some());
    Ok(())
}

#[test]
fn points_sort_applies_after_all_pages_and_deduplicates() -> Result<()> {
    let mut store = Store::memory()?;
    let a = issue("a", 10, false, RECENT);
    let source = Fixture::new([
        Reply::Program,
        page(1, None, None, 101, vec![a.clone()]),
        page(100, None, Some("next"), 101, vec![a.clone()]),
        page(
            100,
            Some("next"),
            None,
            101,
            vec![
                a,
                issue("z", 500, false, RECENT),
                issue("c", 500, false, RECENT),
            ],
        ),
    ]);
    let mut engine = Engine::new(&mut store, &source, EngineConfig::default());
    let mut options = ListingOptions {
        sort: Sort::Points,
        limit: 1,
        ..ListingOptions::default()
    };
    let result = engine.list("stellar", &Filter::default(), options)?;
    assert_eq!(result.plan.mode, Mode::ScopedSync);
    assert_eq!(result.total, 3);
    assert_eq!(ids(&result), ["c"]);
    options.page = 2;
    assert_eq!(
        ids(&engine.list("stellar", &Filter::default(), options)?),
        ["z"]
    );
    source.complete();
    Ok(())
}

#[test]
fn interrupted_full_sync_resumes_before_marking_departures_stale() -> Result<()> {
    let mut store = Store::memory()?;
    let a = issue("a", 10, false, RECENT);
    let b = issue("b", 10, false, RECENT);
    let source = Fixture::new([
        Reply::Program,
        page(1, None, None, 2, vec![a.clone()]),
        page(100, None, None, 2, vec![a, b.clone()]),
        page(1, None, None, 100, vec![b.clone()]),
        page(100, None, Some("resume-here"), 101, vec![b]),
        page(
            100,
            Some("resume-here"),
            None,
            101,
            vec![issue("c", 20, false, RECENT)],
        ),
    ]);
    let filters = Filter {
        pending: Some(Bounds {
            min: Some(0),
            max: Some(0),
        }),
        ..Filter::default()
    };
    let initial = EngineConfig::default();
    let mut options = ListingOptions::default();
    let result =
        Engine::new(&mut store, &source, initial.clone()).list("stellar", &filters, options)?;
    assert_eq!(ids(&result), ["a", "b"]);
    options.full = true;
    let constrained = EngineConfig {
        max_requests: 2,
        ..initial.clone()
    };
    let result =
        Engine::new(&mut store, &source, constrained).list("stellar", &filters, options)?;
    assert!(result.partial);
    assert_eq!(ids(&result), ["b"]);
    assert_eq!(store.stats()?.stale_memberships, 0);
    options.full = false;
    let resumed = Engine::new(&mut store, &source, initial).list("stellar", &filters, options)?;
    assert!(!resumed.partial);
    assert_eq!(ids(&resumed), ["b", "c"]);
    assert_eq!(store.stats()?.stale_memberships, 1);
    source.complete();
    Ok(())
}

#[test]
fn incremental_sync_uses_full_overlap_pages_and_keeps_newest_watermark() -> Result<()> {
    let mut store = Store::memory()?;
    let a = issue("a", 10, false, RECENT);
    let older = "2026-11-24T15:49:59Z";
    let mut without_time = issue("no-time", 10, false, older);
    without_time
        .as_object_mut()
        .expect("issue object")
        .remove("updatedAt");
    let source = Fixture::new([
        Reply::Program,
        page(1, None, None, 1, vec![a.clone()]),
        page(100, None, None, 1, vec![a.clone()]),
        page(
            100,
            None,
            Some("continue"),
            5,
            vec![
                issue("old-but-in-mixed-page", 10, false, older),
                issue("new", 10, false, "2026-11-24T16:01:00Z"),
                a,
            ],
        ),
        page(
            100,
            Some("continue"),
            Some("no-timestamp"),
            5,
            vec![without_time],
        ),
        page(
            100,
            Some("no-timestamp"),
            Some("not-fetched"),
            5,
            vec![issue("too-old", 10, false, older)],
        ),
    ]);
    let config = EngineConfig {
        listing_ttl: Duration::ZERO,
        ..EngineConfig::default()
    };
    let options = ListingOptions {
        sync: true,
        ..ListingOptions::default()
    };
    let mut engine = Engine::new(&mut store, &source, config);
    engine.list("stellar", &Filter::default(), options)?;
    let result = engine.list("stellar", &Filter::default(), options)?;
    assert!(!result.partial);
    assert_eq!(
        ids(&result),
        ["new", "a", "old-but-in-mixed-page", "no-time"]
    );
    source.complete();
    Ok(())
}

#[test]
fn bounded_first_scope_probe_reports_cost_without_broad_scan() -> Result<()> {
    let mut store = Store::memory()?;
    let source = Fixture::new([Reply::Program, page(1, None, None, 9_000, Vec::new())]);
    let mut engine = Engine::new(&mut store, &source, EngineConfig::default());
    let filter = Filter {
        assigned: Some(false),
        ..Filter::default()
    };
    let options = ListingOptions::default();
    let error = engine.list("stellar", &filter, options).unwrap_err();
    assert!(error.to_string().contains("90 page requests"));
    assert_eq!(
        engine
            .explain("stellar", &filter, options)?
            .estimated_requests,
        Some(90)
    );
    source.complete();
    Ok(())
}

#[test]
fn discovery_pages_preserve_unread_matches_without_resending_delivered_rows() -> Result<()> {
    let mut store = Store::memory()?;
    let a = issue("a", 1, false, RECENT);
    let b = issue("b", 2, false, RECENT);
    let c = issue("c", 3, false, RECENT);
    let d = issue("d", 4, false, RECENT);
    let source = Fixture::new([
        Reply::Program,
        page(1, None, None, 1, vec![a.clone()]),
        page(100, None, None, 1, vec![a.clone()]),
        Reply::Program,
        page(100, None, None, 4, vec![a, b, c, d]),
    ]);
    let mut engine = Engine::new(&mut store, &source, EngineConfig::default());
    let filter = Filter::default();
    let mut options = ListingOptions {
        new_since_last: true,
        sort: Sort::Points,
        limit: 1,
        ..ListingOptions::default()
    };
    let baseline = engine.list("stellar", &filter, options)?;
    assert_eq!(baseline.total, 0);
    assert!(!baseline.partial);
    options.freshness = Freshness::Fresh;
    let report = engine.list("stellar", &filter, options)?;
    assert_eq!(report.total, 3);
    assert_eq!(ids(&report), ["d"]);
    options.freshness = Freshness::Default;
    options.page = 2;
    assert_eq!(ids(&engine.list("stellar", &filter, options)?), ["c"]);
    assert!(engine.list("stellar", &filter, options)?.data.is_empty());
    options.page = 3;
    assert_eq!(ids(&engine.list("stellar", &filter, options)?), ["b"]);
    options.page = 1;
    assert_eq!(engine.list("stellar", &filter, options)?.total, 0);
    source.complete();
    Ok(())
}

#[test]
fn github_reference_resolves_via_title_but_requires_exact_repo_and_number() -> Result<()> {
    const ID: &str = "76031d64-bd10-4ced-a700-79b271796be9";
    let mut store = Store::memory()?;
    let expected = issue(ID, 9, false, RECENT);
    let mut wrong_repo = issue("another-repo", 10, false, RECENT);
    wrong_repo["repo"]["gitHubRepoFullName"] = json!("other/repo");
    let mut wrong_number = issue("another-number", 10, false, RECENT);
    wrong_number["gitHubIssueNumber"] = json!(6);
    let source = Fixture::new([
        Reply::Program,
        Reply::Title("title discovered from GitHub".into()),
        Reply::Listing {
            limit: 20,
            cursor: None,
            search: Some("title discovered from GitHub"),
            page: Page {
                data: vec![wrong_repo, wrong_number, expected.clone()],
                total: Some(3),
                next_cursor: None,
            },
        },
        Reply::Issue(None),
    ]);
    let mut engine = Engine::new(&mut store, &source, EngineConfig::default());
    let offline = engine.issue("owner/repo#7", Freshness::Offline, "stellar")?;
    assert!(offline.data.is_none());
    assert!(offline.partial);
    let result = engine.issue(
        "https://github.com/OWNER/repo/issues/7?ignored#anchor",
        Freshness::Default,
        "stellar",
    )?;
    assert_eq!(result.data, Some(expected.clone()));
    assert_eq!(
        engine
            .issue("owner/repo#7", Freshness::Default, "stellar")?
            .data,
        Some(expected)
    );
    let missing = engine.issue(ID, Freshness::Fresh, "stellar")?;
    assert!(missing.data.is_none());
    assert!(!missing.partial);
    assert!(!engine.issue(ID, Freshness::Default, "stellar")?.partial);
    source.complete();
    Ok(())
}

#[test]
fn offline_cold_and_stale_queries_expose_partial_and_as_of() -> Result<()> {
    let mut store = Store::memory()?;
    let source = Fixture::new([
        Reply::Program,
        page(20, None, None, 1, vec![issue("a", 1, false, RECENT)]),
    ]);
    let config = EngineConfig {
        listing_ttl: Duration::ZERO,
        ..EngineConfig::default()
    };
    let mut engine = Engine::new(&mut store, &source, config);
    let filters = Filter::default();
    let offline = ListingOptions {
        freshness: Freshness::Offline,
        ..ListingOptions::default()
    };
    let cold = engine.list("stellar", &filters, offline)?;
    assert!(cold.partial);
    assert!(cold.as_of.is_none());
    let fetched = engine.list("stellar", &filters, ListingOptions::default())?;
    let stale = engine.list("stellar", &filters, offline)?;
    assert!(!stale.partial);
    assert!(stale
        .warnings
        .iter()
        .any(|warning| warning.contains("stale")));
    assert_eq!(stale.as_of, fetched.as_of);
    assert_eq!(ids(&stale), ["a"]);
    source.complete();
    assert_eq!(store.prune(Utc::now() + TimeDelta::days(1))?.issues, 1);
    Ok(())
}
