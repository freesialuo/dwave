use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, ETAG, RETRY_AFTER};
use reqwest::Url;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;
use wave_core::gateway::{
    BudgetExhausted, Fetched, Gateway, GetRequest, HostPolicy, HttpResponse, HttpStatus,
    HttpTransport, Metrics, TransportError,
};
use wave_core::source::{HttpWaveSource, RemoteQuery, WaveSource};

struct RequestLog {
    url: Url,
    etag: Option<String>,
    bearer_token: Option<String>,
}

type Outcome = std::result::Result<HttpResponse, TransportError>;

#[derive(Clone)]
struct FixtureTransport {
    outcomes: Arc<Mutex<VecDeque<Outcome>>>,
    requests: Arc<Mutex<Vec<RequestLog>>>,
}

impl FixtureTransport {
    fn new(outcomes: impl IntoIterator<Item = Outcome>) -> Self {
        Self {
            outcomes: Arc::new(Mutex::new(outcomes.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn gateway(&self, limit: u32) -> Arc<Gateway> {
        Arc::new(
            Gateway::with_transport(
                limit,
                HostPolicy {
                    burst: 20,
                    retry_base: Duration::ZERO,
                    ..HostPolicy::default()
                },
                self.clone(),
            )
            .expect("valid gateway policy"),
        )
    }
}

impl HttpTransport for FixtureTransport {
    fn get(&self, req: &GetRequest, url: &Url, _: Duration) -> Outcome {
        self.requests.lock().unwrap().push(RequestLog {
            url: url.to_owned(),
            etag: req.etag.clone(),
            bearer_token: req.bearer_token.clone(),
        });
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected outbound request")
    }
}

fn response(status: u16, body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        headers: HeaderMap::new(),
        body: body.to_vec(),
    }
}

fn json_response(body: &Value) -> Outcome {
    Ok(response(200, &serde_json::to_vec(body).unwrap()))
}

#[test]
fn recorded_wave_shapes_and_encoded_query_round_trip() -> Result<()> {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/wave_api.json"))?;
    let transport = FixtureTransport::new([
        json_response(&fixture["program"]),
        json_response(&fixture["waves"]),
        json_response(&fixture["issues"]),
        json_response(&fixture["issue"]),
        Ok(response(404, b"")),
    ]);
    let gateway = transport.gateway(5);
    let source = HttpWaveSource::with_base_url(gateway.clone(), "https://wave.example/")?;

    let program = source.program("stellar")?;
    let program_id = program["id"].as_str().unwrap();
    let waves = source.waves(program_id, 400)?;
    assert_eq!(
        waves,
        fixture["waves"]["data"].as_array().unwrap().as_slice()
    );
    let page = source.issues_page(
        &RemoteQuery {
            program_id: program_id.into(),
            state: Some("open".into()),
            has_applications: Some(false),
            has_pr: Some(false),
            search: Some("bug+ & test".into()),
            repo_id: Some("repo-id".into()),
            sort_by: Some("updatedAt".into()),
            sort_order: Some("desc".into()),
            limit: 300,
        },
        Some("previous+CURSOR/="),
    )?;
    assert_eq!(
        page.total,
        fixture["issues"]["pagination"]["total"].as_u64()
    );
    assert_eq!(page.next_cursor.as_deref(), Some("opaque+/=cursor"));
    assert_eq!(
        page.data,
        fixture["issues"]["data"].as_array().unwrap().as_slice()
    );
    let issue_id = page.data[0]["id"].as_str().unwrap();
    assert_eq!(source.issue(issue_id)?, Some(fixture["issue"].clone()));
    assert_eq!(source.issue("missing")?, None);
    assert_eq!(source.remaining_requests(), Some(0));

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].url.path(), "/api/wave-programs/stellar");
    assert_eq!(
        requests[1].url.path(),
        format!("/api/wave-programs/{program_id}/waves")
    );
    assert_eq!(requests[1].url.query(), Some("limit=100"));
    assert_eq!(requests[2].url.path(), "/api/issues");
    let observed: HashMap<_, _> = requests[2].url.query_pairs().into_owned().collect();
    let expected = [
        ("waveProgramId", program_id),
        ("limit", "100"),
        ("state", "open"),
        ("hasApplications", "false"),
        ("hasPr", "false"),
        ("search", "bug+ & test"),
        ("repoId", "repo-id"),
        ("sortBy", "updatedAt"),
        ("sortOrder", "desc"),
        ("cursor", "previous+CURSOR/="),
    ]
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .into_iter()
    .collect();
    assert_eq!(observed, expected);
    assert_eq!(requests[3].url.path(), format!("/api/issues/{issue_id}"));
    Ok(())
}

#[test]
fn waves_accepts_listing_without_pagination() -> Result<()> {
    let wave = json!({"id":"wave-id","waveNumber": 2});
    let transport = FixtureTransport::new([json_response(&json!({"data": [wave]}))]);
    let source = HttpWaveSource::new(transport.gateway(1));
    assert_eq!(source.waves("program-id", 1)?, vec![wave]);
    Ok(())
}

#[test]
fn future_issue_fields_and_states_survive_without_optional_fields() -> Result<()> {
    let record = json!({
        "id": "example-id",
        "state": "future-state",
        "newProviderField": {"unknown": true},
    });
    let transport = FixtureTransport::new([json_response(&record)]);
    let source = HttpWaveSource::new(transport.gateway(1));
    assert_eq!(source.issue("example-id")?, Some(record));
    Ok(())
}

#[test]
fn pagination_refuses_to_report_completion_when_cursor_is_missing() {
    let transport = FixtureTransport::new([json_response(&json!({
        "data": [{"id":"example-id"}],
        "pagination": {"hasNextPage": true}
    }))]);
    let source = HttpWaveSource::new(transport.gateway(1));
    let result = source.issues_page(
        &RemoteQuery {
            program_id: "program-id".into(),
            limit: 100,
            ..RemoteQuery::default()
        },
        None,
    );
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("omitted its cursor"));
}

