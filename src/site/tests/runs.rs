//! Runs (`run.rs`, `run/log.rs`) through `/api/run*`, against the fake ACP agent in
//! `tests/fake-agent.mjs`.

use super::*;

/// How a prompt of notes starts, before the notes: the introduction, then the ask for the
/// author's report.
fn prompt_start() -> String {
    format!(
        "These are review notes an engineer wrote while reading this repository.\n\
         Address every note. Change nothing the notes do not ask for.\n\
         When you finish, list each note id with one line on what you did.\n{}",
        crate::report::ASK
    )
}

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

/// The `file://` URI of `path` in the worktree of thread `thread` (its first run, `R1`), as
/// a prompt links it.
fn file_uri_of(site: &Site, thread: &str, path: &str) -> String {
    let thread = get_json(site, &format!("postil://localhost/api/thread?id={thread}"));
    format!("file://{}/{path}", thread["path"].as_str().unwrap())
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
                file_uri_of(&site, "R1", "b.txt"),
                file_uri_of(&site, "R1", "src/a.rs")
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
            // The `turn` scenario's reply holds no report: the run says so, and ends well.
            "noReport",
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
        preview["prompt"].as_str().unwrap(),
        prompt_start()
            + "\n\
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
    let clean = get_json(&site, &format!("{RUN}/preview?notes=N1"));
    assert_eq!(clean["uncommitted"], 0, "{clean}");
    repo.write("src/a.rs", "fn a() {\n    one();\n// moved\n");
    repo.write("b.txt", "not noted\n");
    // An untracked folder counts once, as `git status` shows it, whatever it holds.
    fs::create_dir_all(repo.root.join("vendor")).unwrap();
    repo.write("vendor/one.rs", "1\n");
    repo.write("vendor/two.rs", "2\n");

    let preview = get_json(&site, &format!("{RUN}/preview?notes=N2,N1"));
    // The thread starts from the last commit: the send step says what it will not see.
    assert_eq!(preview["uncommitted"], 3, "{preview}");
    assert_eq!(preview["uncommittedNoted"], serde_json::json!(["src/a.rs"]));
    assert_eq!(
        preview["prompt"].as_str().unwrap(),
        prompt_start()
            + "\n\
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

    // A noted file renamed away is still named: the agent's copy has it at the old path.
    repo.git(&["add", "--all"]);
    repo.git(&["mv", "src/a.rs", "src/c.rs"]);
    let renamed = get_json(&site, &format!("{RUN}/preview?notes=N1"));
    assert_eq!(
        renamed["uncommittedNoted"],
        serde_json::json!(["src/a.rs"]),
        "{renamed}"
    );

    // A rename only the working tree has (`git add -N`) is one change, in the second column.
    repo.git(&["commit", "--quiet", "--message", "moved"]);
    fs::rename(repo.root.join("src/c.rs"), repo.root.join("src/d.rs")).unwrap();
    repo.git(&["add", "-N", "src/d.rs"]);
    let moved = get_json(&site, &format!("{RUN}/preview?notes=N1"));
    assert_eq!(moved["uncommitted"], 1, "{moved}");

    // A git status that fails says why, and the prompt is still there.
    fs::write(repo.root.join(".git/index"), "not an index").unwrap();
    let unknown = get_json(&site, &format!("{RUN}/preview?notes=N1"));
    assert!(unknown["uncommitted"].is_null(), "{unknown}");
    assert!(
        unknown["uncommittedError"]
            .as_str()
            .unwrap()
            .contains("index"),
        "{unknown}"
    );
    assert!(unknown["prompt"].as_str().unwrap().contains("[N1]"));
}

#[test]
fn a_note_in_an_untracked_folder_is_named_as_changed() {
    let repo = TempRepo::new("run-preview-untracked");
    repo.write("a.txt", "a\n");
    let (site, _nudges) = run_site(&repo, "plain");
    fs::create_dir_all(repo.root.join("vendor")).unwrap();
    repo.write("vendor/one.rs", "1\n");
    post_note(&site, "vendor/one.rs", 1, 1, "new here");
    // Committed and unchanged: not named.
    post_note(&site, "a.txt", 1, 1, "as it was");

    let preview = get_json(&site, &format!("{RUN}/preview?notes=N1,N2"));

    // `git status` names only `vendor/`: the agent's copy lacks the file it holds.
    assert_eq!(preview["uncommitted"], 1, "{preview}");
    assert_eq!(
        preview["uncommittedNoted"],
        serde_json::json!(["vendor/one.rs"])
    );
}

#[test]
fn each_send_works_in_a_worktree_and_on_a_branch_of_its_own_and_leaves_the_engineer_s_alone() {
    let repo = TempRepo::new("run-worktrees");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "writes");
    // Uncommitted work of the engineer's: a thread starts from the last commit, without it.
    repo.write("mine.txt", "draft\n");
    post_note(&site, "a.txt", 1, 1, "Fix the first line");
    post_note(&site, "a.txt", 2, 2, "two");

    send(&site, &["N1"]);
    let first = wait_for(&site, &nudges, "ended");
    send(&site, &["N2"]);
    let second = wait_for_run(&site, &nudges, "R2");
    assert!(
        kinds(&second).contains(&"message:prompt 1".to_owned()),
        "a new thread's agent is a fresh one: {second}"
    );

    let thread = |id: &str| get_json(&site, &format!("postil://localhost/api/thread?id={id}"));
    let (one, two) = (thread("R1"), thread("R2"));
    assert_eq!(one["branch"], "postil/r1-fix-the-first-line");
    assert_eq!(two["branch"], "postil/r2-two");
    assert_eq!(one["state"], "open");
    assert_eq!(one["base"], "trunk", "the branch HEAD was on");
    let one_path = PathBuf::from(one["path"].as_str().unwrap());
    assert_ne!(one_path, PathBuf::from(two["path"].as_str().unwrap()));
    assert!(
        one_path.starts_with(repo.home().canonicalize().unwrap()),
        "{one_path:?} under Postil's home"
    );
    // In a folder named after the repository, then a hash of its path.
    let folder = one_path
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    let repo_name = repo.root.file_name().unwrap().to_str().unwrap();
    assert!(folder.starts_with(&format!("{repo_name}-")), "{folder}");
    // Each agent wrote in its own worktree, and its turn was committed on its branch.
    assert_eq!(
        fs::read_to_string(one_path.join("a.txt")).unwrap(),
        "a\nprompt 1\n"
    );
    assert!(
        !one_path.join("mine.txt").exists(),
        "not committed: not there"
    );
    let after = first["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "snapshot" && entry["point"] == "after")
        .unwrap()["tree"]
        .clone();
    assert_eq!(
        git_out(&repo.root, &["rev-parse", "postil/r1-fix-the-first-line"]),
        after
    );
    assert_eq!(one["tip"], after, "the record follows the branch");
    assert_eq!(
        git_out(&repo.root, &["log", "-1", "--format=%s", "postil/r2-two"]),
        "Postil R2: notes N2"
    );
    assert_eq!(git_out(&one_path, &["status", "--porcelain"]), "");

    // The engineer's working tree, index and branch are as they were.
    assert_eq!(
        fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        "a\nb\n"
    );
    assert!(!repo.root.join("new.txt").exists());
    assert_eq!(git_out(&repo.root, &["branch", "--show-current"]), "trunk");
    assert_eq!(
        git_out(&repo.root, &["status", "--porcelain"]),
        "?? mine.txt"
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
    stop(&site);
    site.runner().unwrap().backdate_stop();
    stop(&site);
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
    let (status, body) =
        answer(serde_json::json!({ "run": waiting["id"], "request": 99, "content": {} }));
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
        &serde_json::json!({ "run": waiting["id"], "request": request, "option": "once" })
            .to_string(),
    );
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a question is no permission request: {body}"
    );

    let content = serde_json::json!({ "question_0": "Yes", "question_0_custom": "and log it" });
    let (status, body) =
        answer(serde_json::json!({ "run": waiting["id"], "request": request, "content": content }));
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
    let (status, body) =
        answer(serde_json::json!({ "run": waiting["id"], "request": request, "content": null }));
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
    // The same agent, kept for a follow-up: its second turn reports the reply to the first's
    // question.
    let body = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/follow"), &body).0,
        StatusCode::OK
    );
    let ended = wait_for_run(&site, &nudges, "R2");
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
        let body =
            serde_json::json!({ "run": waiting["id"], "request": request, "option": option });
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
        "agent": { "name": "Fake" },
        "reviewers": [{ "name": "Reviewer" }, { "name": "Reviewer" }],
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
    // A review stage logs findings, or judges them, and never a report: its json block is
    // its findings.
    for stage in [&proposer, &judge] {
        assert_eq!(report_entry(stage), serde_json::Value::Null, "{stage}");
    }

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
pub(super) fn set_reviewers(site: &Site, author: &str, reviewers: &[(&str, &str)]) {
    let mut custom = vec![serde_json::json!({ "name": "Fake", "command": fake_agent(author) })];
    custom.extend(reviewers.iter().map(
        |(name, scenario)| serde_json::json!({ "name": name, "command": fake_agent(scenario) }),
    ));
    let mut enabled = vec!["Fake"];
    enabled.extend(reviewers.iter().map(|(name, _)| *name));
    let slots: Vec<_> = reviewers
        .iter()
        .map(|(name, _)| serde_json::json!({ "name": name }))
        .collect();
    let settings = serde_json::json!({
        "customAgents": custom,
        "enabledAgents": enabled,
        "agent": { "name": "Fake" },
        "reviewers": slots,
    });
    let (status, body) = save(site, &settings.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
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
        linked.ends_with(&format!(" | {}", file_uri_of(&site, "R1", "src/a.rs"))),
        "the edited file is linked once: {linked}"
    );
}

