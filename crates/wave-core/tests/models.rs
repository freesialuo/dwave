use serde_json::{json, Value};
use wave_core::models::{Issue, IssueState, Program, Repo, Wave, WaveStatus};
use wave_core::source::Page;

#[test]
fn projects_records_from_the_wire_fixture() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/wave_api.json")).unwrap();
    let program = Program::try_from(&fixture["program"]).unwrap();
    assert_eq!(program.slug.as_deref(), Some("stellar"));
    assert_eq!(program.name.as_deref(), Some("Stellar"));

    let wave = Wave::try_from(&fixture["waves"]["data"][0]).unwrap();
    assert_eq!(wave.wave_program_id.as_deref(), Some(program.id.as_str()));
    assert_eq!(wave.wave_number, Some(11));
    assert_eq!(wave.status, Some(WaveStatus::Active));
    assert_eq!(wave.budget_usd.as_deref(), Some("75000.00"));

    let page = Page {
        data: fixture["issues"]["data"].as_array().unwrap().clone(),
        total: Some(33413),
        next_cursor: Some("opaque+/=cursor".to_owned()),
    };
    let listing_issue = Issue::try_from(&page.data[0]).unwrap();
    assert_eq!(listing_issue, Issue::try_from(&fixture["issue"]).unwrap());
    assert_eq!(listing_issue.state, Some(IssueState::Open));
    assert_eq!(listing_issue.github_issue_id, Some(10));
    assert_eq!(listing_issue.github_issue_number, Some(177));
    assert_eq!(listing_issue.pending_applications_count, Some(0));
    assert_eq!(listing_issue.assigned_applicant, None);
    assert_eq!(page.data[0]["assignedApplicant"], Value::Null);
    let repo = listing_issue.repo.unwrap();
    assert_eq!(repo.github_repo_full_name.as_deref(), Some("example/demo"));
    assert_eq!(repo, Repo::try_from(&fixture["issue"]["repo"]).unwrap());
}

#[test]
fn preserves_unrecognized_values_through_projection_round_trips() {
    let issue_record = json!({
        "id": "issue-id",
        "state": "Reopened-Later",
        "repo": {"id": "repo-id", "newRepoField": {"nested": [53]}},
        "newIssueField": [false, null, {"key": "value"}]
    });
    let issue = Issue::try_from(&issue_record).unwrap();
    assert_eq!(
        issue.state,
        Some(IssueState::Unknown("Reopened-Later".to_owned()))
    );
    assert_eq!(issue.extra["newIssueField"], issue_record["newIssueField"]);
    assert_eq!(serde_json::to_value(&issue).unwrap(), issue_record);

    let wave_record = json!({"id": "wave-id", "status": "Awaiting_Payout", "newWaveField": null});
    let wave = Wave::try_from(&wave_record).unwrap();
    assert_eq!(
        wave.status,
        Some(WaveStatus::Unknown("Awaiting_Payout".to_owned()))
    );
    assert_eq!(serde_json::to_value(wave).unwrap(), wave_record);

    let program_record = json!({"id": "program-id", "newProgramField": {"flag": true}});
    let program = Program::try_from(&program_record).unwrap();
    assert_eq!(program.name, None);
    assert_eq!(serde_json::to_value(program).unwrap(), program_record);
}

#[test]
fn requires_identity_and_accepts_sparse_records() {
    assert!(Issue::try_from(&json!({"title": "Missing ID"})).is_err());
    assert!(Wave::try_from(&json!({"id": 4})).is_err());
    assert!(Program::try_from(&json!({"id": null})).is_err());
    assert!(Repo::try_from(&json!({})).is_err());

    let issue = Issue::try_from(&json!({"id": "issue-id", "state": null})).unwrap();
    assert_eq!(issue.id, "issue-id");
    assert_eq!(issue.state, None);
    assert_eq!(issue.repo, None);
}