#[test]
fn issue_pagination_requires_completion_metadata_and_honors_last_page() -> Result<()> {
    let transport = FixtureTransport::new([
        json_response(&json!({"data": []})),
        json_response(&json!({"data": [], "pagination": {}})),
        json_response(&json!({"data": [], "pagination": {
            "hasNextPage": false, "nextCursor": "stale-cursor"
        }})),
    ]);
    let source = HttpWaveSource::new(transport.gateway(3));
    let query = RemoteQuery {
        program_id: "program-id".into(),
        limit: 100,
        ..RemoteQuery::default()
    };

    assert!(source.issues_page(&query, None).is_err());
    assert!(source.issues_page(&query, None).is_err());
    assert_eq!(source.issues_page(&query, None)?.next_cursor, None);
    Ok(())
}

#[test]
fn cold_github_title_lookup_spends_one_request_and_ignores_prs() -> Result<()> {
    let transport = FixtureTransport::new([
        json_response(&json!({"title": "Wave search title", "unknownField": 7})),
        json_response(&json!({"title": "This is a PR", "pull_request": {}})),
    ]);
    let gateway = transport.gateway(2);
    let source = HttpWaveSource::new(gateway.clone());
    assert_eq!(
        source.github_issue_title("Owner/Repo", 177)?.as_deref(),
        Some("Wave search title"),
    );
    assert_eq!(gateway.remaining(), 1);
    assert_eq!(source.github_issue_title("Owner/Repo", 178)?, None);
    assert_eq!(gateway.remaining(), 0);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].url.host_str(), Some("api.github.com"));
    assert_eq!(requests[0].url.path(), "/repos/Owner/Repo/issues/177");
    assert_eq!(requests[0].bearer_token, None);
    Ok(())
}

