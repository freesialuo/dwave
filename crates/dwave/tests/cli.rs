use chrono::Utc;
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};
use wave_core::store::Store;

const ISSUE_ID: &str = "a4771015-3300-426d-a6e1-608ac2cab9f3";
const MISSING_ID: &str = "b5771015-3300-426d-a6e1-608ac2cab9f3";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        // Cargo retains the small SQLite and TOML fixtures inside its target directory.
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dwave"));
        command
            .current_dir(&self.root)
            .env("DWAVE_CONFIG", self.root.join("config.toml"))
            .env("DWAVE_DB", self.root.join("cache.db"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn store(&self) -> Store {
        Store::open(self.root.join("cache.db")).unwrap()
    }

    fn write_issue(&self, pending: u32) {
        self.store()
            .upsert_issue(
                &json!({
                    "id": ISSUE_ID,
                    "waveProgramId": "stellar-id",
                    "title": "Improve documentation",
                    "state": "open",
                    "pendingApplicationsCount": pending,
                    "points": 500,
                    "prLink": null,
                    "assignedApplicant": null,
                    "gitHubIssueNumber": 77,
                    "repo": { "gitHubRepoFullName": "org/project" },
                    "extraField": "preserved"
                }),
                Utc::now(),
            )
            .unwrap();
    }
}

fn assert_status(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout={}; stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn envelope(out: &Output, code: i32) -> Value {
    assert_status(out, code);
    let output: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(output["schema"], "dwave/v1");
    assert_eq!(output["meta"]["requests"], 0);
    assert!(output["meta"]["partial"].is_boolean());
    assert!(output["meta"]["warnings"].is_array());
    output
}

#[test]
fn validates_filters_and_global_arguments_as_usage_errors() {
    let fixture = Fixture::new("invalid-args");
    for args in [
        &["issues", "--fresh", "--offline"][..],
        &["sync", "--offline", "--full"],
        &["issue", "show", "invalid", "--offline"],
        &["issues", "--json", "--ndjson"],
        &["issues", "--offline", "--limit", "0"],
        &["issues", "--pending", "8..=3"],
        &["issues", "--has-pr", "--no-pr"],
        &["issues", "--min-points", "100", "--max-points", "60"],
        &["issues", "--preset", "repo", "--explain"],
        &["config", "set", "max_requests", "0"],
        &["config", "set", "wave_rate", "1e-100"],
    ] {
        let out = fixture.run(args);
        assert_status(&out, 2);
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("error:"));
    }
}

#[test]
fn persists_defaults_and_presets_and_applies_explicit_overrides() {
    let fixture = Fixture::new("config-presets");
    let rate = envelope(&fixture.run(&["--json", "config", "get", "wave_rate"]), 0);
    assert_eq!(rate["data"]["value"], 2.0);
    let set_rate = envelope(
        &fixture.run(&["config", "set", "wave_rate", "3.5", "--json"]),
        0,
    );
    assert_eq!(set_rate["data"]["value"], 3.5);
    let program = envelope(&fixture.run(&["program", "use", "other-wave", "--json"]), 0);
    assert_eq!(program["meta"]["program"], "other-wave");
    let threshold = envelope(
        &fixture.run(&["config", "set", "high_value_min_points", "250", "--json"]),
        0,
    );
    assert_eq!(threshold["data"]["value"], 250);
    let save = envelope(
        &fixture.run(&[
            "preset",
            "save",
            "triage",
            "--state",
            "all",
            "--pending",
            "..=2",
            "--no-pr",
            "--repo",
            "org/project",
            "--json",
        ]),
        0,
    );
    assert!(save["data"][0]["filter"]["state"].is_null());
    let list = envelope(&fixture.run(&["preset", "list", "--json"]), 0);
    let presets = list["data"].as_array().unwrap();
    let get = |name| {
        presets
            .iter()
            .find(|preset| preset["name"] == name)
            .unwrap()
    };
    assert_eq!(get("high-value")["filter"]["points"]["min"], 250);
    assert_eq!(get("triage")["filter"]["pending"]["max"], 2);
    assert!(get("triage")["filter"]["state"].is_null());

    let plan = envelope(
        &fixture.run(&[
            "issues",
            "--preset",
            "triage",
            "--state",
            "open",
            "--has-pr",
            "--explain",
            "--offline",
            "--json",
        ]),
        0,
    );
    assert_eq!(plan["meta"]["program"], "other-wave");
    assert_eq!(plan["data"]["mode"], "scoped_sync");
    assert_eq!(plan["data"]["remote"]["state"], "open");
    assert_eq!(plan["data"]["remote"]["has_pr"], true);
    let default_plan = envelope(
        &fixture.run(&["issues", "--program", "stellar", "--explain", "--json"]),
        0,
    );
    assert_eq!(default_plan["meta"]["program"], "stellar");
    assert_eq!(default_plan["data"]["mode"], "passthrough");

    assert_status(&fixture.run(&["preset", "rm", "untouched"]), 0);
    assert_status(
        &fixture.run(&["issues", "--preset", "untouched", "--explain"]),
        2,
    );
    let alternate = envelope(
        &fixture.run(&[
            "--config",
            "another.toml",
            "config",
            "get",
            "default_program",
            "--json",
        ]),
        0,
    );
    assert_eq!(alternate["data"]["value"], "stellar");
}

#[test]
fn emits_one_ndjson_envelope_per_preset() {
    let fixture = Fixture::new("ndjson");
    let output = fixture.run(&["preset", "list", "--ndjson"]);
    assert_status(&output, 0);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let records: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 5);
    for record in records {
        assert_eq!(record["schema"], "dwave/v1");
        assert_eq!(record["meta"]["requests"], 0);
        assert_eq!(record["data"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn watch_reports_confirmed_changes_and_partial_errors_with_json() {
    let fixture = Fixture::new("watch");
    fixture.write_issue(0);
    let issue = envelope(
        &fixture.run(&["issue", "show", ISSUE_ID, "--offline", "--json"]),
        0,
    );
    assert_eq!(issue["data"]["extraField"], "preserved");
    assert_eq!(issue["data"]["pendingApplicationsCount"], 0);
    let add = envelope(
        &fixture.run(&[
            "watch",
            "add",
            ISSUE_ID,
            "--note",
            "Follow up",
            "--offline",
            "--json",
        ]),
        0,
    );
    assert_eq!(add["data"][0]["note"], "Follow up");
    let baseline = envelope(&fixture.run(&["watch", "check", "--offline", "--json"]), 0);
    assert_eq!(baseline["data"], json!([]));
    assert_eq!(baseline["meta"]["partial"], false);

    fixture
        .store()
        .watch_add(MISSING_ID, None, Utc::now())
        .unwrap();
    fixture.write_issue(1);
    let changed = envelope(&fixture.run(&["watch", "check", "--offline", "--json"]), 10);
    assert_eq!(changed["meta"]["partial"], true);
    assert_eq!(changed["data"][0]["issue_id"], ISSUE_ID);
    assert_eq!(changed["data"][0]["type"], "applications_changed");
    assert_eq!(changed["data"][0]["from"], 0);
    assert_eq!(changed["data"][0]["to"], 1);

    let incomplete = envelope(&fixture.run(&["watch", "check", "--offline", "--json"]), 1);
    assert_eq!(incomplete["data"], json!([]));
    assert_eq!(incomplete["meta"]["partial"], true);
    assert!(incomplete["meta"]["warnings"][0]
        .as_str()
        .unwrap()
        .contains(MISSING_ID));

    let history = envelope(&fixture.run(&["watch", "log", ISSUE_ID, "--json"]), 0);
    assert_eq!(
        history["data"][0]["changes"][0]["type"],
        "applications_changed"
    );
    let remove = envelope(
        &fixture.run(&["watch", "rm", MISSING_ID, "--offline", "--json"]),
        0,
    );
    assert_eq!(remove["data"][0]["removed"], true);
}

#[test]
fn issue_show_keeps_wave_data_when_github_identity_is_invalid() {
    let fixture = Fixture::new("invalid-github-identity");
    fixture
        .store()
        .upsert_issue(
            &json!({
                "id": ISSUE_ID,
                "title": "Wave issue with malformed repository metadata",
                "gitHubIssueNumber": 77,
                "repo": { "gitHubRepoFullName": "not an owner/repo" }
            }),
            Utc::now(),
        )
        .unwrap();

    let issue = envelope(
        &fixture.run(&["issue", "show", ISSUE_ID, "--github", "--offline", "--json"]),
        0,
    );
    assert_eq!(issue["data"]["id"], ISSUE_ID);
    assert_eq!(issue["meta"]["partial"], true);
    assert!(issue["data"]["github"].is_null());
    assert!(issue["meta"]["warnings"][0]
        .as_str()
        .unwrap()
        .contains("GitHub"));
}

#[test]
fn renders_offline_program_waves_and_issue_pages_from_the_shared_store() {
    use wave_core::{
        engine::{Engine, EngineConfig, Freshness, ListingOptions},
        filter::Filter,
        source::{Issue, Page, Program, RemoteQuery, Wave, WaveSource},
    };

    struct Source {
        count: usize,
    }
    impl WaveSource for Source {
        fn program(&self, slug: &str) -> anyhow::Result<Program> {
            Ok(json!({ "id": "stellar-id", "slug": slug }))
        }

        fn waves(&self, _: &str, _: u32) -> anyhow::Result<Vec<Wave>> {
            Ok(vec![
                json!({ "id": "wave-5", "waveNumber": 5, "status": "completed" }),
                json!({ "id": "wave-6", "waveNumber": 6, "status": "active" }),
            ])
        }

        fn issues_page(
            &self,
            query: &RemoteQuery,
            cursor: Option<&str>,
        ) -> anyhow::Result<Page<Issue>> {
            let issues = [
                json!({
                    "id": ISSUE_ID, "state": "open", "title": "First issue",
                    "waveProgramId": "stellar-id", "updatedAt": "2026-02-20T00:00:00Z",
                    "gitHubIssueNumber": 77, "repo": {"gitHubRepoFullName": "org/project"},
                }),
                json!({
                    "id": MISSING_ID, "state": "open", "title": "Second issue",
                    "waveProgramId": "stellar-id", "updatedAt": "2026-02-19T00:00:00Z",
                }),
            ];
            let offset: usize = cursor.unwrap_or("0").parse()?;
            let data: Vec<_> = issues
                .into_iter()
                .take(self.count)
                .skip(offset)
                .take(query.limit as usize)
                .collect();
            let end = offset + data.len();
            Ok(Page {
                data,
                total: Some(self.count as u64),
                next_cursor: (end < self.count).then(|| end.to_string()),
            })
        }

        fn issue(&self, _: &str) -> anyhow::Result<Option<Issue>> {
            anyhow::bail!("fixture has no single-issue endpoint")
        }
    }

    let fixture = Fixture::new("listing");
    {
        let mut store = fixture.store();
        let mut engine = Engine::new(&mut store, &Source { count: 2 }, EngineConfig::default());
        engine
            .list(
                "stellar",
                &Filter::default(),
                ListingOptions {
                    limit: 1,
                    page: 2,
                    ..ListingOptions::default()
                },
            )
            .unwrap();
        engine.waves("stellar-id", Freshness::Fresh).unwrap();
    }

    let current = envelope(
        &fixture.run(&["waves", "--current", "--offline", "--json"]),
        0,
    );
    assert_eq!(current["data"].as_array().unwrap().len(), 1);
    assert_eq!(current["data"][0]["waveNumber"], 6);

    let program = envelope(
        &fixture.run(&[
            "program",
            "show",
            "stellar",
            "--program",
            "ignore-me",
            "--offline",
            "--json",
        ]),
        0,
    );
    assert_eq!(program["data"]["id"], "stellar-id");
    assert_eq!(program["meta"]["program"], "stellar");

    let issue_page = envelope(
        &fixture.run(&[
            "issues",
            "--limit",
            "1",
            "--page",
            "2",
            "--offline",
            "--json",
        ]),
        0,
    );
    assert_eq!(
        issue_page["page"],
        json!({ "page": 2, "limit": 1, "total": 2 })
    );
    assert_eq!(issue_page["data"][0]["title"], "Second issue");
    assert_eq!(issue_page["meta"]["partial"], false);

    let update_scope = |count| {
        let mut store = fixture.store();
        Engine::new(&mut store, &Source { count }, EngineConfig::default())
            .list(
                "stellar",
                &Filter::default(),
                ListingOptions {
                    full: true,
                    ..ListingOptions::default()
                },
            )
            .unwrap();
    };
    update_scope(1);
    let baseline = envelope(
        &fixture.run(&["issues", "--new-since-last", "--offline", "--json"]),
        0,
    );
    assert_eq!(baseline["data"], json!([]));
    assert_eq!(baseline["meta"]["partial"], false);

    update_scope(2);
    let out_of_range = envelope(
        &fixture.run(&[
            "issues",
            "--new-since-last",
            "--page",
            "9",
            "--offline",
            "--json",
        ]),
        0,
    );
    assert_eq!(out_of_range["page"]["total"], 1);
    assert_eq!(out_of_range["data"], json!([]));

    let delivered = envelope(
        &fixture.run(&["issues", "--new-since-last", "--offline", "--json"]),
        10,
    );
    assert_eq!(delivered["data"][0]["title"], "Second issue");
    let repeat = envelope(
        &fixture.run(&["issues", "--new-since-last", "--offline", "--json"]),
        0,
    );
    assert_eq!(repeat["data"], json!([]));

    let reference = "org/project#77";
    envelope(
        &fixture.run(&["watch", "add", reference, "--offline", "--json"]),
        0,
    );
    envelope(&fixture.run(&["watch", "check", "--offline", "--json"]), 0);
    fixture
        .store()
        .set_issue_missing(ISSUE_ID, Utc::now())
        .unwrap();
    let gone = envelope(&fixture.run(&["watch", "check", "--offline", "--json"]), 10);
    assert_eq!(gone["data"][0]["type"], "disappeared");
    let log = envelope(
        &fixture.run(&[
            "watch",
            "log",
            "https://github.com/Org/Project/issues/77",
            "--json",
        ]),
        0,
    );
    assert_eq!(log["data"][0]["issue_id"], ISSUE_ID);
    assert_eq!(log["data"][0]["changes"][0]["type"], "disappeared");
    let removed = envelope(
        &fixture.run(&["watch", "rm", reference, "--offline", "--json"]),
        0,
    );
    assert_eq!(
        removed["data"],
        json!([{ "issue_id": ISSUE_ID, "removed": true }])
    );
}
