//! Runs (`run.rs`, `run/log.rs`) through `/api/run*`, against the fake ACP agent in
//! `tests/fake-agent.mjs`.

use super::*;

/// The kinds of a run's entries, each message with its text after a colon. Snapshots are
/// left out, as in [`entries`].
fn kinds(run: &serde_json::Value) -> Vec<String> {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] != "snapshot")
        .map(|entry| match entry["kind"].as_str().unwrap() {
            "message" => format!("message:{}", entry["text"].as_str().unwrap()),
            kind => kind.to_owned(),
        })
        .collect()
}

/// A run's entries without its snapshots: the tests of snapshots read them on their own,
/// and the others count entries by place.
fn entries(run: &serde_json::Value) -> Vec<serde_json::Value> {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] != "snapshot")
        .cloned()
        .collect()
}

/// The points of a run's snapshots, in order: `["before", "after"]` for a run that ended.
fn snapshot_points(run: &serde_json::Value) -> Vec<serde_json::Value> {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] == "snapshot")
        .map(|entry| entry["point"].clone())
        .collect()
}

fn file_uri_of(repo: &TempRepo, path: &str) -> String {
    let root = Repo::open(&repo.root).unwrap().root().to_owned();
    format!("file://{}/{path}", root.display())
}

/// The run's first entry of `kind`.
fn first_of_kind(run: &serde_json::Value, kind: &str) -> serde_json::Value {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == kind)
        .unwrap()
        .clone()
}

/// The message ids of the run's message entries.
fn message_ids(run: &serde_json::Value) -> Vec<&str> {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] == "message")
        .map(|entry| entry["message"].as_str().unwrap())
        .collect()
}

#[test]
fn sent_notes_reach_the_agent_and_every_step_of_the_run_is_kept_on_disk() {
    let repo = TempRepo::new("run-turn");
    repo.write("src/a.rs", "fn a() {\n    one();\n}\n");
    repo.write("b.txt", "b\n");
    let (site, nudges) = run_site(&repo, "turn");
    post_note(&site, "src/a.rs", 2, 2, "rename one");
    post_note(&site, "b.txt", 1, 1, "drop b");
    post_note(&site, "src/a.rs", 1, 3, "not this one");

    assert_eq!(
        get(&site, RUN),
        (StatusCode::OK, "null".to_owned()),
        "no run yet"
    );
    let (status, body) = send(&site, &["N2", "N1"]);
    assert_eq!(status, StatusCode::OK, "{body}");

    // Postil answers the permission request itself, so the run goes to its end.
    let ended = wait_for(&site, &nudges, "ended");
    let prompt = ended["prompt"].as_str().unwrap();
    assert!(
        prompt.find("[N2] b.txt:1") < prompt.find("[N1] src/a.rs:2"),
        "the notes in the order chosen: {prompt}"
    );
    assert_eq!(
        kinds(&ended)[..3],
        [
            "started".to_owned(),
            format!(
                "message:got {prompt} | {} | {}",
                file_uri_of(&repo, "b.txt"),
                file_uri_of(&repo, "src/a.rs")
            ),
            "message:prompt 1".to_owned(),
        ],
        "the prompt as previewed, then one link per file"
    );
    assert_eq!(ended["notes"], serde_json::json!(["N2", "N1"]));
    assert_eq!(ended["agent"], "Fake");
    assert_eq!(
        states(&site),
        [
            serde_json::json!({ "kind": "sent", "run": "R1" }),
            serde_json::json!({ "kind": "sent", "run": "R1" }),
            serde_json::json!({ "kind": "open" }),
        ],
        "only the notes sent are marked, with their run"
    );

    let permission = first_of_kind(&ended, "permission");
    assert_eq!(permission["toolCall"], "t1");
    assert_eq!(
        first_of_kind(&ended, "answer")["option"],
        "once",
        "allowed once, of the options allow once, allow always and reject"
    );
    assert_eq!(
        first_of_kind(&ended, "tool")["places"],
        serde_json::json!([{ "path": "src/a.rs", "line": 3 }]),
        "a tool's files are relative to the repository"
    );
    let kinds = kinds(&ended);
    assert_eq!(
        kinds[3..],
        [
            "message:fs answered -32601",
            "thought",
            "tool",
            "tool",
            "permission",
            "answer",
            "message:chose once",
            "toolUpdate",
            "plan",
            "ended",
        ]
    );
    let ids = message_ids(&ended);
    assert_eq!(
        ids,
        ["m1", "m2", "m3", "m4"],
        "each part keeps its message's id"
    );
    let update = &entries(&ended)[kinds.iter().position(|kind| kind == "toolUpdate").unwrap()];
    assert_eq!(
        update["output"],
        serde_json::json!([
            { "type": "diff", "path": "src/a.rs", "oldText": "a", "newText": "b" }
        ]),
        "paths inside the repository are shown relative to it"
    );
    let last = ended["entries"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["reason"], "end_turn");

    // The page asks only for what it has not seen.
    let total = ended["total"].as_u64().unwrap();
    let tail = get_json(&site, &format!("{RUN}?after={}", total - 1));
    assert_eq!(tail["entries"], serde_json::json!([last]));
    assert_eq!(tail["total"], total);
    assert_eq!(tail["status"], "ended");
    assert!(
        tail.get("prompt").is_none() && tail.get("notes").is_none(),
        "the page has the header fields that never change: {tail}"
    );

    drop(site);
    assert_read_back(&repo, &ended);
}

/// The log on disk holds `run`, as a new site (a restart) reads it back.
fn assert_read_back(repo: &TempRepo, run: &serde_json::Value) {
    let read = get_json(&repo.site(), RUN);
    assert_eq!(read["status"], "ended");
    assert_eq!(read["entries"], run["entries"]);
    assert_eq!(read["prompt"], run["prompt"]);
    assert_eq!(read["problem"], serde_json::Value::Null);
}

#[test]
fn file_and_folder_notes_reach_the_agent_by_path_and_only_files_are_linked() {
    let repo = TempRepo::new("run-paths");
    fs::create_dir_all(repo.root.join("src/acp")).unwrap();
    repo.write("src/acp/wire.rs", "wire\n");
    repo.write("README.md", "# Readme\n");
    repo.write("old.rs", "old\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_path_note(&site, "src/acp", "folder", "Split this module.");
    post_path_note(&site, "README.md", "file", "Document the flag.");
    post_path_note(&site, "old.rs", "file", "Fold into lib.rs.");
    post_path_note(&site, "src", "folder", "Explain the layout.");
    fs::remove_file(repo.root.join("old.rs")).unwrap();

    let preview = get_json(&site, &format!("{RUN}/preview?notes=N1,N2,N3"));
    assert_eq!(
        preview["prompt"],
        "These are review notes an engineer wrote while reading this repository.\n\
         Address every note. Change nothing the notes do not ask for.\n\
         When you finish, list each note id with one line on what you did.\n\
         \n\
         [N1] src/acp/ (folder)\n\
         Split this module.\n\
         \n\
         [N2] README.md (file)\n\
         Document the flag.\n\
         \n\
         [N3] old.rs (file)\n\
         (This file is no longer in the repository.)\n\
         Fold into lib.rs.\n"
    );
    assert_eq!(
        preview["files"],
        serde_json::json!(["README.md", "old.rs"]),
        "a folder is named in the text, never linked"
    );

    let (status, body) = send(&site, &["N4"]);
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = wait_for(&site, &nudges, "ended");
    let prompt = ended["prompt"].as_str().unwrap();
    assert!(
        prompt.ends_with("[N4] src/ (folder)\nExplain the layout.\n"),
        "{prompt}"
    );
    assert_eq!(
        kinds(&ended)[1],
        format!("message:got {prompt}"),
        "the text alone, with no link"
    );
}

#[test]
fn the_prompt_preview_is_the_text_the_agent_receives() {
    let repo = TempRepo::new("run-preview");
    repo.write("src/a.rs", "fn a() {\n    one();\n}\n");
    let (site, _nudges) = run_site(&repo, "plain");
    post_note(&site, "src/a.rs", 1, 2, "  explain this  \n");
    post_note(&site, "src/a.rs", 3, 3, "and this");
    repo.write("src/a.rs", "fn a() {\n    one();\n// moved\n");

    let preview = get_json(&site, &format!("{RUN}/preview?notes=N2,N1"));
    assert_eq!(
        preview["prompt"],
        "These are review notes an engineer wrote while reading this repository.\n\
         Address every note. Change nothing the notes do not ask for.\n\
         When you finish, list each note id with one line on what you did.\n\
         \n\
         [N2] src/a.rs:3\n\
         (These lines are no longer in the file; they were:)\n\
         > }\n\
         and this\n\
         \n\
         [N1] src/a.rs:1-2\n\
         > fn a() {\n\
         >     one();\n\
         explain this\n"
    );
}

#[test]
fn the_agent_is_kept_between_runs_and_replaced_when_another_is_chosen() {
    let repo = TempRepo::new("run-keep");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 2, 2, "two");

    send(&site, &["N1"]);
    let first = wait_for(&site, &nudges, "ended");
    assert!(
        kinds(&first).contains(&"message:prompt 1".to_owned()),
        "{first}"
    );
    let (status, body) = send(&site, &["N2"]);
    assert_eq!(status, StatusCode::OK, "{body}");
    let second = wait_for(&site, &nudges, "ended");
    assert_eq!(second["id"], "R2");
    assert!(
        kinds(&second).contains(&"message:prompt 2".to_owned()),
        "the same agent process: {second}"
    );

    // Another command is another agent, started fresh.
    set_agents(&site, &[("Other", &fake_agent("plain2"))], "Other");
    send(&site, &["N1"]);
    let third = wait_for(&site, &nudges, "ended");
    assert_eq!(third["agent"], "Other");
    assert!(
        kinds(&third).contains(&"message:prompt 1".to_owned()),
        "{third}"
    );
}

