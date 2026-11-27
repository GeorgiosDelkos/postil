//! Asks (docs/review-assist.md, "Ask without changing") through `/api/run/ask`: a note with
//! intent ask, run read-only in a thread of its own that only reads, against the fake ACP agent
//! in `tests/fake-agent.mjs`.

use super::runs::set_reviewers;
use super::threads::{end, thread};
use super::*;
use crate::run::TestDisk;

const ASK: &str = "postil://localhost/api/run/ask";

/// A `POST /api/run/ask` body asking `body` about lines `start` to `end` of `path`, with the
/// text the page shows on them, as the composer sends it.
fn ask_lines(site: &Site, path: &str, start: usize, end: usize, body: &str) -> String {
    let text = site.repo().unwrap().read(path).unwrap();
    let shown: Vec<&str> = text.lines().collect();
    serde_json::json!({
        "note": {
            "path": path,
            "lines": { "start": start, "end": end },
            "snippet": shown[start - 1..end].join("\n"),
            "body": body,
        },
    })
    .to_string()
}

/// Asks `body` about line 1 of `a.txt`, which must be accepted, and returns every note.
fn ask(site: &Site, body: &str) -> serde_json::Value {
    let (status, reply) = post(site, ASK, &ask_lines(site, "a.txt", 1, 1, body));
    assert_eq!(status, StatusCode::OK, "{reply}");
    serde_json::from_str(&reply).unwrap()
}

/// The `got` message of a run: what the fake agent received.
fn got(run: &serde_json::Value) -> String {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["text"].as_str())
        .find(|text| text.starts_with("got "))
        .unwrap_or_else(|| panic!("no prompt in {run}"))
        .to_owned()
}

/// The texts of a run's messages, in order.
fn said(run: &serde_json::Value) -> Vec<String> {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] == "message")
        .map(|entry| entry["text"].as_str().unwrap().to_owned())
        .collect()
}

/// The kinds of a run's entries.
fn entry_kinds(run: &serde_json::Value) -> Vec<String> {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["kind"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn an_ask_runs_read_only_in_a_thread_of_its_own_and_logs_no_report() {
    let repo = TempRepo::new("ask-read-only");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "asks-to-edit");

    let review = ask(&site, "Why does it return early?");

    let notes = review["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 1, "{review}");
    assert_eq!(notes[0]["id"], "N1", "{review}");
    assert_eq!(notes[0]["intent"], "ask", "{review}");
    let run = wait_for(&site, &nudges, "ended");
    assert_eq!(run["readOnly"], true, "{run}");
    let words = said(&run);
    assert!(words.contains(&"edit no".to_owned()), "{words:?}");
    assert!(words.contains(&"read once".to_owned()), "{words:?}");
    // No snapshot, so no changes view; and no report, though the reply ends with a block that
    // reads as one.
    let kinds = entry_kinds(&run);
    for kind in ["snapshot", "report", "noReport"] {
        assert!(!kinds.contains(&kind.to_owned()), "{kind}: {kinds:?}");
    }
    let (status, body) = get(&site, &format!("{RUN}/changes?from=R1"));
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // A thread of its own that only reads, at the commit HEAD names, which nothing moved.
    let record = thread(&site, "R1");
    assert_eq!(record["mode"], "read", "{record}");
    // Named from the question, as a thread is from its first note.
    assert_eq!(record["branch"], "postil/r1-why-does-it-return-early");
    let worktree = PathBuf::from(record["path"].as_str().unwrap());
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), record["fork"]);
    assert_eq!(
        states(&site),
        [serde_json::json!({ "kind": "sent", "run": "R1" })]
    );
}

