//! Tests of `/api/usage`, `/api/usage/refresh`, and durable per-model usage tracking.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::usage::quota::{QuotaPoller, QuotaReport, QuotaStatus, QuotaWindow};

fn make_executable(dir: &Path, name: &str) {
    let path = dir.join(name);
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn usage_lists_installed_built_in_agents_with_live_quotas_and_refresh() {
    let repo = TempRepo::new("usage-built-in");
    let bin = repo.root.join(".git/fake-bin");
    fs::create_dir_all(&bin).unwrap();
    make_executable(&bin, "claude");
    make_executable(&bin, "codex");

    let calls = Arc::new(AtomicUsize::new(0));
    let probe_calls = Arc::clone(&calls);
    let poller = QuotaPoller::new(move |name, _| {
        let count = probe_calls.fetch_add(1, Ordering::SeqCst) + 1;
        QuotaReport {
            status: QuotaStatus::Ok,
            plan: Some(if name == "Claude" { "Max" } else { "Plus" }.to_owned()),
            windows: vec![QuotaWindow {
                label: "5-hour window".to_owned(),
                model: None,
                used_percent: if name == "Claude" { 42.0 } else { 15.0 },
                resets_at: Some(1_800_000_000),
                detail: Some(format!("call {count}")),
            }],
            credits: None,
            message: None,
            fetched_at: Some(1_795_000_000),
        }
    });

    let search = OsString::from(&bin);
    let scanner = Scanner::new(move || search.clone());
    // Wait so the first GET sees the finished scan immediately.
    let _ = scanner.wait();

    let site =
        Site::new(Some(&repo.config()), None, None, scanner, quiet()).with_quota_poller(poller);
    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();

    let usage = "postil://localhost/api/usage?scope=project&range=7d";
    let first = get_json(&site, usage);
    assert_eq!(
        first["polling"], true,
        "the first GET starts the probes: {first}"
    );
    site.quotas.join_probes();
    let body = get_json(&site, usage);
    assert_eq!(body["polling"], false, "{body}");

    let agents = body["agents"].as_array().unwrap();
    let names: Vec<&str> = agents
        .iter()
        .map(|agent| agent["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Claude", "Codex"]);
    assert_eq!(agents[0]["quota"]["status"], "ok");
    assert_eq!(agents[0]["quota"]["plan"], "Max");
    assert_eq!(agents[0]["quota"]["windows"][0]["usedPercent"], 42.0);
    assert_eq!(agents[1]["quota"]["plan"], "Plus");
    let before_refresh = calls.load(Ordering::SeqCst);
    assert_eq!(before_refresh, 2);

    // POST /api/usage/refresh forces fresh probes.
    let (status, _) = post(
        &site,
        "postil://localhost/api/usage/refresh?scope=project&range=7d",
        "",
    );
    assert_eq!(status, StatusCode::OK);
    site.quotas.join_probes();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "refresh re-runs both probes"
    );
    // The two refresh probes run at once, so either may be call 3.
    let refreshed = get_json(&site, usage);
    let mut details: Vec<&str> = refreshed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["quota"]["windows"][0]["detail"].as_str().unwrap())
        .collect();
    details.sort_unstable();
    assert_eq!(
        details,
        ["call 3", "call 4"],
        "the page shows the new results"
    );
}

#[test]
fn a_built_in_agent_with_history_but_no_program_is_not_probed_and_says_not_installed() {
    let repo = TempRepo::new("usage-uninstalled");
    let bin = repo.root.join(".git/empty-bin");
    fs::create_dir_all(&bin).unwrap();
    let config = repo.config();
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("usage.jsonl"),
        concat!(
            r#"{"version":1,"runId":"R1","repo":"/elsewhere","agent":"Claude","#,
            r#""origin":"notes","outcome":"ended","startedAt":1,"endedAt":2}"#,
            "\n",
        ),
    )
    .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let probe_calls = Arc::clone(&calls);
    let poller = QuotaPoller::new(move |_, _| {
        probe_calls.fetch_add(1, Ordering::SeqCst);
        QuotaReport::unavailable("probed")
    });
    let search = OsString::from(&bin);
    let scanner = Scanner::new(move || search.clone());
    let _ = scanner.wait();
    let site = Site::new(Some(&config), None, None, scanner, quiet()).with_quota_poller(poller);

    let body = get_json(&site, "postil://localhost/api/usage?scope=all&range=all");
    let claude = &body["agents"][0];
    assert_eq!(claude["name"], "Claude", "{body}");
    assert_eq!(claude["builtIn"], true);
    assert_eq!(claude["quota"]["message"], "Not installed on PATH");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an absent program is never probed"
    );
}