#[test]
fn any_run_can_be_read_by_its_id_in_this_window_and_from_disk_in_another() {
    let repo = TempRepo::new("run-by-id");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 2, 2, "two");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    send(&site, &["N2"]);
    let second = wait_for(&site, &nudges, "ended");

    let first = get_json(&site, &format!("{RUN}?id=R1"));
    assert_eq!(first["id"], "R1");
    assert_eq!(first["notes"], serde_json::json!(["N1"]));
    assert!(
        kinds(&first).contains(&"message:prompt 1".to_owned()),
        "{first}"
    );
    assert_eq!(
        get_json(&site, &format!("{RUN}?id=R2")),
        second,
        "this window's run, whole"
    );

    // Another window has run nothing: it reads the logs.
    let (other, _) = run_site(&repo, "plain");
    assert_eq!(get_json(&other, &format!("{RUN}?id=R1")), first);

    let (status, body) = get(&site, &format!("{RUN}?id=R3"));
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("no such run log"), "{body}");
    for id in ["R01", "R+1", "..%2FR1", ""] {
        let (status, body) = get(&site, &format!("{RUN}?id={id}"));
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {body}");
        assert!(body.contains("expected R and a number"), "{id}: {body}");
    }

    // A file named like a log but not by its own id is no run: listing it as R1 would name
    // another log.
    fs::write(repo.root.join(".git/postil/runs/R01.jsonl"), "").unwrap();
    assert_eq!(
        get_json(&site, RUNS)["runs"],
        serde_json::json!(["R2", "R1"])
    );
}

#[test]
fn a_run_is_deleted_with_its_last_note() {
    let repo = TempRepo::new("run-delete");
    repo.write("a.txt", "a\nb\nc\n");
    let (site, nudges) = run_site(&repo, "plain");
    for (line, body) in [(1, "one"), (2, "two"), (3, "three")] {
        post_note(&site, "a.txt", line, line, body);
    }
    send(&site, &["N1", "N2"]);
    wait_for(&site, &nudges, "ended");
    send(&site, &["N3"]);
    wait_for(&site, &nudges, "ended");
    let log = |id: &str| repo.root.join(format!(".git/postil/runs/{id}.jsonl"));

    let (status, body) = delete(&site, &format!("{NOTES}?id=N1"));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(log("R1").exists(), "R1 still has N2");
    let runs = || get_json(&site, RUNS)["runs"].clone();
    assert_eq!(runs(), serde_json::json!(["R2", "R1"]), "newest first");

    delete(&site, &format!("{NOTES}?id=N3"));
    assert!(!log("R2").exists(), "R2's only note is gone");
    assert_eq!(
        get_json(&site, RUN)["id"],
        "R1",
        "the latest run left is shown"
    );
    assert_eq!(get(&site, &format!("{RUN}?id=R2")).0, StatusCode::NOT_FOUND);
    assert_eq!(runs(), serde_json::json!(["R1"]));

    // R2's number is not given again: a page that read R2 would take the new run for it.
    send(&site, &["N2"]);
    assert_eq!(wait_for(&site, &nudges, "ended")["id"], "R3");
    assert_eq!(runs(), serde_json::json!(["R3", "R1"]));

    // A log that cannot be read is left alone: its notes cannot be checked.
    fs::write(log("R8"), "not a run log\n").unwrap();
    delete(&site, &format!("{NOTES}?id=N2"));
    assert!(!log("R1").exists());
    assert!(log("R8").exists(), "an unreadable log is kept");
    assert_eq!(
        get_json(&site, RUN),
        serde_json::Value::Null,
        "no run is left"
    );
    assert_eq!(runs(), serde_json::json!(["R8"]));
}

#[test]
fn a_live_run_is_kept_when_its_note_is_deleted_and_goes_at_the_next_delete() {
    let repo = TempRepo::new("run-delete-live");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "hang");
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 2, 2, "two");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "running");
    let log = repo.root.join(".git/postil/runs/R1.jsonl");

    delete(&site, &format!("{NOTES}?id=N1"));
    assert!(log.exists(), "the live run keeps its log");
    assert_eq!(get_json(&site, RUN)["status"], "running");

    // Stopped twice, the second time forced, the run ends; the next delete takes it.
    let cancel = format!("{RUN}/cancel");
    post(&site, &cancel, "");
    site.runner().unwrap().backdate_stop();
    post(&site, &cancel, "");
    wait_for(&site, &nudges, "failed");
    delete(&site, &format!("{NOTES}?id=N2"));
    assert!(!log.exists());
}

#[test]
fn a_question_waits_for_the_engineer_and_reaches_the_agent_answered_or_skipped() {
    let repo = TempRepo::new("run-ask");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "ask");
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 2, 2, "two");
    let answer =
        |body: serde_json::Value| post(&site, &format!("{RUN}/question"), &body.to_string());

    send(&site, &["N1"]);
    let waiting = wait_for(&site, &nudges, "waiting");
    assert!(
        kinds(&waiting).contains(&r#"message:form {"form":{}}"#.to_owned()),
        "Postil says it shows form questions: {waiting}"
    );
    let question = first_of_kind(&waiting, "question");
    assert_eq!(
        question["message"],
        "Should divide(i32::MIN, -1) also return None?"
    );
    assert_eq!(
        question["fields"]["properties"]["question_0"]["oneOf"][0]["const"], "Yes",
        "the form as the agent sent it"
    );
    let request = question["request"].clone();
    let (status, body) = answer(serde_json::json!({ "request": 99, "content": {} }));
    assert_eq!(status, StatusCode::NOT_FOUND, "no such question: {body}");
    let (status, _) = get(&site, &format!("{RUN}/question"));
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "an answer is posted"
    );
    let (status, body) = post(
        &site,
        &format!("{RUN}/permission"),
        &serde_json::json!({ "request": request, "option": "once" }).to_string(),
    );
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a question is no permission request: {body}"
    );

    let content = serde_json::json!({ "question_0": "Yes", "question_0_custom": "and log it" });
    let (status, body) = answer(serde_json::json!({ "request": request, "content": content }));
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = wait_for(&site, &nudges, "ended");
    let accepted = serde_json::json!({ "action": "accept", "content": content });
    assert!(
        kinds(&ended).contains(&format!("message:answered {accepted}")),
        "{ended}"
    );
    assert_eq!(first_of_kind(&ended, "reply")["content"], content);

    // The second asks by the name Claude's adapter uses; a skip is a decline.
    send(&site, &["N2"]);
    let waiting = wait_for(&site, &nudges, "waiting");
    let request = first_of_kind(&waiting, "question")["request"].clone();
    let (status, body) = answer(serde_json::json!({ "request": request, "content": null }));
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = wait_for(&site, &nudges, "ended");
    assert!(
        kinds(&ended).contains(&r#"message:answered {"action":"decline"}"#.to_owned()),
        "{ended}"
    );
    assert_eq!(
        first_of_kind(&ended, "reply")["content"],
        serde_json::Value::Null
    );
}

#[test]
fn a_withdrawn_question_closes_and_a_url_question_is_refused() {
    let repo = TempRepo::new("run-withdraw");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "ask-withdraw");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let ended = wait_for(&site, &nudges, "ended");
    let kinds = kinds(&ended);
    assert!(
        kinds.contains(&"message:url answered -32602".to_owned()),
        "a URL question cannot be shown as a form: {ended}"
    );
    assert!(
        kinds.contains(&"message:empty form answered -32602".to_owned()),
        "a form with no fields is refused: {ended}"
    );
    assert!(
        kinds.contains(&"message:withdrawn answered cancel".to_owned()),
        "a withdrawn request is still answered, as ACP asks: {ended}"
    );
    let question = kinds.iter().position(|kind| kind == "question").unwrap();
    assert_eq!(kinds[question + 1], "answer", "withdrawn: {ended}");
    assert_eq!(
        entries(&ended)[question + 1]["option"],
        serde_json::Value::Null
    );
}