#[test]
fn the_prompt_of_an_ask_asks_for_no_change_and_quotes_its_lines_file_or_folder() {
    let repo = TempRepo::new("ask-prompt");
    repo.write("a.txt", "a\nb\n");
    repo.write("src/lib.rs", "fn f() {}\n");
    let (site, nudges) = run_site(&repo, "plain");

    ask(&site, "Why?");
    let lines = got(&wait_for_run(&site, &nudges, "R1"));
    let rules = crate::notes::ASK_RULES;
    assert!(lines.contains(rules), "{lines}");
    assert!(
        lines.contains("[N1] a.txt:1\n> a\nWhy?\n"),
        "the lines quoted: {lines}"
    );
    assert!(!lines.contains(crate::report::ASK), "no report: {lines}");
    assert!(!lines.contains("Address every note"), "{lines}");
    // The file is linked, as a note's is.
    assert!(lines.contains("/a.txt"), "{lines}");

    for (path, target, place, id) in [
        (
            "src/lib.rs",
            "file",
            "[N2] src/lib.rs (file)\nWhat for?\n",
            "R2",
        ),
        ("src", "folder", "[N3] src/ (folder)\nWhat for?\n", "R3"),
    ] {
        let body = serde_json::json!({
            "note": { "path": path, "target": target, "body": "What for?" },
        });
        let (status, reply) = post(&site, ASK, &body.to_string());
        assert_eq!(status, StatusCode::OK, "{reply}");
        let prompt = got(&wait_for_run(&site, &nudges, id));
        assert!(prompt.contains(rules) && prompt.contains(place), "{prompt}");
    }
}

#[test]
fn a_read_thread_clones_no_build_folders() {
    let repo = TempRepo::new("ask-no-clones");
    repo.write(".gitignore", "target/\n");
    repo.write("a.txt", "a\n");
    fs::create_dir_all(repo.root.join("target/debug")).unwrap();
    repo.write("target/debug/app", "built\n");
    let (site, nudges) = run_site(&repo, "plain");

    ask(&site, "Why?");
    wait_for(&site, &nudges, "ended");

    let record = thread(&site, "R1");
    let worktree = PathBuf::from(record["path"].as_str().unwrap());
    assert!(worktree.join("a.txt").exists(), "a checkout: {record}");
    assert!(!worktree.join("target").exists(), "{record}");
    assert_eq!(record.get("skipped"), None, "{record}");
}

#[test]
fn a_follow_up_of_an_ask_is_read_only_with_its_kept_agent_and_with_a_fresh_one() {
    let repo = TempRepo::new("ask-follow");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "asks-to-edit");
    ask(&site, "Why?");
    wait_for_run(&site, &nudges, "R1");
    let follow = |site: &Site, of: &str| {
        let body = serde_json::json!({ "of": of, "text": "And then?" }).to_string();
        let (status, reply) = post(site, &format!("{RUN}/follow"), &body);
        assert_eq!(status, StatusCode::OK, "{reply}");
    };

    // The kept agent gets the words alone: no reminder of a report it was never asked for.
    follow(&site, "R1");
    let kept = wait_for_run(&site, &nudges, "R2");
    assert_eq!(kept["readOnly"], true, "{kept}");
    assert_eq!(got(&kept), "got And then?");
    assert!(said(&kept).contains(&"edit no".to_owned()), "{kept}");
    assert!(
        !entry_kinds(&kept).contains(&"snapshot".to_owned()),
        "{kept}"
    );
    assert!(!entry_kinds(&kept).contains(&"report".to_owned()), "{kept}");
    drop(site);

    // Another window has no agent for the thread: a fresh one is told the conversation, and
    // the rules again.
    let (site, nudges) = run_site(&repo, "asks-to-edit");
    follow(&site, "R2");
    let fresh = wait_for_run(&site, &nudges, "R3");
    assert_eq!(fresh["readOnly"], true, "{fresh}");
    let prompt = got(&fresh);
    assert!(prompt.contains(crate::notes::ASK_RULES), "{prompt}");
    assert!(!prompt.contains(crate::report::ASK_AGAIN), "{prompt}");
    assert!(
        prompt.contains("The engineer now asks:\n\nAnd then?"),
        "{prompt}"
    );
    assert!(said(&fresh).contains(&"edit no".to_owned()), "{fresh}");
}

