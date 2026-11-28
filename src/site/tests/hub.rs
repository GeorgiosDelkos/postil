//! End-to-end tests of the Agentic Developer Hub (`/api/hub*`), capability-tier lane previews
//! (`/api/lanes/preview`), and multi-stage execution pipelines (`/api/pipelines`, `/api/lane/*`).

use super::*;

const HUB: &str = "postil://localhost/api/hub";
const HUB_REPOS: &str = "postil://localhost/api/hub/repos";
const LANES_PREVIEW: &str = "postil://localhost/api/lanes/preview";
const LANE_START: &str = "postil://localhost/api/lane/start";
const LANE_APPROVE: &str = "postil://localhost/api/lane/gate/approve";
const PIPELINES: &str = "postil://localhost/api/pipelines";

fn wait_for_pipeline_status(
    site: &Site,
    nudges: &mpsc::Receiver<()>,
    pipeline_id: &str,
    target: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let list = get_json(site, PIPELINES);
        if let Some(found) = list
            .as_array()
            .and_then(|arr| arr.iter().find(|p| p["id"] == pipeline_id))
        {
            let status = found["status"].as_str().unwrap_or("");
            if status == target {
                return found.clone();
            }
            assert!(
                status != "failed" && status != "stopped",
                "pipeline {pipeline_id} ended unexpectedly in status {status:?}: {found}"
            );
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "timed out waiting for pipeline {pipeline_id} to reach {target}"
        );
        let _ = nudges
            .recv_timeout(left.min(Duration::from_millis(200)))
            .ok();
    }
}

#[test]
fn hub_monitored_repos_crud_and_lane_preview() {
    let repo = TempRepo::new("hub-crud");
    repo.write("a.txt", "hello\n");
    let (site, _nudges) = run_site(&repo, "echo");

    let add_body = serde_json::json!({
        "action": "add",
        "path": repo.root,
    })
    .to_string();
    let (status, body) = post(&site, HUB_REPOS, &add_body);
    assert_eq!(status, StatusCode::OK, "{body}");

    let hub = get_json(&site, HUB);
    assert_eq!(hub["repos"].as_array().unwrap().len(), 1);
    assert!(hub["ghAvailable"].is_boolean());

    let settings = get_json(&site, SETTINGS);
    assert!(settings["memory"]["backend"]["kind"].is_string());

    // Preview stage counts for Easy / Middle / Hard lanes.
    let easy = get_json(&site, &format!("{LANES_PREVIEW}?lane=easy&kind=issue"));
    assert_eq!(easy["stages"].as_array().unwrap().len(), 1);

    let middle = get_json(&site, &format!("{LANES_PREVIEW}?lane=middle&kind=issue"));
    assert_eq!(middle["stages"].as_array().unwrap().len(), 3);

    let hard_issue = get_json(&site, &format!("{LANES_PREVIEW}?lane=hard&kind=issue"));
    assert_eq!(hard_issue["stages"].as_array().unwrap().len(), 6);

    let hard_pr = get_json(
        &site,
        &format!("{LANES_PREVIEW}?lane=hard&kind=pullRequest"),
    );
    assert_eq!(hard_pr["stages"].as_array().unwrap().len(), 4);

    // Removing the monitored repo clears it from `/api/hub`.
    let remove_body = serde_json::json!({
        "action": "remove",
        "path": repo.root,
    })
    .to_string();
    let (status, body) = post(&site, HUB_REPOS, &remove_body);
    assert_eq!(status, StatusCode::OK, "{body}");

    let hub_after = get_json(&site, HUB);
    assert_eq!(hub_after["repos"].as_array().unwrap().len(), 0);
}