#[test]
fn a_permission_request_with_no_allow_once_option_is_allowed_always() {
    let repo = TempRepo::new("run-always-only");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "always-only");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let ended = wait_for(&site, &nudges, "ended");
    assert_eq!(first_of_kind(&ended, "answer")["option"], "always");
    assert!(
        kinds(&ended).contains(&"message:chose always".to_owned()),
        "{ended}"
    );
}

#[test]
fn a_question_left_open_at_the_turn_s_end_is_answered_cancel() {
    let repo = TempRepo::new("run-leave-open");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "leave-open");
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 2, 2, "two");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    // The same agent, kept: its second turn reports the reply to the first's question.
    send(&site, &["N2"]);
    let ended = wait_for(&site, &nudges, "ended");
    assert!(
        kinds(&ended).contains(&"message:left open answered cancel".to_owned()),
        "{ended}"
    );
}

#[test]
fn a_permission_request_with_no_allow_option_waits_for_the_engineer() {
    let repo = TempRepo::new("run-reject-only");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "reject-only");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let waiting = wait_for(&site, &nudges, "waiting");
    let request = first_of_kind(&waiting, "permission")["request"].clone();
    let choose = |option: &str| {
        let body = serde_json::json!({ "request": request, "option": option });
        post(&site, &format!("{RUN}/permission"), &body.to_string())
    };
    let (status, body) = choose("sure");
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "not an offered option: {body}"
    );
    let (status, body) = choose("no");
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = wait_for(&site, &nudges, "ended");
    assert!(
        kinds(&ended).contains(&"message:chose no".to_owned()),
        "{ended}"
    );
}

#[test]
fn reviewers_propose_read_only_then_the_last_judges_the_findings() {
    let repo = TempRepo::new("run-review");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    let review = |of: &str| {
        let body = serde_json::json!({ "of": of }).to_string();
        post(&site, &format!("{RUN}/review"), &body)
    };
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    let (status, body) = review("R1");
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("no reviewers are set"), "{body}");

    let settings = serde_json::json!({
        "customAgents": [
            { "name": "Fake", "command": fake_agent("plain") },
            { "name": "Reviewer", "command": fake_agent("reviewer") },
        ],
        "enabledAgents": ["Fake", "Reviewer"],
        "agent": "Fake",
        "reviewers": ["Reviewer", "Reviewer"],
    });
    let (status, body) = save(&site, &settings.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = review("R1");
    assert_eq!(status, StatusCode::OK, "{body}");

    // The judge, R3, starts on its own once the proposer, R2, ends.
    let judge = loop {
        let run = get_json(&site, RUN);
        if run["id"] == "R3" && run["status"] == "ended" {
            break run;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("no change while waiting for the judge: {run}"));
    };
    let proposer = get_json(&site, &format!("{RUN}?id=R2"));
    assert_eq!(
        proposer["review"],
        serde_json::json!({ "of": "R1", "reviewer": 1, "reviewers": 2 })
    );
    assert_eq!(
        proposer["notes"],
        serde_json::json!(["N1"]),
        "the reviewed run's notes"
    );
    assert!(
        kinds(&proposer).contains(&"message:edit no".to_owned()),
        "a proposer cannot edit: {proposer}"
    );
    assert!(
        kinds(&proposer).contains(&"message:read once".to_owned()),
        "a proposer can read: {proposer}"
    );
    let findings = first_of_kind(&proposer, "findings");
    assert_eq!(findings["findings"][0]["id"], "F1");
    assert_eq!(findings["findings"][0]["note"], "N1");
    assert_eq!(findings["problem"], serde_json::Value::Null);

    assert_eq!(judge["review"]["reviewer"], 2);
    assert!(
        snapshot_points(&proposer).is_empty(),
        "a proposer may not edit: {proposer}"
    );
    assert_eq!(
        snapshot_points(&judge),
        ["before", "after"],
        "the judge edits: {judge}"
    );
    assert!(
        kinds(&judge).contains(&"message:judged: saw F1".to_owned()),
        "the judge is given the findings: {judge}"
    );
    assert!(kinds(&judge).contains(&"message:F1: accepted - said more".to_owned()));
    let got = kinds(&proposer)
        .into_iter()
        .find(|kind| kind.starts_with("message:got "))
        .unwrap();
    assert!(
        got.contains("a.txt\n\nprompt 1"),
        "the reviewed reply, its messages a blank line apart: {got}"
    );
    assert!(!site.runner().unwrap().reviewing(), "the judge ends it");
    assert_eq!(
        states(&site),
        [serde_json::json!({ "kind": "sent", "run": "R1" })],
        "a review marks no note sent"
    );

    let (status, body) = review("R2");
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("is itself a review"), "{body}");
    assert_eq!(
        get(&site, &format!("{RUN}/review")).0,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

/// Saves Fake (`author`) as the agent notes go to and `reviewers`, each a (name,
/// scenario) pair, as the reviewers in order.
fn set_reviewers(site: &Site, author: &str, reviewers: &[(&str, &str)]) {
    let mut custom = vec![serde_json::json!({ "name": "Fake", "command": fake_agent(author) })];
    custom.extend(reviewers.iter().map(
        |(name, scenario)| serde_json::json!({ "name": name, "command": fake_agent(scenario) }),
    ));
    let mut enabled = vec!["Fake"];
    enabled.extend(reviewers.iter().map(|(name, _)| *name));
    let settings = serde_json::json!({
        "customAgents": custom,
        "enabledAgents": enabled,
        "agent": "Fake",
        "reviewers": reviewers.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
    });
    let (status, body) = save(site, &settings.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Waits until run `id` is the latest and has ended, and returns it.
fn wait_for_run(site: &Site, nudges: &mpsc::Receiver<()>, id: &str) -> serde_json::Value {
    loop {
        let run = get_json(site, RUN);
        if run["id"] == id && ["ended", "failed"].contains(&run["status"].as_str().unwrap_or("")) {
            return run;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("no change while waiting for {id}: {run}"));
    }
}

#[test]
fn a_reviewer_sees_the_reviewed_run_s_edits() {
    let repo = TempRepo::new("run-review-edits");
    repo.write("src/a.rs", "fn a() {\n    one();\n}\n");
    let (site, nudges) = run_site(&repo, "turn");
    post_note(&site, "src/a.rs", 2, 2, "rename one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    set_reviewers(&site, "turn", &[("Reviewer", "reviewer")]);
    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );
    let proposer = wait_for_run(&site, &nudges, "R2");
    assert!(
        kinds(&proposer).contains(&"message:saw the edit: true".to_owned()),
        "the run's diff, before and after, is in the prompt: {proposer}"
    );
    let linked = kinds(&proposer)
        .into_iter()
        .find(|kind| kind.starts_with("message:got "))
        .unwrap();
    assert!(
        linked.ends_with(&format!(" | {}", file_uri_of(&repo, "src/a.rs"))),
        "the edited file is linked once: {linked}"
    );
}

#[test]
fn a_review_stops_at_a_reviewer_that_fails_finds_nothing_readable_or_edits() {
    for (scenario, problem) in [
        // Nothing readable is no finding, so the judge has nothing to judge.
        ("reviewer-silent", Some("no ```json block")),
        ("refuse-prompt", None),
        ("reviewer-edits", Some("reviewer 1 changed files")),
    ] {
        let repo = TempRepo::new(&format!("run-review-stop-{scenario}"));
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        set_reviewers(
            &site,
            "plain",
            &[("First", scenario), ("Judge", "reviewer")],
        );
        let body = serde_json::json!({ "of": "R1" }).to_string();
        assert_eq!(
            post(&site, &format!("{RUN}/review"), &body).0,
            StatusCode::OK
        );
        let first = wait_for_run(&site, &nudges, "R2");
        if let Some(problem) = problem {
            let found = first_of_kind(&first, "findings");
            assert!(
                found["problem"].as_str().unwrap_or("").contains(problem),
                "{scenario}: {found}"
            );
        }
        assert!(
            !site.runner().unwrap().reviewing(),
            "{scenario}: no judge starts"
        );
        assert_eq!(
            get_json(&site, RUNS)["runs"],
            serde_json::json!(["R2", "R1"]),
            "{scenario}"
        );
    }
}

#[test]
fn a_send_that_fails_between_two_stages_leaves_the_review_going() {
    if skip_as_root() {
        return;
    }
    let repo = TempRepo::new("run-review-refused-send");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    set_reviewers(
        &site,
        "plain",
        &[("First", "reviewer"), ("Judge", "reviewer")],
    );
    let release = site.runner().unwrap().hold_next_stage();
    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );
    wait_for_run(&site, &nudges, "R2");
    assert!(site.runner().unwrap().reviewing(), "the judge is next");

    // A send between the proposer's end and the judge's start, whose log cannot be made.
    let runs = repo.root.join(".git/postil/runs");
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o555)).unwrap();
    let (status, body) = send(&site, &["N1"]);
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    drop(release);

    let judge = loop {
        let run = get_json(&site, RUN);
        if run["review"]["reviewer"] == 2 && run["status"] == "ended" {
            break run;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("no judge after the refused send: {run}"));
    };
    assert!(
        kinds(&judge).contains(&"message:judged: saw F1".to_owned()),
        "{judge}"
    );
}