#[test]
fn an_ask_that_edits_is_undone_and_says_so_after_a_restart_too() {
    let repo = TempRepo::new("ask-edits");
    repo.write("a.txt", "a\n");
    // Changes a.txt through a command, as an agent that edits without asking does.
    let (site, nudges) = run_site(&repo, "reviewer-writes");

    ask(&site, "Why?");
    let run = wait_for(&site, &nudges, "ended");

    let notice = "the agent changed files, though it may only read: its changes were undone";
    assert_eq!(run["problem"], notice, "not failed, and says so: {run}");
    let kinds = entry_kinds(&run);
    let undone = kinds.iter().position(|kind| kind == "undone");
    let ended = kinds.iter().position(|kind| kind == "ended");
    assert!(undone.is_some() && undone < ended, "{kinds:?}");
    let record = thread(&site, "R1");
    let worktree = PathBuf::from(record["path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(worktree.join("a.txt")).unwrap(), "a\n");
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    drop(site);

    // Logged, so the notice survives the window.
    let run = get_json(&repo.site(), &format!("{RUN}?id=R1"));
    assert_eq!(run["problem"], notice, "{run}");
    assert_eq!(run["status"], "ended", "{run}");
}

#[test]
fn apply_pull_request_and_review_refuse_a_read_thread_and_discard_ends_it() {
    let repo = TempRepo::new("ask-refusals");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    ask(&site, "Why?");
    wait_for(&site, &nudges, "ended");
    set_reviewers(&site, "plain", &[("Judge", "plain")]);

    for (refused, why) in [
        (end(&site, "apply", "R1"), "it has no changes to apply"),
        (
            end(&site, "pr", "R1"),
            "it has no changes for a pull request",
        ),
        (
            post(
                &site,
                &format!("{RUN}/review"),
                &serde_json::json!({ "of": "R1" }).to_string(),
            ),
            "there are no changes to review",
        ),
    ] {
        let (status, body) = refused;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
        assert!(body.contains(why), "{body}");
    }

    let (status, body) = end(&site, "discard", "R1");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(thread(&site, "R1")["state"], "discarded");
}

#[test]
fn a_send_and_a_preview_refuse_an_ask() {
    let repo = TempRepo::new("ask-not-sent");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    ask(&site, "Why?");
    wait_for(&site, &nudges, "ended");
    post_note(&site, "a.txt", 1, 1, "a change");

    let refusals = [
        send(&site, &["N2", "N1"]),
        get(&site, &format!("{RUN}/preview?notes=N1")),
    ];

    for (status, body) in refusals {
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("note N1 is a question"), "{body}");
    }
    assert_eq!(get_json(&site, RUNS)["runs"], serde_json::json!(["R1"]));
}

#[test]
fn an_ask_refused_at_the_cap_or_with_no_commit_saves_no_note() {
    let repo = TempRepo::new("ask-cap");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "hang");
    for (note, run) in [("N1", "R1"), ("N2", "R2"), ("N3", "R3")] {
        post_note(&site, "a.txt", 1, 1, note);
        send(&site, &[note]);
        assert_eq!(wait_for(&site, &nudges, "running")["id"], run);
    }

    let (status, body) = post(&site, ASK, &ask_lines(&site, "a.txt", 1, 1, "Why?"));

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("3 agents already work"), "{body}");
    assert_eq!(get_json(&site, NOTES)["notes"].as_array().unwrap().len(), 3);
    assert_eq!(
        get_json(&site, RUNS)["runs"],
        serde_json::json!(["R3", "R2", "R1"])
    );
    site.runner().unwrap().close();

    // A new branch with no commit yet: `HEAD` names nothing, as a send is refused for.
    let repo = TempRepo::new("ask-no-commit");
    repo.write("a.txt", "a\n");
    let (site, _nudges) = run_site(&repo, "plain");
    repo.git(&["checkout", "--quiet", "--orphan", "fresh"]);
    let (status, body) = post(&site, ASK, &ask_lines(&site, "a.txt", 1, 1, "Why?"));
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("no commit yet"), "{body}");
    assert_eq!(get_json(&site, NOTES)["notes"], serde_json::json!([]));
}