#[test]
fn retries_across_hosts_share_budget_with_conditional_requests() -> Result<()> {
    let mut not_modified = response(304, b"");
    not_modified
        .headers
        .insert(ETAG, HeaderValue::from_static("\"old\""));
    let transport = FixtureTransport::new([
        Ok(response(503, b"")),
        Ok(response(200, b"{}")),
        Ok(not_modified),
    ]);
    let gateway = transport.gateway(3);

    let wave: Fetched<Value> = gateway.get_json(GetRequest::new("https://wave.example/issues"))?;
    assert_eq!(wave.body, Some(json!({})));
    let mut conditional = GetRequest::new("https://api.github.com/repos/example/demo/issues/177");
    conditional.etag = Some("\"old\"".into());
    conditional.bearer_token = Some("fixture-token".into());
    let fetched: Fetched<Value> = gateway.get_json(conditional)?;
    assert_eq!(fetched.status, 304);
    assert_eq!(fetched.etag.as_deref(), Some("\"old\""));
    assert_eq!(fetched.body, None);
    let refused = gateway.get_json::<Value>(GetRequest::new("https://wave.example/issues"));
    assert!(refused.unwrap_err().is::<BudgetExhausted>());
    assert_eq!(
        gateway.metrics(),
        Metrics {
            requests: 3,
            retries: 1,
            not_modified: 1,
            bytes: 2,
        }
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].url, requests[0].url);
    assert_eq!(requests[2].etag.as_deref(), Some("\"old\""));
    assert_eq!(requests[2].bearer_token.as_deref(), Some("fixture-token"));
    Ok(())
}

#[test]
fn a_pending_retry_reports_budget_exhaustion_without_sending_again() {
    let transport = FixtureTransport::new([Err(TransportError {
        error: anyhow::anyhow!("connection test"),
        retryable: true,
    })]);
    let gateway = transport.gateway(1);
    let failure = gateway
        .get_json::<Value>(GetRequest::new("https://wave.example/issues"))
        .unwrap_err();
    assert!(failure.is::<BudgetExhausted>());
    assert_eq!(gateway.metrics().requests, 1);
    assert_eq!(gateway.metrics().retries, 0);
}

#[test]
fn long_retry_after_cools_down_only_the_affected_host() -> Result<()> {
    let mut limited = response(429, b"");
    limited
        .headers
        .insert(RETRY_AFTER, HeaderValue::from_static("300"));
    let transport = FixtureTransport::new([Ok(limited), Ok(response(200, b"{}"))]);
    let gateway = transport.gateway(4);
    for _ in 0..2 {
        let failure = gateway
            .get_json::<Value>(GetRequest::new("https://wave.example/issues"))
            .unwrap_err();
        let http = failure.downcast_ref::<HttpStatus>().unwrap();
        assert_eq!(http.status, 429);
        assert!(http.retry_after.unwrap() > Duration::from_secs(30));
    }
    assert_eq!(gateway.metrics().requests, 1);
    let other = gateway.get_json::<Value>(GetRequest::new("https://another.example/issues"))?;
    assert_eq!(other.status, 200);
    assert_eq!(gateway.remaining(), 2);
    Ok(())
}

#[test]
fn simultaneous_calls_cannot_spend_the_same_budget_slot() {
    let transport = FixtureTransport::new([Ok(response(200, b"{}"))]);
    let gateway = transport.gateway(1);
    let barrier = Arc::new(Barrier::new(2));
    let results: Vec<_> = ["https://a.example/issues", "https://b.example/issues"]
        .into_iter()
        .map(|url| {
            let gateway = gateway.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                gateway.get_json::<Value>(GetRequest::new(url))
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .filter(|error| error.is::<BudgetExhausted>())
            .count(),
        1,
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[test]
#[ignore = "opt-in smoke test against the live Wave API"]
fn live_wave_smoke() -> Result<()> {
    let gateway = Arc::new(Gateway::new(4, HostPolicy::default())?);
    let source = HttpWaveSource::new(gateway.clone());
    let program = source.program("stellar")?;
    let program_id = program["id"].as_str().unwrap();
    assert!(!source.waves(program_id, 1)?.is_empty());
    let issues = source.issues_page(
        &RemoteQuery {
            program_id: program_id.into(),
            state: Some("open".into()),
            sort_by: Some("updatedAt".into()),
            limit: 1,
            ..RemoteQuery::default()
        },
        None,
    )?;
    assert_eq!(issues.data.len(), 1);
    let issue_id = issues.data[0]["id"].as_str().unwrap();
    assert!(source.issue(issue_id)?.is_some());
    assert_eq!(gateway.metrics().requests, 4);
    Ok(())
}