#[test]
fn a_single_reviewer_only_proposes_and_a_send_after_a_review_starts_a_fresh_agent() {
    let repo = TempRepo::new("run-review-single");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 2, 2, "two");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    // The reviewer is the very agent notes go to: Fake.
    let settings = serde_json::json!({
        "customAgents": [{ "name": "Fake", "command": fake_agent("plain") }],
        "enabledAgents": ["Fake"],
        "agent": "Fake",
        "reviewers": ["Fake"],
    });
    assert_eq!(save(&site, &settings.to_string()).0, StatusCode::OK);
    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );
    let proposer = wait_for_run(&site, &nudges, "R2");
    assert_eq!(proposer["review"]["reviewers"], 1);
    assert_eq!(
        get_json(&site, RUNS)["runs"],
        serde_json::json!(["R2", "R1"]),
        "no judge"
    );
    assert!(!site.runner().unwrap().reviewing());

    // The kept agent is the reviewer's, told not to change files: a send starts afresh.
    send(&site, &["N2"]);
    let next = wait_for_run(&site, &nudges, "R3");
    assert!(
        kinds(&next).contains(&"message:prompt 1".to_owned()),
        "a new agent's first prompt: {next}"
    );
}

#[test]
fn a_follow_up_goes_to_the_same_conversation_or_tells_a_fresh_agent_the_thread() {
    let repo = TempRepo::new("run-follow");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 2, 2, "two");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    let follow = |of: &str, text: &str| {
        let body = serde_json::json!({ "of": of, "text": text }).to_string();
        post(&site, &format!("{RUN}/follow"), &body)
    };

    let (status, body) = follow("R1", "  and more  ");
    assert_eq!(status, StatusCode::OK, "{body}");
    let second = wait_for_run(&site, &nudges, "R2");
    assert_eq!(
        second["followUp"],
        serde_json::json!({ "of": "R1", "text": "and more" })
    );
    assert_eq!(
        second["notes"],
        serde_json::json!(["N1"]),
        "its thread's notes"
    );
    assert_eq!(
        kinds(&second)[1..3],
        ["message:got and more", "message:prompt 2"],
        "the kept agent's second prompt, the engineer's words alone: {second}"
    );
    assert_eq!(
        states(&site),
        [
            serde_json::json!({ "kind": "sent", "run": "R1" }),
            serde_json::json!({ "kind": "open" })
        ],
        "a follow-up marks no note sent"
    );

    // Another send, R3, goes to the kept agent. Its conversation goes on past R2, so a
    // follow-up of R2 starts a fresh agent and tells it the thread.
    send(&site, &["N2"]);
    let other = wait_for_run(&site, &nudges, "R3");
    assert!(
        kinds(&other).contains(&"message:prompt 3".to_owned()),
        "{other}"
    );
    let (status, body) = follow("R2", "again");
    assert_eq!(status, StatusCode::OK, "{body}");
    let fresh = wait_for_run(&site, &nudges, "R4");
    let got = kinds(&fresh)
        .into_iter()
        .find(|kind| kind.starts_with("message:got "))
        .unwrap();
    for part in [
        "its conversation is gone",
        "R1 was asked:\n\nThese are review notes",
        "R2 was asked:\n\nand more\n\nIt answered:\n\ngot and more\n\nprompt 2",
        "The engineer now asks:\n\nagain",
    ] {
        assert!(got.contains(part), "{part:?} in {got}");
    }
    assert!(
        kinds(&fresh).contains(&"message:prompt 1".to_owned()),
        "{fresh}"
    );
    assert_eq!(
        fresh["followUp"]["text"], "again",
        "the engineer's words, not the prompt"
    );
}

#[test]
fn a_follow_up_is_refused_for_a_review_a_followed_run_or_no_text() {
    let repo = TempRepo::new("run-follow-refused");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    let body = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/follow"), &body).0,
        StatusCode::OK
    );
    wait_for_run(&site, &nudges, "R2");

    // A review reviews the run a thread started from, not a follow-up.
    set_reviewers(&site, "plain", &[("Reviewer", "reviewer-silent")]);
    let review = |of: &str| {
        let body = serde_json::json!({ "of": of }).to_string();
        post(&site, &format!("{RUN}/review"), &body)
    };
    let (status, body) = review("R2");
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("R2 is a follow-up of R1"), "{body}");
    assert_eq!(review("R1").0, StatusCode::OK);
    wait_for_run(&site, &nudges, "R3");

    for (body, status, why) in [
        (
            r#"{"of": "R2", "text": "  "}"#,
            StatusCode::BAD_REQUEST,
            "has no text",
        ),
        (
            r#"{"of": "R3", "text": "x"}"#,
            StatusCode::BAD_REQUEST,
            "R3 is a review",
        ),
        (
            r#"{"of": "R1", "text": "x"}"#,
            StatusCode::BAD_REQUEST,
            "R2 already follows R1: follow up R2",
        ),
        (r#"{"of": "R9", "text": "x"}"#, StatusCode::NOT_FOUND, "R9"),
        (
            r#"{"of": "R2"}"#,
            StatusCode::BAD_REQUEST,
            "unreadable request",
        ),
    ] {
        let (got, reply) = post(&site, &format!("{RUN}/follow"), body);
        assert_eq!(got, status, "{body}: {reply}");
        assert!(reply.contains(why), "{body}: {reply}");
    }
    assert_eq!(
        get(&site, &format!("{RUN}/follow")).0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        get_json(&site, RUNS)["runs"],
        serde_json::json!(["R3", "R2", "R1"]),
        "nothing refused made a run"
    );
}