/// The reviewed run's report reaches its reviewer: the changes it rated as needing no judgment
/// are named for the reviewer to check, and the author's ask for a report is not passed on.
#[test]
fn a_reviewer_is_asked_to_check_the_report_s_low_ratings() {
    let repo = TempRepo::new("run-review-report");
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "1\n2\n3\n4\n");
    let (site, nudges) = run_site(&repo, "report");
    post_note(&site, "a.txt", 1, 1, "change a");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    set_reviewers(&site, "report", &[("Reviewer", "reviewer")]);
    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );
    let proposer = wait_for_run(&site, &nudges, "R2");
    let prompt = proposer["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("- a.txt:1-2 (routine)\n- b.txt (mechanical)\n"),
        "{prompt}"
    );
    // The notes as quoted, before the author's reply: the fake author echoes its own prompt
    // in its reply, which reviewers are shown whole.
    let quoted = prompt
        .split_once("as the agent received them:")
        .and_then(|(_, rest)| rest.split_once("The agent's reply:"))
        .unwrap()
        .0;
    assert!(quoted.contains("change a"), "{quoted}");
    assert!(
        !quoted.contains(crate::report::ASK),
        "a reviewer ends with its findings, not a report: {quoted}"
    );
}

#[test]
fn a_review_stops_at_a_reviewer_that_fails_finds_nothing_readable_or_edits() {
    for (scenario, problem) in [
        // Nothing readable is no finding, so the judge has nothing to judge.
        ("reviewer-silent", Some("no ```json block")),
        ("refuse-prompt", None),
        ("reviewer-edits", Some("reviewer 1 changed files")),
        // An edit through a command reports nothing; the worktree shows it.
        ("reviewer-writes", Some("reviewer 1 changed files")),
        // An edit it committed leaves no change; the branch moved.
        ("reviewer-commits", Some("reviewer 1 changed files")),
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
            // Undone, so neither the next run's commit nor Apply takes the reviewer's edit.
            let thread = get_json(&site, "postil://localhost/api/thread?id=R1");
            let worktree = PathBuf::from(thread["path"].as_str().unwrap());
            if scenario != "reviewer-silent" {
                assert_eq!(
                    git_out(&worktree, &["status", "--porcelain"]),
                    "",
                    "{scenario}"
                );
                assert_eq!(
                    git_out(&worktree, &["rev-parse", "HEAD"]),
                    thread["tip"],
                    "{scenario}"
                );
                assert_eq!(
                    fs::read_to_string(worktree.join("a.txt")).unwrap(),
                    "a\n",
                    "{scenario}"
                );
            }
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
fn the_judge_s_edits_are_kept() {
    let repo = TempRepo::new("run-review-judge-edits");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    // The proposer finds one thing; the judge, which may change files, writes a.txt.
    set_reviewers(
        &site,
        "plain",
        &[("First", "reviewer"), ("Judge", "writes")],
    );
    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );

    let judge = wait_for_run(&site, &nudges, "R3");

    assert_eq!(judge["status"], "ended", "{judge}");
    assert_eq!(judge["problem"], serde_json::Value::Null, "{judge}");
    let thread = get_json(&site, "postil://localhost/api/thread?id=R1");
    let worktree = PathBuf::from(thread["path"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("a.txt")).unwrap(),
        "a\nprompt 1\n"
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
        "agent": { "name": "Fake" },
        "reviewers": [{ "name": "Fake" }],
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
    // The engineer's words, then the reminder to end with a report: the words alone would
    // leave a kept agent's follow-up without one.
    let reminded = |words: &str| format!("message:got {words}\n\n{}", crate::report::ASK_AGAIN);
    assert_eq!(
        kinds(&second)[1..3],
        [reminded("and more"), "message:prompt 2".to_owned()],
        "the kept agent's second prompt: {second}"
    );
    assert_eq!(
        states(&site),
        [
            serde_json::json!({ "kind": "sent", "run": "R1" }),
            serde_json::json!({ "kind": "open" })
        ],
        "a follow-up marks no note sent"
    );

    // Another send, R3, starts a thread of its own, in its own worktree, with an agent of its
    // own. The first thread keeps its agent, so a follow-up of R2 still reaches it.
    send(&site, &["N2"]);
    let other = wait_for_run(&site, &nudges, "R3");
    assert!(
        kinds(&other).contains(&"message:prompt 1".to_owned()),
        "{other}"
    );
    let (status, body) = follow("R2", "still there?");
    assert_eq!(status, StatusCode::OK, "{body}");
    let kept = wait_for_run(&site, &nudges, "R4");
    assert_eq!(
        kinds(&kept)[1..3],
        [reminded("still there?"), "message:prompt 3".to_owned()],
        "{kept}"
    );

    // A third thread makes the window's third agent: the idle one used least recently, R3's,
    // goes. A follow-up of R3 then starts a fresh agent and tells it the thread.
    post_note(&site, "a.txt", 1, 2, "three");
    send(&site, &["N3"]);
    wait_for_run(&site, &nudges, "R5");
    let (status, body) = follow("R3", "again");
    assert_eq!(status, StatusCode::OK, "{body}");
    let fresh = wait_for_run(&site, &nudges, "R6");
    let got = kinds(&fresh)
        .into_iter()
        .find(|kind| kind.starts_with("message:got "))
        .unwrap();
    for part in [
        "its conversation is gone",
        "R3 was asked:\n\nThese are review notes",
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

    // The engineer stages a change of their own meanwhile.
    repo.write("secret.txt", "two\n");
    repo.git(&["add", "secret.txt"]);

    // A follow-up writes again: R1 to R2 is the whole thread, R2 alone its own change.
    let body = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/follow"), &body).0,
        StatusCode::OK
    );
    let follow_up = wait_for_run(&site, &nudges, "R2");
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
        !thread.contains("secret.txt"),
        "the engineer's own change is not the thread's: {thread}"
    );

    // Each snapshot after a run is a commit on the thread's branch, the follow-up's last.
    let after = |run: &serde_json::Value| {
        run["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kind"] == "snapshot" && entry["point"] == "after")
            .unwrap()["tree"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let branch = get_json(&site, "postil://localhost/api/thread?id=R1")["branch"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        git_out(&repo.root, &["rev-parse", &branch]),
        after(&follow_up)
    );
    assert_eq!(
        git_out(&repo.root, &["rev-parse", &format!("{branch}~1")]),
        after(&first)
    );
    // The engineer's staged files are as they left them.
    assert_eq!(
        git_out(&repo.root, &["diff", "--cached", "--name-only"]),
        "secret.txt"
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
fn a_commit_that_fails_is_the_run_s_problem_and_its_work_stays_in_the_worktree() {
    let repo = TempRepo::new("run-no-commit");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "writes");
    // An empty name, which git refuses: every commit after this one fails, the worktree's
    // too, since it shares this config. Signing and hooks cannot fail it: Postil turns both
    // off for its commits.
    repo.git(&["config", "user.name", ""]);
    repo.git(&["config", "commit.gpgSign", "true"]);
    repo.git(&["config", "gpg.program", "false"]);
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let ended = wait_for(&site, &nudges, "ended");

    let problem = ended["problem"].as_str().unwrap_or_default();
    assert!(
        problem.contains("no snapshot of the files was taken") && problem.contains("commit"),
        "{ended}"
    );
    assert!(
        kinds(&ended).contains(&"message:prompt 1".to_owned()),
        "the run went on: {ended}"
    );
    assert_eq!(snapshot_points(&ended), ["before"], "{ended}");
    // With no snapshot after it, the run is compared with the worktree's files as they are.
    let changes = get_json(&site, &format!("{RUN}/changes?from=R1"));
    assert_eq!(changes["after"], serde_json::Value::Null);
    assert!(
        changes["patch"].as_str().unwrap().contains("+prompt 1"),
        "{changes}"
    );
    let thread = get_json(&site, "postil://localhost/api/thread?id=R1");
    assert_eq!(thread["tip"], thread["fork"], "nothing was committed");

    // The uncommitted work would be lost by ending the thread, so Postil refuses, naming it.
    for ending in ["apply", "discard"] {
        let (status, body) = post(
            &site,
            &format!("postil://localhost/api/thread/{ending}"),
            r#"{"thread": "R1"}"#,
        );
        assert_eq!(status, StatusCode::CONFLICT, "{ending}: {body}");
        assert!(
            body.contains("holds work Postil did not commit") && body.contains("a.txt"),
            "{body}"
        );
    }
    assert_eq!(fs::read_to_string(repo.root.join("a.txt")).unwrap(), "a\n");

    // As the refusal says: committing the work there keeps it with the thread, and Apply
    // takes the engineer's commit.
    let worktree = PathBuf::from(thread["path"].as_str().unwrap());
    repo.git(&["config", "user.name", "Postil Test"]);
    repo.git(&["config", "commit.gpgSign", "false"]);
    git_out(&worktree, &["add", "--all"]);
    git_out(&worktree, &["commit", "--quiet", "--message", "mine"]);
    let (status, body) = post(
        &site,
        "postil://localhost/api/thread/apply",
        r#"{"thread": "R1"}"#,
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        "a\nprompt 1\n"
    );
}

/// A snapshot of the files as they are (`Repo::snapshot`), which the changes view takes of a
/// worktree with no commit after its run, heals its own index and drops one git cannot use.
#[test]
fn a_snapshot_copies_its_index_again_when_torn_and_drops_a_copy_git_cannot_use() {
    if skip_as_root() {
        return;
    }
    let repo = TempRepo::new("snapshot-index");
    repo.write("a.txt", "a\n");
    repo.git(&["add", "a.txt"]);
    let index = repo.root.join(".git/index");
    let tree = Repo::open(&repo.root).unwrap();

    // The first snapshot starts from a copy of the real index, which cannot be read.
    fs::set_permissions(&index, fs::Permissions::from_mode(0o000)).unwrap();
    let error = tree.snapshot().unwrap_err();
    fs::set_permissions(&index, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(error_chain(&error).contains(".git/index"), "{error:?}");

    // Postil's own index torn: it is copied again from the real one at once.
    let first = tree.snapshot().unwrap();
    let ours = repo.root.join(".git/postil/snapshot/index");
    fs::write(&ours, "torn").unwrap();
    assert_eq!(tree.snapshot().unwrap(), first);

    // A file taken out of git and ignored is no longer followed; an ignored file force-added
    // is, from then on. `git add --all` alone would do neither.
    repo.write("secret.txt", "one\n");
    repo.git(&["add", "secret.txt"]);
    tree.snapshot().unwrap();
    repo.git(&["rm", "--cached", "--quiet", "secret.txt"]);
    repo.write(".gitignore", "secret.txt\nforced.txt\n");
    repo.write("forced.txt", "kept\n");
    repo.git(&["add", "--force", "forced.txt"]);
    let listed = git_out(
        &repo.root,
        &["ls-tree", "--name-only", &tree.snapshot().unwrap()],
    );
    assert_eq!(listed, ".gitignore\na.txt\nforced.txt");

    // The real index torn: its copy cannot be used, so the snapshot fails and the copy is
    // removed rather than failing every snapshot after it.
    fs::remove_file(&ours).unwrap();
    fs::write(&index, "torn").unwrap();
    assert!(tree.snapshot().is_err());
    assert!(!ours.exists(), "the unusable copy is gone");
}

#[test]
fn a_run_can_be_cancelled_and_its_thread_takes_one_run_at_a_time() {
    let repo = TempRepo::new("run-cancel");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "cancel");
    post_note(&site, "a.txt", 1, 1, "one");
    let cancel = serde_json::json!({ "run": "R1" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/cancel"), &cancel);
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "nothing to cancel yet: {body}"
    );

    send(&site, &["N1"]);
    wait_for(&site, &nudges, "waiting");
    let follow = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/follow"), &follow);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("still working"), "{body}");

    let (status, body) = stop(&site);
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

    let late = serde_json::json!({ "run": "R1", "request": 0, "content": {} });
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

/// Each tool call of the run, in order, with its command and exit code as the page folds them
/// (`foldRun`, ui/run.js): a `tool` entry replaces the call whole, and an update's fields
/// replace the call's.
fn executions(run: &serde_json::Value) -> Vec<(String, Option<String>, Option<i64>)> {
    let mut calls: Vec<(String, Option<String>, Option<i64>)> = Vec::new();
    for entry in entries(run) {
        if entry["kind"] != "tool" && entry["kind"] != "toolUpdate" {
            continue;
        }
        let id = entry["id"].as_str().unwrap();
        let index = calls
            .iter()
            .position(|call| call.0 == id)
            .unwrap_or_else(|| {
                calls.push((id.to_owned(), None, None));
                calls.len() - 1
            });
        if entry["kind"] == "tool" {
            calls[index] = (id.to_owned(), None, None);
        }
        if let Some(command) = entry["command"].as_str() {
            calls[index].1 = Some(command.to_owned());
        }
        if let Some(exit) = entry["exit"].as_i64() {
            calls[index].2 = Some(exit);
        }
    }
    calls
}

/// Claude's adapter and Grok report a command in different shapes (tests/fake-agent.mjs,
/// "checks"); both are kept, and no other call's input is.
#[test]
fn an_execute_call_keeps_its_command_and_exit_code_and_no_other_call_does() {
    let repo = TempRepo::new("run-checks");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "checks");
    post_note(&site, "a.txt", 1, 1, "check it");
    let (status, body) = send(&site, &["N1"]);
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = wait_for(&site, &nudges, "ended");

    // "echo " and 2600 two-byte characters: 5205 bytes, cut at the last whole character
    // before byte 4096.
    let cut = format!("echo {}… (5205 bytes in all)", "é".repeat(2045));
    let call = |id: &str, command: Option<&str>, exit: Option<i64>| {
        (id.to_owned(), command.map(str::to_owned), exit)
    };
    assert_eq!(
        executions(&ended),
        [
            // The update's command replaces the one before it, spacing and all; output that
            // starts like Claude's failure text is no exit code on a completed call.
            call("c1", Some("cargo  test"), None),
            // Grok names the kind on an update, and ends a failure as completed.
            call("c2", Some("cargo build"), Some(101)),
            call("c3", Some(&cut), Some(2)),
            // A finished exit code of 0 is kept: a success Grok confirmed.
            call("c4", Some("make"), Some(0)),
            // A read's input and output, though it names a command and an exit code.
            call("r1", None, None),
            call("e1", None, None),
        ]
    );
    let c2_exits: Vec<_> = entries(&ended)
        .iter()
        .filter(|entry| entry["id"] == "c2")
        .filter_map(|entry| entry["exit"].as_i64())
        .collect();
    assert_eq!(c2_exits, [101], "no exit code while the command runs");

    drop(site);
    assert_read_back(&repo, &ended);
}

/// A log written before commands were kept reads, its execute call without one.
#[test]
fn a_version_4_log_reads_without_commands() {
    let repo = TempRepo::new("run-version-4");
    let runs = repo.root.join(".git/postil/runs");
    fs::create_dir_all(&runs).unwrap();
    let header = header("R1").replace(r#""version":1"#, r#""version":4"#);
    let tool = concat!(
        r#"{"kind":"tool","id":"c1","title":"cargo test","tool":"execute","#,
        r#""status":"completed","places":[],"output":[]}"#,
    );
    fs::write(
        runs.join("R1.jsonl"),
        format!("{header}\n{tool}\n{{\"kind\":\"ended\",\"reason\":\"end_turn\"}}\n"),
    )
    .unwrap();

    let run = get_json(&repo.site(), RUN);
    assert_eq!(run["problem"], serde_json::Value::Null, "{run}");
    assert_eq!(executions(&run), [("c1".to_owned(), None, None)]);
}

/// The run's last `report` or `noReport` entry, or null when it has none.
fn report_entry(run: &serde_json::Value) -> serde_json::Value {
    entries(run)
        .into_iter()
        .rfind(|entry| entry["kind"] == "report" || entry["kind"] == "noReport")
        .unwrap_or_default()
}

/// The agent edits and deletes files and ends with its report (tests/fake-agent.mjs,
/// `report`): it is logged before the run's end, paths made relative to the repository, the
/// deleted file's step covering the whole file, and what cannot be read counted.
#[test]
fn a_run_ends_with_its_report() {
    let repo = TempRepo::new("run-report");
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "1\n2\n3\n4\n");
    let (site, nudges) = run_site(&repo, "report");
    post_note(&site, "a.txt", 1, 1, "change a");
    let (status, body) = send(&site, &["N1"]);
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = wait_for(&site, &nudges, "ended");

    // The block as the reply holds it, for the page to leave out.
    let mut entry = report_entry(&ended);
    let block = entry["report"]
        .as_object_mut()
        .unwrap()
        .remove("block")
        .unwrap();
    let block = block.as_str().unwrap();
    assert!(
        block.starts_with("```json\n{") && block.ends_with("}\n```"),
        "{block}"
    );
    let replied = kinds(&ended)
        .into_iter()
        .any(|kind| kind == format!("message:{block}"));
    assert!(replied, "the agent's own message: {ended}");
    assert_eq!(
        entry,
        serde_json::json!({
            "kind": "report",
            "report": {
                "steps": [
                    { "path": "a.txt", "lines": { "start": 1, "end": 2 }, "why": "the change",
                      "risk": "routine" },
                    { "path": "b.txt", "why": "no longer used", "risk": "mechanical" },
                    { "path": "a.txt", "lines": { "start": 2, "end": 2 }, "why": "absolute",
                      "risk": "eyes" },
                ],
                "checks": [{ "command": "cargo test", "result": "passed" }],
                // A path outside, lines that are no range, a result that is no claim.
                "cut": 3,
            },
        })
    );
    let kinds = kinds(&ended);
    assert_eq!(
        kinds[kinds.len() - 2..],
        ["report", "ended"],
        "logged before the end"
    );
    assert_eq!(
        states(&site)[0]["kind"],
        "sent",
        "the note is marked sent as before"
    );

    drop(site);
    assert_read_back(&repo, &ended);
}

/// No report, or one that cannot be read, is said on the run, which still ends well.
#[test]
fn a_reply_with_no_report_or_a_bad_one_ends_the_run_well_and_says_why() {
    for (scenario, problem) in [
        ("plain", "the reply has no ```json block"),
        ("report-bad", "the report's steps are not a list"),
    ] {
        let repo = TempRepo::new(&format!("run-report-{scenario}"));
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, scenario);
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let ended = wait_for(&site, &nudges, "ended");
        assert_eq!(ended["status"], "ended", "{scenario}: {ended}");
        assert_eq!(
            report_entry(&ended),
            serde_json::json!({ "kind": "noReport", "problem": problem }),
            "{scenario}"
        );
    }
}

/// Two reports with text between and after: the last is the report.
#[test]
fn the_last_report_block_wins() {
    let repo = TempRepo::new("run-report-twice");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "report-twice");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let ended = wait_for(&site, &nudges, "ended");
    assert_eq!(
        report_entry(&ended)["report"],
        serde_json::json!({
            "steps": [],
            "checks": [{ "command": "second", "result": "failed" }],
            "cut": 0,
            "block": concat!(
                "```json\n",
                r#"{"checks": [{"command": "second", "result": "failed"}]}"#,
                "\n```",
            ),
        })
    );
}

/// A run stopped after its agent wrote a report reads none: a stopped turn's reply can end
/// anywhere.
#[test]
fn a_stopped_run_reads_no_report() {
    let repo = TempRepo::new("run-report-stopped");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "report-cancel");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    // Stopped once the report is written.
    loop {
        let run = get_json(&site, RUN);
        if kinds(&run).iter().any(|kind| kind.contains("```json")) {
            break;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("no report written: {run}"));
    }
    let (status, body) = stop(&site);
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = wait_for(&site, &nudges, "ended");
    assert_eq!(report_entry(&ended), serde_json::Value::Null, "{ended}");
}

/// A log written before reports were kept reads: a run that ended well with no report entry,
/// its command kept. It shares its path with the version 4 test, and is kept because every
/// version bump adds a test that the version before it reads (the owner's rule, 2026-11-25).
#[test]
fn a_version_5_log_reads_without_a_report() {
    let repo = TempRepo::new("run-version-5");
    let runs = repo.root.join(".git/postil/runs");
    fs::create_dir_all(&runs).unwrap();
    let header = header("R1").replace(r#""version":1"#, r#""version":5"#);
    let tool = concat!(
        r#"{"kind":"tool","id":"c1","title":"cargo test","tool":"execute","#,
        r#""status":"completed","places":[],"output":[],"command":"cargo test","exit":0}"#,
    );
    fs::write(
        runs.join("R1.jsonl"),
        format!("{header}\n{tool}\n{{\"kind\":\"ended\",\"reason\":\"end_turn\"}}\n"),
    )
    .unwrap();

    let run = get_json(&repo.site(), RUN);
    assert_eq!(run["problem"], serde_json::Value::Null, "{run}");
    assert_eq!(run["status"], "ended", "{run}");
    assert_eq!(
        executions(&run),
        [("c1".to_owned(), Some("cargo test".to_owned()), Some(0))]
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
            r#"{"version":8,"id":"R3","newField":1}"#,
            "it is version 8, and this build of Postil reads versions 1 to 7",
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
    let (status, body) = stop(&site);
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

    // The runs and the threads stay writable; the notes' lock, which marking them takes,
    // does not.
    let lock = repo.root.join(".git/postil/review.lock");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o444)).unwrap();
    send(&site, &["N1"]);
    let run = wait_for(&site, &nudges, "ended");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
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
    assert_eq!(stop(&site).0, StatusCode::OK);
    let stopping = get_json(&site, RUN);
    assert_eq!(stopping["status"], "cancelling");
    assert!(stopping["forceIn"].as_u64().unwrap() > 0, "{stopping}");
    assert_eq!(stop(&site).0, StatusCode::OK);
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
    assert_eq!(stop(&site).0, StatusCode::OK);
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
fn another_window_cannot_start_a_run_in_a_thread_whose_agent_works_here() {
    let repo = TempRepo::new("run-two-windows");
    repo.write("a.txt", "a\n");
    let (first, nudges) = run_site(&repo, "hang");
    post_note(&first, "a.txt", 1, 1, "one");
    post_note(&first, "a.txt", 1, 1, "two");
    send(&first, &["N1"]);
    wait_for(&first, &nudges, "running");
    // Made after the first send: both windows read the one settings file.
    let (second, second_nudges) = run_site(&repo, "plain");
    let follow = serde_json::json!({ "of": "R1", "text": "more" }).to_string();

    let (status, body) = post(&second, &format!("{RUN}/follow"), &follow);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("another Postil window"), "{body}");
    // A thread of its own runs beside it.
    let (status, body) = send(&second, &["N2"]);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        wait_for_run(&second, &second_nudges, "R2")["status"],
        "ended"
    );

    // Once the first window's run ends, the second may run in its thread.
    stop(&first);
    first.runner().unwrap().backdate_stop();
    stop(&first);
    let (status, body) = post(&second, &format!("{RUN}/follow"), &follow);
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn a_fourth_live_agent_in_a_repository_is_refused_in_any_window() {
    let repo = TempRepo::new("run-cap");
    repo.write("a.txt", "a\n");
    let (first, nudges) = run_site(&repo, "hang");
    for text in ["one", "two", "three", "four"] {
        post_note(&first, "a.txt", 1, 1, text);
    }
    for (note, run) in [("N1", "R1"), ("N2", "R2"), ("N3", "R3")] {
        send(&first, &[note]);
        assert_eq!(wait_for(&first, &nudges, "running")["id"], run);
    }
    assert_eq!(
        get_json(&first, RUNS)["live"],
        serde_json::json!([
            { "id": "R1", "thread": "R1" },
            { "id": "R2", "thread": "R2" },
            { "id": "R3", "thread": "R3" }
        ])
    );
    let (second, _second_nudges) = run_site(&repo, "plain");

    for window in [&first, &second] {
        let (status, body) = send(window, &["N4"]);
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body.contains("3 agents already work in this repository, in the threads of R1, R2, R3"),
            "{body}"
        );
    }

    // A run that ends frees its place.
    let body = serde_json::json!({ "run": "R2" }).to_string();
    assert_eq!(
        post(&first, &format!("{RUN}/cancel"), &body).0,
        StatusCode::OK
    );
    first.runner().unwrap().backdate_stop();
    assert_eq!(
        post(&first, &format!("{RUN}/cancel"), &body).0,
        StatusCode::OK
    );
    for id in ["R1", "R3"] {
        let view = get_json(&first, &format!("{RUN}?id={id}"));
        assert_eq!(view["status"], "running", "only R2 was stopped: {id}");
    }
    let (status, body) = send(&second, &["N4"]);
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn each_answer_reaches_only_the_run_it_names() {
    let repo = TempRepo::new("run-answer-routing");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "ask");
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 1, 1, "two");
    send(&site, &["N1"]);
    assert_eq!(wait_for(&site, &nudges, "waiting")["id"], "R1");
    send(&site, &["N2"]);
    assert_eq!(wait_for(&site, &nudges, "waiting")["id"], "R2");
    assert_eq!(
        get_json(&site, RUNS)["live"],
        serde_json::json!([{ "id": "R1", "thread": "R1" }, { "id": "R2", "thread": "R2" }])
    );
    let question = |id: &str| {
        let view = get_json(&site, &format!("{RUN}?id={id}"));
        first_of_kind(&view, "question")["request"].clone()
    };
    let (first, second) = (question("R1"), question("R2"));
    // A run that does not exist names no live run, though two are live.
    let missing = serde_json::json!({ "run": "R9" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/cancel"), &missing);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    for id in ["R1", "R2"] {
        let view = get_json(&site, &format!("{RUN}?id={id}"));
        assert_eq!(view["status"], "waiting", "{id} was not stopped");
    }
    let answer = |run: &str, request: &serde_json::Value| {
        let body = serde_json::json!({ "run": run, "request": request, "content": null });
        post(&site, &format!("{RUN}/question"), &body.to_string())
    };

    let (status, body) = answer("R2", &second);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(wait_for_run(&site, &nudges, "R2")["status"], "ended");
    assert_eq!(
        get_json(&site, &format!("{RUN}?id=R1"))["status"],
        "waiting",
        "R2's answer left R1's question open"
    );
    // R2's agent has no question open: R1's number names none there.
    let (status, body) = answer("R2", &first);
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = answer("R1", &first);
    assert_eq!(status, StatusCode::OK, "{body}");
    // R1 is no longer the latest run, which `wait_for` follows: it is read by id.
    let deadline = Instant::now() + WAIT;
    while get_json(&site, &format!("{RUN}?id=R1"))["status"] != "ended" {
        assert!(Instant::now() < deadline, "R1 never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
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
    let body = serde_json::json!({ "run": waiting["id"], "request": request, "content": content })
        .to_string();
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
    let answer = r#"{"run": "R1", "request": 0, "option": "once"}"#;
    let (status, body) = post(&site, &format!("{RUN}/permission"), answer);
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no run, so no request: {body}"
    );
}

#[test]
fn a_review_keeps_what_the_engineer_committed_or_left_in_the_worktree() {
    let repo = TempRepo::new("run-review-keeps");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "writes");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    let worktree = PathBuf::from(
        get_json(&site, "postil://localhost/api/thread?id=R1")["path"]
            .as_str()
            .unwrap(),
    );
    // The engineer's own commit there, and an edit not committed.
    fs::write(worktree.join("mine.txt"), "mine\n").unwrap();
    git_out(&worktree, &["add", "mine.txt"]);
    git_out(&worktree, &["commit", "--quiet", "--message", "mine"]);
    fs::write(worktree.join("left.txt"), "left\n").unwrap();
    // A proposer that only reads.
    set_reviewers(&site, "writes", &[("Reviewer", "reviewer-silent")]);
    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );

    let review = wait_for_run(&site, &nudges, "R2");

    let found = review["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "findings")
        .unwrap()
        .clone();
    assert!(
        !found["problem"]
            .as_str()
            .unwrap_or_default()
            .contains("changed files"),
        "the reviewer is not blamed for the engineer's work: {found}"
    );
    assert_eq!(
        fs::read_to_string(worktree.join("mine.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        fs::read_to_string(worktree.join("left.txt")).unwrap(),
        "left\n"
    );
    let thread = get_json(&site, "postil://localhost/api/thread?id=R1");
    assert_eq!(thread["tip"], git_out(&worktree, &["rev-parse", "HEAD"]));
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
}

#[test]
fn a_reviewer_cut_short_has_its_edits_undone_all_the_same() {
    let repo = TempRepo::new("run-review-cut");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    set_reviewers(&site, "plain", &[("First", "reviewer-writes-cut")]);
    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );

    let stage = wait_for_run(&site, &nudges, "R2");

    let problem = stage["problem"].as_str().unwrap_or_default();
    assert!(problem.contains("its changes were undone"), "{stage}");
    let thread = get_json(&site, "postil://localhost/api/thread?id=R1");
    let worktree = PathBuf::from(thread["path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(worktree.join("a.txt")).unwrap(), "a\n");
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
}

#[test]
fn a_reviewer_ended_by_force_has_its_edits_undone() {
    let repo = TempRepo::new("run-review-forced");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    set_reviewers(&site, "plain", &[("First", "reviewer-writes-hangs")]);
    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );
    let thread = get_json(&site, "postil://localhost/api/thread?id=R1");
    let worktree = PathBuf::from(thread["path"].as_str().unwrap());
    let deadline = Instant::now() + WAIT;
    while fs::read_to_string(worktree.join("a.txt")).unwrap() != "rewritten\n" {
        assert!(Instant::now() < deadline, "the reviewer never wrote");
        std::thread::sleep(Duration::from_millis(10));
    }

    // The second Stop ends the agent, whose end is never reported: the undo runs there.
    assert_eq!(stop(&site).0, StatusCode::OK);
    site.runner().unwrap().backdate_stop();
    assert_eq!(stop(&site).0, StatusCode::OK);

    let stage = get_json(&site, &format!("{RUN}?id=R2"));
    assert_eq!(stage["status"], "failed", "{stage}");
    let problem = stage["problem"].as_str().unwrap_or_default();
    assert!(problem.contains("its changes were undone"), "{stage}");
    assert_eq!(fs::read_to_string(worktree.join("a.txt")).unwrap(), "a\n");
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
}

#[test]
fn a_kept_reviewer_that_exits_later_leaves_the_engineer_s_work_alone() {
    let repo = TempRepo::new("run-review-exits-later");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    // A proposer that finds nothing: the review ends with its agent kept, idle.
    let pid_file = repo.root.join(".git/reviewer.pid");
    let reviewer = format!("reviewer-silent {}", pid_file.display());
    set_reviewers(&site, "plain", &[("First", &reviewer)]);
    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );
    wait_for_run(&site, &nudges, "R2");
    let thread = get_json(&site, "postil://localhost/api/thread?id=R1");
    let worktree = PathBuf::from(thread["path"].as_str().unwrap());
    // The engineer's work there since, as the refusal to end a thread asks for.
    fs::write(worktree.join("mine.txt"), "mine\n").unwrap();
    git_out(&worktree, &["add", "mine.txt"]);
    git_out(&worktree, &["commit", "--quiet", "--message", "mine"]);
    fs::write(worktree.join("left.txt"), "left\n").unwrap();

    // The idle reviewer's agent exits on its own.
    let pid = fs::read_to_string(&pid_file).unwrap();
    assert!(
        Command::new("/bin/kill")
            .arg(&pid)
            .status()
            .unwrap()
            .success()
    );
    wait_for_exit(&pid);
    // Its exit is reported, then the page is told.
    nudges.recv_timeout(WAIT).unwrap();

    assert_eq!(git_out(&worktree, &["log", "-1", "--format=%s"]), "mine");
    assert_eq!(
        fs::read_to_string(worktree.join("left.txt")).unwrap(),
        "left\n"
    );
}

#[test]
fn a_window_lets_go_of_an_idle_agent_for_a_new_one_and_never_of_one_at_work() {
    let repo = TempRepo::new("run-evict");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "hang");
    set_reviewers(&site, "hang", &[("Plain", "plain")]);
    for text in ["one", "two", "three"] {
        post_note(&site, "a.txt", 1, 1, text);
    }
    let send_to = |agent: &str, note: &str| {
        let body = serde_json::json!({ "notes": [note], "agent": agent }).to_string();
        let (status, body) = post(&site, RUN, &body);
        assert_eq!(status, StatusCode::OK, "{body}");
    };
    // R1 works on, the oldest; R2 ends, its agent kept and idle.
    send_to("Fake", "N1");
    assert_eq!(wait_for(&site, &nudges, "running")["id"], "R1");
    send_to("Plain", "N2");
    assert_eq!(wait_for_run(&site, &nudges, "R2")["status"], "ended");

    // A third agent: R2's idle one goes, not R1's, used longer ago but at work.
    send_to("Plain", "N3");
    assert_eq!(wait_for_run(&site, &nudges, "R3")["status"], "ended");
    let follow = serde_json::json!({ "of": "R2", "text": "more" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/follow"), &follow);
    assert_eq!(status, StatusCode::OK, "{body}");
    let fresh = wait_for_run(&site, &nudges, "R4");
    assert!(
        kinds(&fresh).contains(&"message:prompt 1".to_owned()),
        "R2's agent was let go, so a fresh one takes the follow-up: {fresh}"
    );
    assert_eq!(
        get_json(&site, &format!("{RUN}?id=R1"))["status"],
        "running"
    );
}

#[test]
fn a_follow_up_takes_its_own_thread_s_place_so_two_others_running_leave_room() {
    let repo = TempRepo::new("run-cap-own");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "hang");
    set_reviewers(&site, "hang", &[("Plain", "plain")]);
    for text in ["one", "two", "three"] {
        post_note(&site, "a.txt", 1, 1, text);
    }
    let body = serde_json::json!({ "notes": ["N1"], "agent": "Plain" }).to_string();
    assert_eq!(post(&site, RUN, &body).0, StatusCode::OK);
    assert_eq!(wait_for_run(&site, &nudges, "R1")["status"], "ended");
    for (note, run) in [("N2", "R2"), ("N3", "R3")] {
        send(&site, &[note]);
        assert_eq!(wait_for(&site, &nudges, "running")["id"], run);
    }

    // R1's thread holds its own lock while the follow-up is made: that one is not counted.
    let follow = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/follow"), &follow);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(wait_for_run(&site, &nudges, "R4")["status"], "ended");
}

#[test]
fn a_thread_whose_agent_exited_takes_no_agent_s_place() {
    let repo = TempRepo::new("run-evict-count");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    set_reviewers(&site, "plain", &[("Gone", "exit-after-turn")]);
    for text in ["one", "two", "three"] {
        post_note(&site, "a.txt", 1, 1, text);
    }
    let send_to = |agent: &str, note: &str, run: &str| {
        let body = serde_json::json!({ "notes": [note], "agent": agent }).to_string();
        let (status, body) = post(&site, RUN, &body);
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(wait_for_run(&site, &nudges, run)["status"], "ended");
    };
    // R1's agent exits after its turn: its thread keeps a slot, with no agent in it.
    send_to("Gone", "N1", "R1");
    send_to("Fake", "N2", "R2");
    // R3's agent makes two in the window: R2's stays.
    send_to("Fake", "N3", "R3");

    let follow = serde_json::json!({ "of": "R2", "text": "more" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/follow"), &follow);
    assert_eq!(status, StatusCode::OK, "{body}");
    let kept = wait_for_run(&site, &nudges, "R4");
    assert!(
        kinds(&kept).contains(&"message:prompt 2".to_owned()),
        "R2's agent took the follow-up as its second prompt: {kept}"
    );
}

#[test]
fn a_reviewer_that_is_the_thread_s_own_agent_still_starts_fresh() {
    let repo = TempRepo::new("run-review-own-agent");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "reviewer");
    let settings = serde_json::json!({
        "customAgents": [{ "name": "Fake", "command": fake_agent("reviewer") }],
        "enabledAgents": ["Fake"],
        "agent": { "name": "Fake" },
        "reviewers": [{ "name": "Fake" }],
    });
    let (status, body) = save(&site, &settings.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    assert_eq!(wait_for_run(&site, &nudges, "R1")["status"], "ended");

    let review = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &review).0,
        StatusCode::OK
    );
    let stage = wait_for_run(&site, &nudges, "R2");
    assert!(
        kinds(&stage).contains(&"message:prompt 1".to_owned()),
        "the thread's kept agent, the same one, did not take the review: {stage}"
    );
}

#[test]
fn the_idle_agent_used_longest_ago_is_the_one_let_go() {
    let repo = TempRepo::new("run-evict-lru");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    for text in ["one", "two", "three"] {
        post_note(&site, "a.txt", 1, 1, text);
    }
    let follow = |of: &str| {
        let body = serde_json::json!({ "of": of, "text": "more" }).to_string();
        let (status, body) = post(&site, &format!("{RUN}/follow"), &body);
        assert_eq!(status, StatusCode::OK, "{body}");
    };
    send(&site, &["N1"]);
    wait_for_run(&site, &nudges, "R1");
    send(&site, &["N2"]);
    wait_for_run(&site, &nudges, "R2");
    // R1's thread is used again after R2's, so R2's agent is the one used longest ago.
    follow("R1");
    wait_for_run(&site, &nudges, "R3");

    send(&site, &["N3"]);
    wait_for_run(&site, &nudges, "R4");

    follow("R3");
    let kept = wait_for_run(&site, &nudges, "R5");
    assert!(
        kinds(&kept).contains(&"message:prompt 3".to_owned()),
        "R1's thread kept its agent: {kept}"
    );
    follow("R2");
    let fresh = wait_for_run(&site, &nudges, "R6");
    assert!(
        kinds(&fresh).contains(&"message:prompt 1".to_owned()),
        "R2's agent was let go: {fresh}"
    );
}

#[test]
fn a_note_in_a_run_still_starting_is_not_sent_again() {
    let repo = TempRepo::new("run-send-twice");
    repo.write("a.txt", "a\n");
    // Its agent never answers `initialize`, so the run stays starting, its note not marked.
    let (site, nudges) = run_site(&repo, "silent");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    assert_eq!(wait_for(&site, &nudges, "starting")["id"], "R1");
    assert_eq!(states(&site), [serde_json::json!({ "kind": "open" })]);

    let (status, body) = send(&site, &["N1"]);

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("N1 is in R1, which is under way"), "{body}");
    assert_eq!(get_json(&site, RUNS)["runs"], serde_json::json!(["R1"]));
    assert_eq!(stop(&site).0, StatusCode::OK);
}