#[test]
fn a_bad_ask_is_refused_with_the_note_s_reason_and_saves_nothing() {
    let repo = TempRepo::new("ask-bad");
    repo.write("a.txt", "a\n");
    let (site, _nudges) = run_site(&repo, "plain");

    for (body, why) in [
        (
            ask_lines(&site, "a.txt", 1, 1, "  "),
            "the note has no text",
        ),
        (
            serde_json::json!({ "note": { "path": "a.txt", "target": "folder", "body": "x" } })
                .to_string(),
            "a.txt is not a folder",
        ),
        // The intent is the route's: a note body names none.
        (
            serde_json::json!({
                "note": { "path": "a.txt", "target": "file", "body": "x", "intent": "ask" },
            })
            .to_string(),
            "unknown field `intent`",
        ),
    ] {
        let (status, reply) = post(&site, ASK, &body);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
        assert!(reply.contains(why), "{reply}");
    }
    assert_eq!(get_json(&site, NOTES)["notes"], serde_json::json!([]));
    assert_eq!(get_json(&site, RUNS)["runs"], serde_json::json!([]));
}

#[test]
fn resolving_an_ask_is_refused_while_it_runs_and_frees_its_worktree_once_it_ended() {
    let repo = TempRepo::new("ask-resolve");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "hang");
    ask(&site, "Why?");
    wait_for(&site, &nudges, "running");
    let worktree = PathBuf::from(thread(&site, "R1")["path"].as_str().unwrap());
    let resolve = || post(&site, &format!("{NOTES}/resolve?id=N1"), "");

    let (status, body) = resolve();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_ne!(states(&site)[0]["kind"], "resolved");
    assert!(worktree.exists());

    // Stopped twice, the second time forced: the agent ignores the first.
    stop(&site);
    site.runner().unwrap().backdate_stop();
    stop(&site);
    wait_for(&site, &nudges, "failed");
    let (status, body) = resolve();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(states(&site)[0]["kind"], "resolved");
    assert_eq!(thread(&site, "R1")["state"], "discarded");
    assert!(!worktree.exists(), "freed");
}

#[test]
fn resolving_an_ask_ends_only_its_own_thread() {
    let repo = TempRepo::new("ask-resolve-own");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    ask(&site, "First?");
    wait_for_run(&site, &nudges, "R1");
    ask(&site, "Second?");
    wait_for_run(&site, &nudges, "R2");

    let (status, body) = post(&site, &format!("{NOTES}/resolve?id=N2"), "");

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(thread(&site, "R2")["state"], "discarded");
    assert_eq!(thread(&site, "R1")["state"], "open", "another ask's thread");
}

#[test]
fn resolving_a_change_note_reads_no_thread_record() {
    let repo = TempRepo::new("ask-resolve-change");
    repo.write("a.txt", "a\n");
    let site = repo.site();
    post_note(&site, "a.txt", 1, 1, "a change");
    // Only an ask has a thread to end first: a broken record is the next send's to report.
    let record = repo.root.join(".git/postil/threads.json");
    fs::write(&record, "{ not json").unwrap();

    let (status, body) = post(&site, &format!("{NOTES}/resolve?id=N1"), "");

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(states(&site)[0]["kind"], "resolved");
}

#[test]
fn an_ask_deleted_while_it_runs_takes_its_thread_with_it_when_the_run_ends() {
    // The run ends on the agent's own answer to a Stop, and by force on a second one.
    for scenario in ["report-cancel", "hang"] {
        let repo = TempRepo::new(&format!("ask-delete-{scenario}"));
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, scenario);
        ask(&site, "Why?");
        wait_for(&site, &nudges, "running");
        let worktree = PathBuf::from(thread(&site, "R1")["path"].as_str().unwrap());
        let log = repo.root.join(".git/postil/runs/R1.jsonl");

        let (status, body) = delete(&site, &format!("{NOTES}?id=N1"));
        assert_eq!(status, StatusCode::OK, "{scenario}: {body}");
        assert!(log.exists(), "{scenario}: the live run keeps its log");

        stop(&site);
        if scenario == "hang" {
            site.runner().unwrap().backdate_stop();
            stop(&site);
        }
        // The run's end deletes its log, and the thread goes with it: no delete follows.
        let deadline = Instant::now() + WAIT;
        while log.exists() || worktree.exists() {
            assert!(Instant::now() < deadline, "{scenario}: still there");
            nudges.recv_timeout(WAIT).ok();
        }
        let (status, body) = get(&site, "postil://localhost/api/thread?id=R1");
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::OK, "null"),
            "{scenario}"
        );
    }
}