#[test]
fn a_finished_run_records_model_tokens_and_cost_that_survive_note_deletion() {
    let repo = TempRepo::new("usage-run");
    repo.write("src/a.rs", "one\ntwo\nthree\n");
    let (site, nudges) = run_site(&repo, "turn");
    let site = site.with_quota_poller(QuotaPoller::new(|_, _| {
        QuotaReport::unavailable("test stub")
    }));

    post_note(&site, "src/a.rs", 1, 1, "check this");
    let (status, body) = send(&site, &["N1"]);
    assert_eq!(status, StatusCode::OK, "{body}");
    let run = wait_for(&site, &nudges, "ended");
    assert_eq!(run["id"], "R1");

    // Delete the note, which removes R1.jsonl from the repo, and verify usage survives.
    let (status, body) = delete(&site, &format!("{NOTES}?id=N1"));
    assert_eq!(status, StatusCode::OK, "{body}");

    let deadline = Instant::now() + Duration::from_secs(5);
    let usage = loop {
        let reply = get_json(
            &site,
            "postil://localhost/api/usage?scope=project&range=24h",
        );
        if reply["scanning"] == false {
            break reply;
        }
        assert!(Instant::now() < deadline, "agent scan did not finish");
        std::thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(usage["summary"]["runs"], 1);
    assert_eq!(usage["summary"]["totalTokens"], 160);
    assert_eq!(usage["summary"]["inputTokens"], 120);
    assert_eq!(usage["summary"]["outputTokens"], 30);
    assert_eq!(usage["summary"]["cachedReadTokens"], 40);
    assert_eq!(usage["summary"]["thoughtTokens"], 10);
    assert_eq!(usage["summary"]["cost"]["currency"], "USD");
    let cost = usage["summary"]["cost"]["amount"].as_f64().unwrap();
    assert!((cost - 0.02).abs() < 1e-9, "cost={cost}");

    let fake = usage["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["name"] == "Fake")
        .expect("custom agent Fake is listed");
    assert_eq!(fake["builtIn"], false);
    assert_eq!(fake["activity"]["runs"], 1);
    assert_eq!(fake["activity"]["models"][0]["model"], "m");
    assert_eq!(fake["activity"]["models"][0]["totalTokens"], 160);
}

#[test]
fn a_kept_agent_follow_up_records_only_its_session_cost_delta() {
    let repo = TempRepo::new("usage-followup-delta");
    repo.write("src/a.rs", "one\ntwo\nthree\n");
    let (site, nudges) = run_site(&repo, "turn");
    let site = site.with_quota_poller(QuotaPoller::new(|_, _| {
        QuotaReport::unavailable("test stub")
    }));

    post_note(&site, "src/a.rs", 1, 1, "check this");
    assert_eq!(send(&site, &["N1"]).0, StatusCode::OK);
    let first = wait_for_run(&site, &nudges, "R1");
    assert_eq!(first["status"], "ended");

    // Follow up on the same kept agent: fake-agent reports cumulative cost 0.04 on prompt 2,
    // so R2 should log only its 0.02 delta (total 0.04 across both runs, not 0.06).
    let (status, body) = post(
        &site,
        &format!("{RUN}/follow"),
        &serde_json::json!({ "of": "R1", "text": "one more tweak" }).to_string(),
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    let second = wait_for_run(&site, &nudges, "R2");
    assert_eq!(second["status"], "ended");

    let deadline = Instant::now() + Duration::from_secs(5);
    let usage = loop {
        let reply = get_json(
            &site,
            "postil://localhost/api/usage?scope=project&range=24h",
        );
        if reply["scanning"] == false {
            break reply;
        }
        assert!(Instant::now() < deadline, "agent scan did not finish");
        std::thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(usage["summary"]["runs"], 2);
    let cost = usage["summary"]["cost"]["amount"].as_f64().unwrap();
    assert!(
        (cost - 0.04).abs() < 1e-9,
        "expected 0.04 cumulative, got {cost}"
    );
}

#[test]
fn invalid_usage_query_params_are_refused() {
    let repo = TempRepo::new("usage-params");
    let site = repo.empty_site();
    assert_eq!(
        get(&site, "postil://localhost/api/usage?scope=bogus").0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&site, "postil://localhost/api/usage?range=1y").0,
        StatusCode::BAD_REQUEST
    );
}