#[test]
fn the_latest_run_deleted_gives_way_to_the_window_s_run_under_way() {
    let repo = TempRepo::new("run-latest-deleted");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "hang");
    set_reviewers(&site, "hang", &[("Plain", "plain")]);
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 1, 1, "two");
    send(&site, &["N1"]);
    assert_eq!(wait_for(&site, &nudges, "running")["id"], "R1");
    let body = serde_json::json!({ "notes": ["N2"], "agent": "Plain" }).to_string();
    assert_eq!(post(&site, RUN, &body).0, StatusCode::OK);
    assert_eq!(wait_for_run(&site, &nudges, "R2")["status"], "ended");

    delete(&site, &format!("{NOTES}?id=N2"));

    // Not R1's log read from disk, unfinished: the run this window holds, under way.
    let latest = get_json(&site, RUN);
    assert_eq!(latest["id"], "R1");
    assert_eq!(latest["status"], "running");
    assert_eq!(stop(&site).0, StatusCode::OK);
}

/// Saves Fake, the fake agent in `author`, as the agent notes go to, running `model`, and
/// Reviewer, the fake agent in scenario "reviewer", as one reviewer per model in `reviewers`.
fn set_models(site: &Site, author: &str, model: Option<&str>, reviewers: &[&str]) {
    let settings = serde_json::json!({
        "customAgents": [
            { "name": "Fake", "command": fake_agent(author) },
            { "name": "Reviewer", "command": fake_agent("reviewer") },
        ],
        "enabledAgents": ["Fake", "Reviewer"],
        "agent": { "name": "Fake", "model": model },
        "reviewers": reviewers
            .iter()
            .map(|model| serde_json::json!({ "name": "Reviewer", "model": model }))
            .collect::<Vec<_>>(),
    });
    let (status, body) = save(site, &settings.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Waits until the usage ledger records `expected`, sorted, as the models `agent` ran, and
/// fails when it does not within [`WAIT`]. A run's record is appended just after the run
/// shows as ended (`run/log.rs`, `Run::record`), so one read could come too early.
fn assert_ledger_models(site: &Site, agent: &str, expected: &[&str]) {
    site.agents.wait();
    let deadline = Instant::now() + WAIT;
    loop {
        let usage = get_json(site, "postil://localhost/api/usage?scope=all&range=all");
        let agents = usage["agents"].as_array().unwrap();
        let listed = agents.iter().find(|each| each["name"] == agent);
        let mut models: Vec<&str> = listed
            .and_then(|listed| listed["activity"]["models"].as_array())
            .into_iter()
            .flatten()
            .map(|each| each["model"].as_str().unwrap())
            .collect();
        models.sort_unstable();
        if models == expected {
            return;
        }
        assert!(Instant::now() < deadline, "{agent} ran {models:?}: {usage}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_slot_s_model_is_set_before_the_prompt_and_a_follow_up_keeps_its_agent_s_model() {
    let repo = TempRepo::new("run-model");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    set_models(&site, "plain", Some("pro"), &[]);
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 1, 1, "two");

    send(&site, &["N1"]);
    let first = wait_for(&site, &nudges, "ended");
    assert_eq!(
        kinds(&first)[1],
        "message:model pro",
        "the model is set before the prompt"
    );

    // The model unchanged, the follow-up goes to the agent kept from R1: its second prompt.
    let follow = |of: &str| {
        let body = serde_json::json!({ "of": of, "text": "more" }).to_string();
        let (status, body) = post(&site, &format!("{RUN}/follow"), &body);
        assert_eq!(status, StatusCode::OK, "{body}");
    };
    follow("R1");
    let kept = wait_for_run(&site, &nudges, "R2");
    assert!(
        kinds(&kept).contains(&"message:prompt 2".to_owned()),
        "{kept}"
    );

    // Another model for the slot: the conversation goes on with the kept agent, on the model
    // it runs, and the slot's new model is for the next fresh agent.
    set_models(&site, "plain", Some("m"), &[]);
    follow("R2");
    let continued = wait_for_run(&site, &nudges, "R3");
    let said = kinds(&continued);
    assert_eq!(said[1], "message:model pro", "{continued}");
    assert!(said.contains(&"message:prompt 3".to_owned()), "{continued}");

    send(&site, &["N2"]);
    let fresh = wait_for_run(&site, &nudges, "R4");
    let said = kinds(&fresh);
    assert_eq!(said[1], "message:model m", "{fresh}");
    assert!(said.contains(&"message:prompt 1".to_owned()), "{fresh}");
    assert_ledger_models(&site, "Fake", &["m", "pro"]);
}

#[test]
fn each_reviewer_runs_its_own_model() {
    let repo = TempRepo::new("run-review-models");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    set_models(&site, "plain", None, &["fast", "pro"]);
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let sent = wait_for(&site, &nudges, "ended");
    assert!(
        kinds(&sent)[1].starts_with("message:got "),
        "no model is set without one in the slot: {sent}"
    );

    let body = serde_json::json!({ "of": "R1" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/review"), &body).0,
        StatusCode::OK
    );
    // The judge's stage last: R2 may already have given way to it when the test first looks.
    let judge = wait_for_run(&site, &nudges, "R3");
    assert_eq!(kinds(&judge)[1], "message:model pro", "{judge}");
    let proposer = get_json(&site, &format!("{RUN}?id=R2"));
    assert_eq!(kinds(&proposer)[1], "message:model fast", "{proposer}");
    assert_ledger_models(&site, "Reviewer", &["fast", "pro"]);
}

#[test]
fn a_model_the_agent_does_not_offer_fails_the_run_before_any_prompt() {
    let repo = TempRepo::new("run-model-gone");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    // A model id an older agent offered, as gemini-2.x ids were before agy.
    set_models(&site, "plain", Some("gemini-2.5-pro"), &[]);
    post_note(&site, "a.txt", 1, 1, "one");

    send(&site, &["N1"]);
    let failed = wait_for(&site, &nudges, "failed");
    let error = entries(&failed)[0]["error"].as_str().unwrap().to_owned();
    assert_eq!(
        error,
        "the agent does not offer the model \"gemini-2.5-pro\"; choose another for it in \
         Settings (offered: m, pro, fast, deep, long)"
    );
    assert!(
        !kinds(&failed)
            .iter()
            .any(|kind| kind.starts_with("message:")),
        "the agent got no prompt: {failed}"
    );
    assert_eq!(states(&site), [serde_json::json!({ "kind": "open" })]);

    // An agent with no model choice cannot run a model at all.
    set_models(&site, "no-config", Some("pro"), &[]);
    send(&site, &["N1"]);
    let failed = wait_for_run(&site, &nudges, "R2");
    let error = entries(&failed)[0]["error"].as_str().unwrap().to_owned();
    assert_eq!(
        error,
        "the agent does not let Postil choose its model; set it to Agent default in Settings \
         (configured: \"pro\")"
    );
    assert_eq!(states(&site), [serde_json::json!({ "kind": "open" })]);
}