#[test]
fn an_ask_deleted_while_its_worktree_is_made_takes_its_thread_with_it_when_the_run_ends() {
    // The run ends on its start thread: stopped while git makes the worktree, or with an agent
    // that cannot start.
    for case in ["stopped", "no-agent"] {
        let repo = TempRepo::new(&format!("ask-delete-making-{case}"));
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "plain");
        if case == "no-agent" {
            set_agents(&site, &[("Fake", "/nonexistent/agent-bin")], "Fake");
        }
        let release = site.runner().unwrap().hold_making();
        ask(&site, "Why?");
        let worktree = PathBuf::from(thread(&site, "R1")["path"].as_str().unwrap());
        let log = repo.root.join(".git/postil/runs/R1.jsonl");

        let (status, body) = delete(&site, &format!("{NOTES}?id=N1"));
        assert_eq!(status, StatusCode::OK, "{case}: {body}");
        assert!(log.exists(), "{case}: the live run keeps its log");
        if case == "stopped" {
            assert_eq!(stop(&site).0, StatusCode::OK, "{case}");
        }
        drop(release);

        let deadline = Instant::now() + WAIT;
        let gone = || get(&site, "postil://localhost/api/thread?id=R1").1 == "null";
        while log.exists() || worktree.exists() || !gone() {
            assert!(Instant::now() < deadline, "{case}: still there");
            nudges.recv_timeout(WAIT).ok();
        }
    }
}

#[test]
fn an_ask_whose_run_ends_before_its_agent_takes_it_shows_that_run() {
    // Refused by the model, an agent that exits as it starts, and a Stop while git makes the
    // worktree: the ask is the run's, sent in it, and its card and dialog say how it went.
    for (case, status) in [
        ("refusal", "ended"),
        ("exit-on-start", "failed"),
        ("stopped", "ended"),
    ] {
        let repo = TempRepo::new(&format!("ask-ends-early-{case}"));
        repo.write("a.txt", "a\n");
        let scenario = if case == "stopped" { "plain" } else { case };
        let (site, nudges) = run_site(&repo, scenario);
        let release = (case == "stopped").then(|| site.runner().unwrap().hold_making());

        ask(&site, "Why?");
        if case == "stopped" {
            assert_eq!(stop(&site).0, StatusCode::OK);
        }
        drop(release);
        let run = wait_for_run(&site, &nudges, "R1");

        assert_eq!(run["status"], status, "{case}: {run}");
        assert_eq!(
            states(&site),
            [serde_json::json!({ "kind": "sent", "run": "R1" })],
            "{case}"
        );
        assert_eq!(run["notes"], serde_json::json!(["N1"]), "{case}");
    }
}