#[test]
fn a_follow_up_of_a_log_that_follows_itself_or_a_newer_run_is_refused() {
    let repo = TempRepo::new("run-follow-loop");
    let (site, _nudges) = run_site(&repo, "plain");
    let runs = repo.root.join(".git/postil/runs");
    fs::create_dir_all(&runs).unwrap();
    let follows = |file: &str, id: &str, of: &str| {
        let log = header(id).replace(
            r#""version":1"#,
            &format!(r#""version":3,"followUp":{{"of":"{of}","text":"x"}}"#),
        );
        fs::write(runs.join(format!("{file}.jsonl")), format!("{log}\n")).unwrap();
    };
    // R2 follows itself; R3 and R4 follow each other; R6.jsonl names itself R7, so a
    // check of the header's id would let it follow R6 again and again; R8 follows an id
    // that is not one.
    follows("R2", "R2", "R2");
    follows("R3", "R3", "R4");
    follows("R4", "R4", "R3");
    follows("R6", "R7", "R6");
    follows("R8", "R8", "R08");
    for (of, problem) in [
        ("R2", "R2: it follows R2"),
        ("R4", "R3: it follows R4"),
        ("R6", "R6: it follows R6"),
        ("R8", "R8: it follows R08"),
    ] {
        let body = serde_json::json!({ "of": of, "text": "more" }).to_string();
        let (status, reply) = post(&site, &format!("{RUN}/follow"), &body);
        assert_eq!(status, StatusCode::NOT_FOUND, "{of}: {reply}");
        assert!(reply.contains(problem), "{of}: {reply}");
        assert!(reply.contains("which is not an older run"), "{reply}");
    }
}

#[test]
fn a_run_s_changes_are_the_diff_between_its_snapshots_and_the_index_is_left_alone() {
    let repo = TempRepo::new("run-changes");
    repo.write("a.txt", "a\n");
    repo.write("secret.txt", "one\n");
    repo.git(&["add", "a.txt", "secret.txt"]);
    let (site, nudges) = run_site(&repo, "writes");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let first = wait_for(&site, &nudges, "ended");
    let snapshots: Vec<&serde_json::Value> = first["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] == "snapshot")
        .collect();
    assert_eq!(snapshots.len(), 2, "one before, one after: {first}");
    assert_eq!(
        first["entries"][0]["kind"], "snapshot",
        "before the agent starts"
    );
    let changes = |query: &str| get(&site, &format!("{RUN}/changes?{query}"));

    let (status, body) = changes("from=R1");
    assert_eq!(status, StatusCode::OK, "{body}");
    let one: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(one["before"], snapshots[0]["tree"]);
    assert_eq!(one["after"], snapshots[1]["tree"]);
    let patch = one["patch"].as_str().unwrap();
    for part in [
        "+++ b/a.txt",
        "+prompt 1",
        "new file mode",
        "+++ b/new.txt",
        "+new",
    ] {
        assert!(patch.contains(part), "{part:?} in {patch}");
    }

    // The engineer takes a file out of git and ignores it: snapshots stop following it. And
    // force-adds an ignored one: snapshots follow it from then on.
    repo.git(&["rm", "--cached", "--quiet", "secret.txt"]);
    repo.write(".gitignore", "secret.txt\nforced.txt\n");
    repo.write("secret.txt", "two\n");
    repo.write("forced.txt", "kept\n");
    repo.git(&["add", "--force", "forced.txt"]);

    // A follow-up writes again: R1 to R2 is the whole thread, R2 alone its own change.
    let body = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/follow"), &body).0,
        StatusCode::OK
    );
    wait_for_run(&site, &nudges, "R2");
    let patch_of = |query: &str| {
        let (status, body) = changes(query);
        assert_eq!(status, StatusCode::OK, "{query}: {body}");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["patch"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let thread = patch_of("from=R1&to=R2");
    assert!(
        thread.contains("+prompt 2") && !thread.contains("prompt 1"),
        "{thread}"
    );
    let second = patch_of("from=R2");
    assert!(second.contains("-prompt 1\n+prompt 2"), "{second}");
    assert!(!second.contains("new.txt"), "unchanged by R2: {second}");
    assert!(
        !second.contains("+two"),
        "an ignored file left untracked: {second}"
    );
    assert!(
        thread.contains("+++ b/forced.txt"),
        "a force-added file: {thread}"
    );

    // Snapshots go through an index of their own: the engineer's staged files are the same.
    let staged = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["diff", "--cached", "--name-only"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&staged.stdout),
        "a.txt\nforced.txt\n"
    );
}

#[test]
fn changes_compare_with_the_files_now_without_a_snapshot_after_and_refuse_bad_logs() {
    let repo = TempRepo::new("run-changes-logs");
    repo.write("a.txt", "a\n");
    let (site, _nudges) = run_site(&repo, "writes");
    let changes = |query: &str| get(&site, &format!("{RUN}/changes?{query}"));
    let runs = repo.root.join(".git/postil/runs");
    fs::create_dir_all(&runs).unwrap();
    let tree = serde_json::json!(site.repo().unwrap().snapshot().unwrap());

    // A tracked, ignored file deleted but not staged stays in the real index only: the
    // snapshot index is not copied again for it.
    repo.write(".gitignore", "gen.lock\n");
    repo.write("gen.lock", "x\n");
    repo.git(&["add", "--force", "gen.lock"]);
    let open = site.repo().unwrap();
    let copies = open.index_copies();
    open.snapshot().unwrap();
    assert_eq!(
        open.index_copies(),
        copies + 1,
        "a force-added ignored file copies the index"
    );
    fs::remove_file(repo.root.join("gen.lock")).unwrap();
    open.snapshot().unwrap();
    open.snapshot().unwrap();
    assert_eq!(
        open.index_copies(),
        copies + 1,
        "copied again for a deleted file"
    );

    // A run with only the snapshot before it is compared with the files now.
    let log = header("R5").replace(r#""version":1"#, r#""version":4"#);
    fs::write(
        runs.join("R5.jsonl"),
        format!("{log}\n{{\"kind\":\"snapshot\",\"point\":\"before\",\"tree\":{tree}}}\n"),
    )
    .unwrap();
    repo.write("a.txt", "now\n");
    let (status, body) = changes("from=R5");
    assert_eq!(status, StatusCode::OK, "{body}");
    let now: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(now["after"], serde_json::Value::Null);
    assert!(now["patch"].as_str().unwrap().contains("+now"), "{now}");

    let old = header("R4").replace(r#""version":1"#, r#""version":3"#);
    fs::write(runs.join("R4.jsonl"), format!("{old}\n")).unwrap();
    let blank = header("R7").replace(r#""version":1"#, r#""version":4"#);
    fs::write(
        runs.join("R7.jsonl"),
        format!("{blank}\n{{\"kind\":\"snapshot\",\"point\":\"before\",\"tree\":\"\"}}\n"),
    )
    .unwrap();
    // A tree id is passed to git, so one edited into an option is refused, and so is none.
    let edited = header("R6").replace(r#""version":1"#, r#""version":4"#);
    fs::write(
        runs.join("R6.jsonl"),
        format!(
            "{edited}\n{{\"kind\":\"snapshot\",\"point\":\"before\",\"tree\":\"--output=x\"}}\n"
        ),
    )
    .unwrap();
    for (query, status, why) in [
        ("from=R4", StatusCode::BAD_REQUEST, "R4 has no snapshot"),
        (
            "from=R6",
            StatusCode::BAD_REQUEST,
            "expected a git object id",
        ),
        (
            "from=R7",
            StatusCode::BAD_REQUEST,
            "expected a git object id",
        ),
        (
            "from=R5&to=R4",
            StatusCode::BAD_REQUEST,
            "R4 is older than R5",
        ),
        ("to=R1", StatusCode::BAD_REQUEST, "expected ?from="),
        ("from=R9", StatusCode::NOT_FOUND, "R9"),
    ] {
        let (got, body) = changes(query);
        assert_eq!(got, status, "{query}: {body}");
        assert!(body.contains(why), "{query}: {body}");
    }
    let (status, _) = post(&site, &format!("{RUN}/changes?from=R1"), "");
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[test]
fn a_snapshot_that_cannot_be_taken_is_the_run_s_problem_and_the_run_goes_on() {
    if skip_as_root() {
        return;
    }
    let repo = TempRepo::new("run-no-snapshot");
    repo.write("a.txt", "a\n");
    repo.git(&["add", "a.txt"]);
    let index = repo.root.join(".git/index");
    // The first snapshot starts from a copy of the real index, which cannot be read.
    fs::set_permissions(&index, fs::Permissions::from_mode(0o000)).unwrap();
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let ended = wait_for(&site, &nudges, "ended");
    fs::set_permissions(&index, fs::Permissions::from_mode(0o644)).unwrap();
    let problem = ended["problem"].as_str().unwrap_or_default();
    assert!(
        problem.contains("no snapshot of the files was taken"),
        "{ended}"
    );
    assert!(problem.contains(".git/index"), "{problem}");
    assert!(
        kinds(&ended).contains(&"message:prompt 1".to_owned()),
        "{ended}"
    );
    let (status, body) = get(&site, &format!("{RUN}/changes?from=R1"));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("R1 has no snapshot from before it"), "{body}");

    // Postil's own index torn: it is copied again from the real one at once.
    let ours = repo.root.join(".git/postil/snapshot/index");
    fs::write(&ours, "torn").unwrap();
    send(&site, &["N1"]);
    let healed = wait_for_run(&site, &nudges, "R2");
    assert_eq!(snapshot_points(&healed), ["before", "after"], "{healed}");

    // The real index torn: its copy cannot be used, so the snapshot fails and the copy is
    // removed rather than failing every snapshot after it.
    fs::remove_file(&ours).unwrap();
    fs::write(&index, "torn").unwrap();
    send(&site, &["N1"]);
    let torn = wait_for_run(&site, &nudges, "R3");
    let problem = torn["problem"].as_str().unwrap_or_default();
    assert!(problem.contains("no snapshot"), "{torn}");
    assert!(!ours.exists(), "the unusable copy is gone");
}

#[test]
fn a_run_can_be_cancelled_and_one_runs_at_a_time() {
    let repo = TempRepo::new("run-cancel");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "cancel");
    post_note(&site, "a.txt", 1, 1, "one");
    let cancel = format!("{RUN}/cancel");
    let (status, body) = post(&site, &cancel, "");
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "nothing to cancel yet: {body}"
    );

    send(&site, &["N1"]);
    wait_for(&site, &nudges, "waiting");
    let (status, body) = send(&site, &["N1"]);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("still working"), "{body}");
    let follow = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/follow"), &follow);
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a follow-up waits too: {body}"
    );

    let (status, body) = post(&site, &cancel, "");
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = wait_for(&site, &nudges, "ended");
    assert_eq!(
        kinds(&ended)[3..],
        [
            "question",
            "cancelRequested",
            "answer",
            "message:question cancel",
            "message:late permission cancelled",
            "message:late question cancel",
            "ended"
        ],
        "the open question, and the ones that crossed the cancel, are answered cancel"
    );
    assert_eq!(entries(&ended)[5]["option"], serde_json::Value::Null);
    assert_eq!(entries(&ended)[9]["reason"], "cancelled");

    let late = serde_json::json!({ "request": 0, "content": {} });
    let (status, body) = post(&site, &format!("{RUN}/question"), &late.to_string());
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a cancelled question: {body}"
    );
}

#[test]
fn an_agent_that_fails_to_start_fails_the_run_and_leaves_the_notes_open() {
    let repo = TempRepo::new("run-fail");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "exit-on-start");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let failed = wait_for(&site, &nudges, "failed");
    let error = entries(&failed)[0]["error"].as_str().unwrap().to_owned();
    assert!(
        error.contains("Please sign in: run `fake login`"),
        "the agent's own words: {error}"
    );
    assert_eq!(states(&site), [serde_json::json!({ "kind": "open" })]);

    set_agents(&site, &[("Fake", &fake_agent("auth"))], "Fake");
    send(&site, &["N1"]);
    let failed = wait_for(&site, &nudges, "failed");
    assert_eq!(failed["id"], "R2");
    let error = entries(&failed)[0]["error"].as_str().unwrap().to_owned();
    assert!(error.contains("needs you to sign in"), "{error}");

    set_agents(&site, &[("Ghost", "no-such-agent-program --acp")], "Ghost");
    send(&site, &["N1"]);
    let failed = wait_for(&site, &nudges, "failed");
    let error = entries(&failed)[0]["error"].as_str().unwrap().to_owned();
    assert!(
        error.contains("cannot find the agent's program no-such-agent-program"),
        "{error}"
    );
}