#[test]
fn easy_lane_issue_runs_single_stage_and_completes() {
    let repo = TempRepo::new("hub-easy-lane");
    repo.write("a.txt", "initial\n");
    let (site, nudges) = run_site(&repo, "writes");

    let start_body = serde_json::json!({
        "lane": "easy",
        "kind": "issue",
        "number": 42,
        "title": "Fix minor typo in README",
        "body": "Please fix typo.",
        "humanGate": false,
    })
    .to_string();
    let (status, body) = post(&site, LANE_START, &start_body);
    assert_eq!(status, StatusCode::OK, "{body}");
    let started: serde_json::Value = serde_json::from_str(&body).unwrap();
    let pipeline_id = started["id"].as_str().unwrap().to_owned();

    let finished = wait_for_pipeline_status(&site, &nudges, &pipeline_id, "completed");
    assert_eq!(finished["lane"], "easy");
    let stages = finished["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0]["kind"], "implement");
    assert_eq!(stages[0]["status"], "completed");
}

#[test]
fn hard_lane_issue_pauses_at_plan_gate_and_completes_after_approval() {
    let repo = TempRepo::new("hub-hard-lane");
    repo.write("a.txt", "initial\n");
    let (site, nudges) = run_site(&repo, "echo");

    let start_body = serde_json::json!({
        "lane": "hard",
        "kind": "issue",
        "number": 99,
        "title": "Refactor lock ordering in runner",
        "body": "Ensure no deadlock between worker threads.",
        "humanGate": true,
    })
    .to_string();
    let (status, body) = post(&site, LANE_START, &start_body);
    assert_eq!(status, StatusCode::OK, "{body}");
    let started: serde_json::Value = serde_json::from_str(&body).unwrap();
    let pipeline_id = started["id"].as_str().unwrap().to_owned();

    // Advances automatically through Plan + PlanJudge and pauses at AwaitingGate.
    let gated = wait_for_pipeline_status(&site, &nudges, &pipeline_id, "awaitingGate");
    let gated_stages = gated["stages"].as_array().unwrap();
    assert_eq!(gated_stages.len(), 6);
    assert_eq!(gated_stages[0]["status"], "completed");
    assert_eq!(gated_stages[1]["status"], "completed");
    assert_eq!(gated_stages[2]["status"], "pending");

    // Approve the plan gate to run Implement -> ReviewPropose x2 -> ReviewJudge -> Completed.
    let approve_body = serde_json::json!({
        "pipeline": pipeline_id,
        "acceptedProposals": [],
        "extraInstructions": "Keep diff minimal.",
    })
    .to_string();
    let (status, body) = post(&site, LANE_APPROVE, &approve_body);
    assert_eq!(status, StatusCode::OK, "{body}");

    let finished = wait_for_pipeline_status(&site, &nudges, &pipeline_id, "completed");
    let stages = finished["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 6);
    for (idx, stage) in stages.iter().enumerate() {
        let expected = if idx == 5 { "skipped" } else { "completed" };
        assert_eq!(
            stage["status"], expected,
            "stage {idx} unexpected status: {stage}"
        );
    }
    assert!(
        repo.root.join(".git/postil/pipelines.json").is_file(),
        "pipeline state persisted to disk"
    );
}

#[test]
fn custom_lane_applies_stage_overrides_and_rejects_mismatched_length() {
    let repo = TempRepo::new("hub-custom-lane");
    repo.write("a.txt", "initial\n");
    let (site, nudges) = run_site(&repo, "writes");

    let settings = get_json(&site, SETTINGS);
    let enabled = settings["settings"]["enabledAgents"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .to_owned();

    // Mismatched override count for Easy base lane (expects 1 stage, given 2) returns 400.
    let bad_body = serde_json::json!({
        "lane": "custom",
        "baseLane": "easy",
        "stageOverrides": [
            { "agent": enabled, "model": "custom-m1" },
            { "agent": enabled, "model": "custom-m2" }
        ],
        "kind": "issue",
        "number": 77,
        "title": "Bad override count",
    })
    .to_string();
    let (status, _) = post(&site, LANE_START, &bad_body);
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Valid custom lane overrides stage 0's agent & model and completes.
    let good_body = serde_json::json!({
        "lane": "custom",
        "baseLane": "easy",
        "stageOverrides": [
            { "agent": enabled, "model": "pro" }
        ],
        "kind": "issue",
        "number": 78,
        "title": "Run with custom stage override",
        "body": "Apply custom agent/model override.",
        "humanGate": false,
    })
    .to_string();
    let (status, body) = post(&site, LANE_START, &good_body);
    assert_eq!(status, StatusCode::OK, "{body}");
    let started: serde_json::Value = serde_json::from_str(&body).unwrap();
    let pipeline_id = started["id"].as_str().unwrap().to_owned();

    let finished = wait_for_pipeline_status(&site, &nudges, &pipeline_id, "completed");
    assert_eq!(finished["lane"], "custom");
    let stages = finished["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0]["agent"], enabled);
    assert_eq!(stages[0]["model"], "pro");
    assert_eq!(stages[0]["status"], "completed");
}