#[test]
fn an_evicted_ask_s_follow_up_after_a_restart_makes_a_read_only_worktree_again() {
    let repo = TempRepo::new("ask-evicted");
    repo.write(".gitignore", "target/\n");
    repo.write("a.txt", "a\n");
    fs::create_dir_all(repo.root.join("target/debug")).unwrap();
    repo.write("target/debug/app", "built\n");
    let (site, nudges) = run_site(&repo, "asks-to-edit");
    let one_worktree = || TestDisk {
        max_worktrees: Some(1),
        ..TestDisk::default()
    };
    site.runner().unwrap().set_test_disk(one_worktree());
    ask(&site, "First?");
    wait_for_run(&site, &nudges, "R1");
    // A second ask takes the only worktree there is.
    ask(&site, "Second?");
    wait_for_run(&site, &nudges, "R2");
    assert_eq!(thread(&site, "R1")["state"], "evicted");
    drop(site);

    // Another window, so a fresh agent: R1's worktree is made again, R2's goes.
    let (site, nudges) = run_site(&repo, "asks-to-edit");
    site.runner().unwrap().set_test_disk(one_worktree());
    let body = serde_json::json!({ "of": "R1", "text": "And then?" }).to_string();
    let (status, reply) = post(&site, &format!("{RUN}/follow"), &body);
    assert_eq!(status, StatusCode::OK, "{reply}");
    let follow = wait_for_run(&site, &nudges, "R3");

    assert_eq!(follow["status"], "ended", "{follow}");
    assert_eq!(follow["readOnly"], true, "{follow}");
    assert!(
        !entry_kinds(&follow).contains(&"snapshot".to_owned()),
        "{follow}"
    );
    assert!(said(&follow).contains(&"edit no".to_owned()), "{follow}");
    assert!(got(&follow).contains(crate::notes::ASK_RULES), "{follow}");
    let record = thread(&site, "R1");
    assert_eq!(record["state"], "open", "{record}");
    let worktree = PathBuf::from(record["path"].as_str().unwrap());
    assert!(worktree.join("a.txt").exists() && !worktree.join("target").exists());
    assert_eq!(thread(&site, "R2")["state"], "evicted");
}

#[test]
fn a_version_4_review_reads_every_note_as_a_change() {
    let repo = TempRepo::new("ask-review-4");
    repo.write("a.txt", "a\n");
    let site = repo.site();
    let file = site.repo().unwrap().review_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        r#"{"version": 4, "notes": [{"id": "N1", "path": "a.txt", "target": "file",
            "body": "b", "anchor": "found", "state": {"kind": "open"}}]}"#,
    )
    .unwrap();

    assert_eq!(get_json(&site, NOTES)["notes"][0]["intent"], "change");
}

#[test]
fn a_version_3_record_reads_every_thread_as_work() {
    let repo = TempRepo::new("ask-threads-3");
    let site = repo.site();
    let file = repo.root.join(".git/postil/threads.json");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let record = serde_json::json!({
        "version": 3,
        "threads": [{
            "id": "R1", "path": "/nowhere/r1", "branch": "postil/r1", "fork": "f0",
            "base": "trunk", "tip": "f0", "lastRun": 1, "state": "discarded",
        }],
    });
    fs::write(&file, record.to_string()).unwrap();

    let read = get_json(&site, "postil://localhost/api/thread?id=R1");
    assert_eq!(read["mode"], "work", "{read}");
}

#[test]
fn a_version_6_log_reads_as_a_run_that_may_change_files() {
    let repo = TempRepo::new("ask-log-6");
    let runs = repo.root.join(".git/postil/runs");
    fs::create_dir_all(&runs).unwrap();
    let header = concat!(
        r#"{"version":6,"id":"R1","agent":"Fake","notes":["N1"],"prompt":"p","#,
        r#""startedAt":1}"#
    );
    fs::write(
        runs.join("R1.jsonl"),
        format!("{header}\n{{\"kind\":\"ended\",\"reason\":\"end_turn\"}}\n"),
    )
    .unwrap();

    let run = get_json(&repo.site(), RUN);
    assert_eq!(run["readOnly"], false, "{run}");
    assert_eq!(run["problem"], serde_json::Value::Null, "{run}");
}

#[test]
fn the_uncommitted_changes_say_whether_the_asked_file_is_among_them() {
    let repo = TempRepo::new("ask-uncommitted");
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    let (site, _nudges) = run_site(&repo, "plain");
    let uncommitted = |query: &str| get_json(&site, &format!("{RUN}/uncommitted{query}"));
    assert_eq!(uncommitted("")["uncommitted"], 0);

    repo.write("a.txt", "edited\n");

    let asked = uncommitted("?file=a.txt");
    assert_eq!(asked["uncommitted"], 1, "{asked}");
    assert_eq!(asked["uncommittedNoted"], serde_json::json!(["a.txt"]));
    let other = uncommitted("?file=b.txt");
    assert_eq!(other["uncommittedNoted"], serde_json::json!([]), "{other}");
}