#[test]
fn an_agent_that_exits_mid_run_fails_it_and_is_started_again_next_time() {
    let repo = TempRepo::new("run-exit");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "exit-mid-turn");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let failed = wait_for(&site, &nudges, "failed");
    let last = failed["entries"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["error"], "the agent exited: panicked at the model");

    send(&site, &["N1"]);
    let again = wait_for(&site, &nudges, "failed");
    assert_eq!(again["id"], "R2");
    assert!(
        kinds(&again).contains(&"message:prompt 1".to_owned()),
        "a new process: {again}"
    );
}

#[test]
fn closing_the_project_stops_its_agent() {
    let repo = TempRepo::new("run-close");
    let other = TempRepo::new("run-close-other");
    repo.write("a.txt", "a\n");
    let pid_file = repo.root.join(".git/agent-child.pid");
    let command = format!("{} {}", fake_agent("spawns-child"), pid_file.display());
    let (site, nudges) = run_site(&repo, "plain");
    set_agents(&site, &[("Fake", &command)], "Fake");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    let pid = fs::read_to_string(&pid_file).unwrap();
    assert!(alive(&pid), "the agent is kept after its run");

    site.open(Repo::open(&other.root).unwrap(), LiveRun::Stop)
        .unwrap();
    assert!(!alive(&pid), "opening another project stopped it");
    assert_eq!(get(&site, RUN).1, "null", "the new project has no run");
}

fn header(id: &str) -> String {
    let fields = r#""agent":"Fake","notes":["N1"],"prompt":"p","startedAt":1"#;
    format!(r#"{{"version":1,"id":"{id}",{fields}}}"#)
}

#[test]
fn a_log_without_an_end_reads_as_unfinished_and_torn_lines_spoil_only_themselves() {
    let repo = TempRepo::new("run-torn");
    let runs = repo.root.join(".git/postil/runs");
    fs::create_dir_all(&runs).unwrap();
    // A message whose text a crash cut inside a multi-byte character, then a torn line.
    let mut log = format!("{}\n{{\"kind\":\"started\"}}\n", header("R3")).into_bytes();
    log.extend_from_slice(b"{\"kind\":\"message\",\"text\":\"caf\xC3\"}\n{\"kind\":\"mess");
    fs::write(runs.join("R3.jsonl"), log).unwrap();
    fs::write(runs.join("notes.txt"), "not a run\n").unwrap();

    let site = repo.site();
    let run = get_json(&site, RUN);
    assert_eq!(run["id"], "R3", "the newest log");
    assert_eq!(run["status"], "unfinished");
    assert_eq!(
        run["entries"],
        serde_json::json!([
            { "kind": "started" },
            { "kind": "message", "text": "caf\u{FFFD}" },
        ])
    );
    let problem = run["problem"].as_str().unwrap();
    assert!(problem.contains("line 4 of"), "{problem}");

    // ext4 and XFS can extend a file with zeros after a crash.
    let mut log = format!("{}\n{{\"kind\":\"started\"}}\n", header("R3")).into_bytes();
    log.extend_from_slice(&[0; 64]);
    fs::write(runs.join("R3.jsonl"), log).unwrap();
    let run = get_json(&site, RUN);
    assert_eq!(run["entries"], serde_json::json!([{ "kind": "started" }]));
    assert!(
        run["problem"].as_str().unwrap().contains("line 3 of"),
        "{run}"
    );
}

#[test]
fn an_unreadable_newest_log_is_skipped_and_named() {
    let repo = TempRepo::new("run-skip");
    let runs = repo.root.join(".git/postil/runs");
    fs::create_dir_all(&runs).unwrap();
    fs::write(runs.join("R2.jsonl"), format!("{}\n", header("R2"))).unwrap();
    let site = repo.site();

    // Empty: another window is about to write its header, so it is skipped quietly.
    fs::write(runs.join("R3.jsonl"), "").unwrap();
    let run = get_json(&site, RUN);
    assert_eq!(
        (run["id"].as_str(), run["problem"].as_str()),
        (Some("R2"), None)
    );

    for (newest, why) in [
        ("\n", "its first line is not a run header"),
        ("not a header\n", "its first line is not a run header"),
        (
            r#"{"version":5,"id":"R3","newField":1}"#,
            "it is version 5, and this build of Postil reads versions 1 to 4",
        ),
        (
            &*header("R3").replace(
                r#""startedAt":1"#,
                concat!(
                    r#""startedAt":1,"review":{"of":"R1","reviewer":1,"reviewers":1},"#,
                    r#""followUp":{"of":"R2","text":"x"}"#,
                ),
            ),
            "its header is both a review and a follow-up",
        ),
    ] {
        fs::write(runs.join("R3.jsonl"), newest).unwrap();
        let run = get_json(&site, RUN);
        assert_eq!(run["id"], "R2", "{newest:?}");
        let problem = run["problem"].as_str().unwrap();
        assert!(problem.contains("R3.jsonl is not shown"), "{problem}");
        assert!(problem.contains(why), "{problem}");
    }

    fs::remove_file(runs.join("R2.jsonl")).unwrap();
    assert_eq!(get(&site, RUN).1, "null", "nothing readable is no run");

    // A runs directory that cannot be listed is an error, not "no runs".
    fs::remove_dir_all(&runs).unwrap();
    fs::write(&runs, "not a directory").unwrap();
    let (status, body) = get(&site, RUN);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("cannot read the run log"), "{body}");
}

#[test]
fn a_run_log_that_cannot_be_created_refuses_the_send() {
    if skip_as_root() {
        return;
    }
    let repo = TempRepo::new("run-readonly");
    repo.write("a.txt", "a\n");
    let (site, _nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    let postil = repo.root.join(".git/postil");
    fs::set_permissions(&postil, fs::Permissions::from_mode(0o555)).unwrap();
    let (status, body) = send(&site, &["N1"]);
    fs::set_permissions(&postil, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("cannot write the run log"), "{body}");
    assert_eq!(get(&site, RUN).1, "null");
}

/// Whether the process `pid` runs, or has exited and is not reaped yet.
fn alive(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Waits until the process `pid` is gone: exited and reaped.
///
/// This polls, because no signal exists for it: the agent is the runner's child, not the
/// test's, so the test cannot wait on it, and the runner reaps it on a thread of its own.
fn wait_for_exit(pid: &str) {
    let deadline = Instant::now() + WAIT;
    while alive(pid) {
        assert!(Instant::now() < deadline, "the agent was not stopped");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_cancel_while_the_agent_starts_stops_it_and_ends_the_run_at_once() {
    let repo = TempRepo::new("run-cancel-start");
    repo.write("a.txt", "a\n");
    let pid_file = repo.root.join(".git/silent.pid");
    make_fifo(&pid_file);
    let (site, nudges) = run_site(&repo, "plain");
    let silent = format!("{} {}", fake_agent("silent"), pid_file.display());
    set_agents(
        &site,
        &[("Silent", &silent), ("Fake", &fake_agent("plain"))],
        "Silent",
    );
    post_note(&site, "a.txt", 1, 1, "one");

    send(&site, &["N1"]);
    let pid = read_fifo(&pid_file, WAIT);
    assert_eq!(get_json(&site, RUN)["status"], "starting");
    let (status, body) = post(&site, &format!("{RUN}/cancel"), "");
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = get_json(&site, RUN);
    assert_eq!(
        ended["status"], "ended",
        "at once, not after the start times out"
    );
    assert_eq!(entries(&ended)[1]["reason"], "cancelled");
    assert_eq!(states(&site), [serde_json::json!({ "kind": "open" })]);
    wait_for_exit(&pid);

    // Another send goes ahead; the stopped start reports nothing over it.
    let other = serde_json::json!({ "notes": ["N1"], "agent": "Fake" });
    let (status, body) = post(&site, RUN, &other.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
    let next = wait_for(&site, &nudges, "ended");
    assert_eq!(
        (next["id"].as_str(), next["agent"].as_str()),
        (Some("R2"), Some("Fake"))
    );
}

#[test]
fn closing_the_project_while_its_agent_starts_stops_the_agent() {
    let repo = TempRepo::new("run-close-start");
    let other = TempRepo::new("run-close-start-other");
    repo.write("a.txt", "a\n");
    let pid_file = repo.root.join(".git/silent.pid");
    make_fifo(&pid_file);
    let (site, _nudges) = run_site(&repo, "plain");
    let silent = format!("{} {}", fake_agent("silent"), pid_file.display());
    set_agents(&site, &[("Silent", &silent)], "Silent");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let pid = read_fifo(&pid_file, WAIT);

    site.open(Repo::open(&other.root).unwrap(), LiveRun::Stop)
        .unwrap();
    wait_for_exit(&pid);
}

#[test]
fn quitting_while_an_agent_starts_stops_the_agent() {
    let repo = TempRepo::new("run-quit-start");
    repo.write("a.txt", "a\n");
    let pid_file = repo.root.join(".git/silent.pid");
    make_fifo(&pid_file);
    let (site, _nudges) = run_site(&repo, "plain");
    let silent = format!("{} {}", fake_agent("silent"), pid_file.display());
    set_agents(&site, &[("Silent", &silent)], "Silent");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let pid = read_fifo(&pid_file, WAIT);

    // The window closing drops the site.
    drop(site);
    wait_for_exit(&pid);
}

#[test]
fn a_refused_prompt_leaves_the_notes_open_and_the_next_send_starts_a_fresh_agent() {
    let repo = TempRepo::new("run-refused");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "refuse-first");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let failed = wait_for(&site, &nudges, "failed");
    let last = failed["entries"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        last["error"],
        "the agent refused session/prompt: model overloaded (-32603)"
    );

    // The failed turn let the agent go (its session may be broken): a fresh one refuses
    // its first prompt again, where the kept one would have answered.
    send(&site, &["N1"]);
    let again = wait_for(&site, &nudges, "failed");
    assert_eq!(again["id"], "R2");
    // Checked last: the first run's notes would be marked on its reader thread after the
    // run showed failed, and by now that thread is long done.
    assert_eq!(
        states(&site),
        [serde_json::json!({ "kind": "open" })],
        "the agent never took the turn"
    );
}

#[test]
fn notes_that_cannot_be_marked_sent_are_named_on_the_run() {
    if skip_as_root() {
        return;
    }
    let repo = TempRepo::new("run-unmarked");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");

    // The runs and snapshot directories stay writable; the review file's directory does not.
    let postil = repo.root.join(".git/postil");
    fs::set_permissions(&postil, fs::Permissions::from_mode(0o555)).unwrap();
    send(&site, &["N1"]);
    let run = wait_for(&site, &nudges, "ended");
    fs::set_permissions(&postil, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(run["id"], "R2");
    assert_eq!(snapshot_points(&run), ["before", "after"], "{run}");
    let problem = run["problem"].as_str().unwrap();
    assert!(problem.contains("could not be marked sent"), "{problem}");
    assert_eq!(
        states(&site),
        [serde_json::json!({ "kind": "sent", "run": "R1" })]
    );
}

#[test]
fn a_refusal_before_any_work_leaves_the_notes_open() {
    let repo = TempRepo::new("run-refusal");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "refusal");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let ended = wait_for(&site, &nudges, "ended");
    let last = ended["entries"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["reason"], "refusal");
    assert_eq!(states(&site), [serde_json::json!({ "kind": "open" })]);
}

#[test]
fn a_second_stop_ends_an_agent_that_does_not_stop() {
    let repo = TempRepo::new("run-hang");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "hang");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "running");
    let cancel = format!("{RUN}/cancel");
    assert_eq!(post(&site, &cancel, "").0, StatusCode::OK);
    let stopping = get_json(&site, RUN);
    assert_eq!(stopping["status"], "cancelling");
    assert!(stopping["forceIn"].as_u64().unwrap() > 0, "{stopping}");
    assert_eq!(post(&site, &cancel, "").0, StatusCode::OK);
    assert_eq!(
        get_json(&site, RUN)["status"],
        "cancelling",
        "a second Stop at once is the same request"
    );

    site.runner().unwrap().backdate_stop();
    assert_eq!(
        get_json(&site, RUN)["forceIn"],
        0,
        "the page is told the agent had its time"
    );
    assert_eq!(post(&site, &cancel, "").0, StatusCode::OK);
    let failed = get_json(&site, RUN);
    assert_eq!(
        failed["forceIn"],
        serde_json::Value::Null,
        "only while stopping"
    );
    assert_eq!(failed["status"], "failed");
    let last = failed["entries"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let error = last["error"].as_str().unwrap();
    assert!(error.contains("did not stop when asked"), "{error}");
    // Its exit is not reported, so the forced stop takes the snapshot after it.
    let changes = get_json(&site, &format!("{RUN}/changes?from=R1"));
    assert!(changes["after"].is_string(), "{changes}");

    // The working tree is free again, with a new agent: its first prompt.
    let (status, body) = send(&site, &["N1"]);
    assert_eq!(status, StatusCode::OK, "{body}");
    loop {
        let run = get_json(&site, RUN);
        if kinds(&run).contains(&"message:prompt 1".to_owned()) {
            assert_eq!(run["id"], "R2");
            break;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("the new agent never answered: {run}"));
    }
}

#[test]
fn another_window_cannot_start_a_run_on_the_same_working_tree() {
    let repo = TempRepo::new("run-two-windows");
    repo.write("a.txt", "a\n");
    let (first, nudges) = run_site(&repo, "hang");
    post_note(&first, "a.txt", 1, 1, "one");
    send(&first, &["N1"]);
    wait_for(&first, &nudges, "running");
    // Made after the first send: both windows read the one settings file.
    let (second, _second_nudges) = run_site(&repo, "plain");

    let (status, body) = send(&second, &["N1"]);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("another Postil window"), "{body}");

    // Once the first window's run ends, the second may run.
    let cancel = format!("{RUN}/cancel");
    post(&first, &cancel, "");
    first.runner().unwrap().backdate_stop();
    post(&first, &cancel, "");
    let (status, body) = send(&second, &["N1"]);
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn a_line_over_the_limit_is_reported_and_the_run_goes_on() {
    let repo = TempRepo::new("run-long-line");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "long-line");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let run = wait_for_run(&site, &nudges, "R1");
    assert_eq!(run["status"], "ended", "{}", run["status"]);
    let problems: Vec<String> = entries(&run)
        .iter()
        .filter(|entry| entry["kind"] == "unreadable")
        .map(|entry| entry["problem"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        problems.len(),
        2,
        "the rest of the long line is skipped, not read as a line: {problems:?}"
    );
    assert!(
        problems[0].starts_with("a line that is not JSON-RPC"),
        "a line of exactly the limit is read whole: {}",
        &problems[0][..80]
    );
    assert!(
        problems[1].starts_with("a line longer than 1048576 bytes, skipped: xxx"),
        "{}",
        &problems[1][..80]
    );
    let kinds = kinds(&run);
    assert!(
        kinds.contains(&"message:after the long line".to_owned()),
        "the line after it is read whole"
    );
}

#[test]
fn opening_during_a_run_keeps_it_for_the_same_project_and_asks_first_for_another() {
    let repo = TempRepo::new("run-open-live");
    let other = TempRepo::new("run-open-live-other");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "ask");
    post_note(&site, "a.txt", 1, 1, "one");
    let open = |path: &Path, stop: bool| {
        let body = serde_json::json!({ "path": path, "stop": stop }).to_string();
        post(&site, "postil://localhost/api/open", &body)
    };
    send(&site, &["N1"]);
    let waiting = wait_for(&site, &nudges, "waiting");

    // Dropping the open project on the window again: the run and its agent stay.
    assert_eq!(open(&repo.root, false).0, StatusCode::OK);
    assert_eq!(get_json(&site, RUN)["status"], "waiting");
    let (status, body) = open(&other.root, false);
    assert_eq!(status, StatusCode::CONFLICT, "another project: {body}");
    assert_eq!(get_json(&site, RUN)["status"], "waiting");
    let request = first_of_kind(&waiting, "question")["request"].clone();
    let content = serde_json::json!({ "question_0": "Yes" });
    let body = serde_json::json!({ "request": request, "content": content }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/question"), &body);
    assert_eq!(
        status,
        StatusCode::OK,
        "the agent that asked is still there: {body}"
    );
    wait_for(&site, &nudges, "ended");

    send(&site, &["N1"]);
    wait_for(&site, &nudges, "waiting");
    let (status, body) = open(&other.root, true);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        get_json(&site, "postil://localhost/api/repo")["root"],
        site_root(&other.root)
    );
    assert_eq!(get(&site, RUN).1, "null", "the new project has no run");

    // The stopped run says it was stopped, so it does not read as another window's.
    let stopped = fs::read_to_string(repo.root.join(".git/postil/runs/R2.jsonl")).unwrap();
    let end = stopped.lines().last().unwrap();
    assert!(
        end.contains("Postil closed the project while it ran"),
        "{end}"
    );
    // And it has the snapshot after it, so its changes view still shows what it did.
    assert!(
        stopped.contains(r#""kind":"snapshot","point":"after""#),
        "{stopped}"
    );
}

#[test]
fn a_review_between_stages_holds_the_project_and_one_that_failed_to_start_does_not() {
    let repo = TempRepo::new("run-open-review");
    let other = TempRepo::new("run-open-review-other");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    let open_other = || {
        post(
            &site,
            "postil://localhost/api/open",
            &open_request(&other.root),
        )
    };
    let review = |of: &str| {
        let body = serde_json::json!({ "of": of }).to_string();
        assert_eq!(
            post(&site, &format!("{RUN}/review"), &body).0,
            StatusCode::OK
        );
    };
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");

    // A reviewer whose agent speaks another protocol version never starts.
    set_reviewers(&site, "plain", &[("Broken", "version-2")]);
    review("R1");
    assert_eq!(wait_for_run(&site, &nudges, "R2")["status"], "failed");
    let (status, body) = open_other();
    assert_eq!(status, StatusCode::OK, "nothing runs: {body}");

    let (status, body) = post(
        &site,
        "postil://localhost/api/open",
        &open_request(&repo.root),
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    set_reviewers(
        &site,
        "plain",
        &[("First", "reviewer"), ("Judge", "reviewer")],
    );
    let release = site.runner().unwrap().hold_next_stage();
    review("R1");
    wait_for_run(&site, &nudges, "R3");
    let (status, body) = open_other();
    assert_eq!(status, StatusCode::CONFLICT, "the judge is next: {body}");
    drop(release);
}

#[test]
fn a_kept_agent_that_exited_is_started_again() {
    let repo = TempRepo::new("run-dead-kept");
    repo.write("a.txt", "a\n");
    let pid_file = repo.root.join(".git/agent.pid");
    let (site, nudges) = run_site(&repo, "plain");
    let command = format!("{} {}", fake_agent("exit-after-turn"), pid_file.display());
    set_agents(&site, &[("Fake", &command)], "Fake");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    // The exited agent is reaped when the runner drops it on its exit event, so once it
    // is gone the runner holds no agent.
    wait_for_exit(&fs::read_to_string(&pid_file).unwrap());

    send(&site, &["N1"]);
    let second = wait_for(&site, &nudges, "ended");
    assert_eq!(second["id"], "R2");
    assert!(
        kinds(&second).contains(&"message:prompt 1".to_owned()),
        "a new process: {second}"
    );
}

#[test]
fn a_send_right_after_the_kept_agent_exited_starts_a_fresh_one() {
    let repo = TempRepo::new("run-dead-at-once");
    repo.write("a.txt", "a\n");
    let pid_file = repo.root.join(".git/agent.pid");
    // The agent's grandchild writes this once the agent has exited.
    let exited = repo.root.join(".git/agent.pid.exited");
    make_fifo(&exited);
    let (site, nudges) = run_site(&repo, "plain");
    let command = format!(
        "{} {}",
        fake_agent("exit-leaving-stderr"),
        pid_file.display()
    );
    set_agents(&site, &[("Fake", &command)], "Fake");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    // Exited, and not yet reaped: the reader waits out its grace for standard error,
    // which the agent's grandchild holds open, so no exit event has come.
    read_fifo(&exited, WAIT);

    send(&site, &["N1"]);
    let second = wait_for_run(&site, &nudges, "R2");
    assert_eq!(second["status"], "ended", "{second}");
    assert!(
        kinds(&second).contains(&"message:prompt 1".to_owned()),
        "a new process: {second}"
    );
}

#[test]
fn run_ids_continue_after_the_highest_log_on_disk() {
    let repo = TempRepo::new("run-ids");
    repo.write("a.txt", "a\n");
    let runs = repo.root.join(".git/postil/runs");
    fs::create_dir_all(&runs).unwrap();
    let header =
        r#"{"version":1,"id":"R9","agent":"Fake","notes":["N1"],"prompt":"p","startedAt":1}"#;
    fs::write(runs.join("R9.jsonl"), format!("{header}\n")).unwrap();
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    assert_eq!(wait_for(&site, &nudges, "ended")["id"], "R10");
}

#[test]
fn a_damaged_run_counter_is_repaired_by_the_next_run() {
    // A crash leaves a zero-filled tail; damage from outside can leave bytes that are not
    // UTF-8. Neither parses, so both read as no number.
    for (case, damage) in [("zeros", &b"5\0\0"[..]), ("not-utf8", b"5\0\xff")] {
        let repo = TempRepo::new(&format!("run-counter-{case}"));
        repo.write("a.txt", "a\nb\n");
        let runs = repo.root.join(".git/postil/runs");
        fs::create_dir_all(&runs).unwrap();
        fs::write(runs.join("active.lock"), damage).unwrap();
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");
        send(&site, &["N1"]);
        assert_eq!(wait_for(&site, &nudges, "ended")["id"], "R1", "{case}");

        // R1 goes with N1; its number must not come back, which needs the counter readable.
        delete(&site, &format!("{NOTES}?id=N1"));
        send(&site, &["N2"]);
        assert_eq!(wait_for(&site, &nudges, "ended")["id"], "R2", "{case}");
    }
}

#[test]
fn bad_run_requests_name_the_problem_and_make_no_run() {
    let repo = TempRepo::new("run-bad");
    repo.write("a.txt", "a\n");
    let (site, _nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");

    let refused = |body: &str, status: StatusCode, words: &str| {
        let (got, reply) = post(&site, RUN, body);
        assert_eq!(got, status, "{body}: {reply}");
        assert!(reply.contains(words), "{body}: {reply}");
    };
    refused(
        r#"{"notes": []}"#,
        StatusCode::BAD_REQUEST,
        "no notes were chosen",
    );
    refused(
        r#"{"notes": ["N1", "N1"]}"#,
        StatusCode::BAD_REQUEST,
        "chosen twice",
    );
    refused(
        r#"{"notes": ["N7"]}"#,
        StatusCode::NOT_FOUND,
        "there is no note N7",
    );
    refused(
        r#"{"notes": ["x"]}"#,
        StatusCode::BAD_REQUEST,
        "expected N and a number",
    );
    refused(
        r#"{"notes": ["N1"], "extra": 1}"#,
        StatusCode::BAD_REQUEST,
        "unreadable",
    );
    refused(
        r#"{"agent": "Nobody", "notes": ["N1"]}"#,
        StatusCode::BAD_REQUEST,
        "turn Nobody on in Settings",
    );
    let (status, body) = save(&site, r#"{"customAgents": [], "enabledAgents": []}"#);
    assert_eq!(status, StatusCode::OK, "{body}");
    refused(
        r#"{"notes": ["N1"]}"#,
        StatusCode::BAD_REQUEST,
        "choose the agent notes are sent to",
    );
    assert_eq!(get(&site, RUN).1, "null", "a refused send makes no run");

    assert_eq!(
        get(&site, &format!("{RUN}?after=x")).0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&site, &format!("{RUN}/preview")).0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&site, &format!("{RUN}/cancel")).0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        get(&site, &format!("{RUN}/permission")).0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        post(&site, &format!("{RUN}/preview?notes=N1"), "").0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let answer = r#"{"request": 0, "option": "once"}"#;
    let (status, body) = post(&site, &format!("{RUN}/permission"), answer);
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no run, so no request: {body}"
    );
}
