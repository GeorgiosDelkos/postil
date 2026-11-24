//! Notes: `/api/notes` and the routes under it, and the review file they keep.

use super::*;
use crate::notes;

/// The notes' `[id, start, end, anchor]`, the fields that anchoring changes.
fn anchors(review: &serde_json::Value) -> Vec<(String, u64, u64, String)> {
    review["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| {
            (
                note["id"].as_str().unwrap().to_owned(),
                note["lines"]["start"].as_u64().unwrap(),
                note["lines"]["end"].as_u64().unwrap(),
                note["anchor"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn anchor(id: &str, start: u64, end: u64, anchor: &str) -> (String, u64, u64, String) {
    (id.to_owned(), start, end, anchor.to_owned())
}

#[test]
fn notes_are_stored_in_the_git_dir_and_survive_a_restart() {
    let repo = TempRepo::new("notes-store");
    repo.write("src/a.rs", "fn a() {\n    one();\n    two();\n}\n");
    let site = repo.site();

    let review = post_note(&site, "src/a.rs", 2, 3, "  Merge these.\n");
    let note = &review["notes"][0];
    assert_eq!(note["id"], "N1");
    assert_eq!(note["snippet"], "    one();\n    two();");
    assert_eq!(note["body"], "Merge these.", "the body is trimmed");
    assert_eq!(note["anchor"], "found");

    let file = site.repo().unwrap().review_file();
    assert!(
        file.ends_with(".git/postil/review.json"),
        "{}",
        file.display()
    );
    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(stored["version"], 4);
    assert_eq!(stored["notes"], review["notes"]);
    assert_eq!(note["state"], serde_json::json!({ "kind": "open" }));

    // A new process reads the same file.
    let restarted = repo.site();
    assert_eq!(get_json(&restarted, NOTES), review);
    let review = post_note(&restarted, "src/a.rs", 1, 1, "Second.");
    assert_eq!(
        anchors(&review),
        [anchor("N1", 2, 3, "found"), anchor("N2", 1, 1, "found")]
    );
}

#[test]
fn notes_follow_their_text_when_the_file_changes() {
    let repo = TempRepo::new("notes-follow");
    repo.write("src/a.rs", "a\nb\nc\nd\n");
    let site = repo.site();
    post_note(&site, "src/a.rs", 2, 3, "On b and c.");

    repo.write("src/a.rs", "new 1\nnew 2\na\nb\nc\nd\n");
    assert_eq!(
        anchors(&get_json(&site, NOTES)),
        [anchor("N1", 4, 5, "found")]
    );
    // Reading never writes, so a hand edit cannot be overwritten by the page refreshing.
    let stored = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(site.repo().unwrap().review_file()).unwrap())
            .unwrap()
    };
    assert_eq!(anchors(&stored()), [anchor("N1", 2, 3, "found")]);

    // Adding a note saves every note where it is now.
    post_note(&site, "src/a.rs", 1, 1, "On new 1.");
    assert_eq!(
        anchors(&stored()),
        [anchor("N1", 4, 5, "found"), anchor("N2", 1, 1, "found")]
    );

    repo.write("src/a.rs", "new 1\nnew 2\na\nb\nC\nd\n");
    assert_eq!(
        anchors(&get_json(&site, NOTES)),
        [anchor("N1", 4, 5, "lost"), anchor("N2", 1, 1, "found")],
        "the snippet is gone: the note keeps its last saved lines"
    );

    repo.write("src/a.rs", "b\nc\n");
    assert_eq!(
        anchors(&get_json(&site, NOTES)),
        [anchor("N1", 1, 2, "found"), anchor("N2", 1, 1, "lost")]
    );

    fs::remove_file(repo.root.join("src/a.rs")).unwrap();
    assert_eq!(
        anchors(&get_json(&site, NOTES)),
        [anchor("N1", 4, 5, "lost"), anchor("N2", 1, 1, "lost")]
    );
}

#[test]
fn a_review_file_that_cannot_be_read_or_written_is_reported_with_the_next_step() {
    let repo = TempRepo::new("notes-io");
    repo.write("a.txt", "x\n");
    let site = repo.site();
    let file = site.repo().unwrap().review_file();
    let request = &new_note("a.txt", 1, 1, "x", "b");

    // review.json is a directory: it exists but cannot be read.
    fs::create_dir_all(&file).unwrap();
    let (status, reply) = get(&site, NOTES);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.contains("cannot read"), "{reply}");
    assert!(reply.contains(&*file.to_string_lossy()), "{reply}");
    assert!(reply.contains("move it aside"), "{reply}");
    assert!(
        reply.contains("os error"),
        "the cause reaches the page: {reply}"
    );
    fs::remove_dir(&file).unwrap();

    // The postil directory is read-only: there is no review yet, and none can be written.
    let dir = file.parent().unwrap();
    let mut permissions = fs::metadata(dir).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(dir, permissions.clone()).unwrap();
    let (status, reply) = post(&site, NOTES, request);
    #[expect(
        clippy::permissions_set_readonly_false,
        reason = "restores write access so the temporary repository can be removed"
    )]
    permissions.set_readonly(false);
    fs::set_permissions(dir, permissions).unwrap();

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.contains("cannot write"), "{reply}");
    assert!(reply.contains("the change was not saved"), "{reply}");
    assert_eq!(
        fs::read_dir(dir).unwrap().count(),
        0,
        "no review and no temporary file is left behind"
    );
}

#[test]
fn notes_written_into_the_file_by_hand_are_read_and_ids_continue_after_them() {
    let repo = TempRepo::new("notes-by-hand");
    repo.write("a.txt", "x\ny\n");
    let site = repo.site();
    let file = site.repo().unwrap().review_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let by_hand = r#"{"version": 1, "notes": [{"id": "N5", "path": "a.txt",
        "lines": {"start": 2, "end": 2}, "snippet": "y", "body": "From a script."}]}"#;
    fs::write(&file, by_hand).unwrap();

    let unchanged = get_json(&site, NOTES);
    assert_eq!(anchors(&unchanged), [anchor("N5", 2, 2, "found")]);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        by_hand,
        "nothing moved, so the file is not rewritten"
    );

    let review = post_note(&site, "a.txt", 1, 1, "Mine.");
    assert_eq!(
        anchors(&review),
        [anchor("N5", 2, 2, "found"), anchor("N6", 1, 1, "found")]
    );
    // A version 1 file has no states or targets: its notes read as open and on lines, and
    // the write that added N6 made it version 4.
    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(stored["version"], 4);
    assert_eq!(
        stored["notes"][0]["state"],
        serde_json::json!({ "kind": "open" })
    );
    assert_eq!(stored["notes"][0]["target"], "lines");
}

#[test]
fn bad_note_requests_are_refused_and_nothing_is_written() {
    let repo = TempRepo::new("notes-bad");
    repo.write("a.txt", "x\ny\n");
    let site = repo.site();

    let cases = [
        (new_note("a.txt", 0, 1, "x", "b"), "1-based"),
        (new_note("a.txt", 2, 1, "y", "b"), "1-based"),
        (new_note("a.txt", 1, 1, "x", " \n"), "no text"),
        (new_note("a.txt", 1, 2, "x", "b"), "does not have 2 lines"),
        (new_note("../a.txt", 1, 1, "x", "b"), "inside"),
        (
            r#"{"path":"a.txt","lines":{"start":1,"end":1},"snippet":"x"}"#.to_owned(),
            "body",
        ),
        (
            r#"{"path":"a.txt","lines":{"start":1,"end":1},"snippet":"x","body":"b","x":1}"#
                .to_owned(),
            "`x`",
        ),
        ("not json".to_owned(), "unreadable request"),
    ];
    for (request, expected) in cases {
        let (status, reply) = post(&site, NOTES, &request);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{request}: {reply}");
        assert!(reply.contains(expected), "{request}: {reply}");
    }
    assert!(!site.repo().unwrap().review_file().exists());

    // Lines past the end hold none of the shown text: the file changed since it was shown.
    let (status, reply) = post(&site, NOTES, &new_note("a.txt", 2, 3, "y\nz", "b"));
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(reply.contains("select the lines again"), "{reply}");
    assert!(!site.repo().unwrap().review_file().exists());

    assert_eq!(put(&site, NOTES, "").0, StatusCode::METHOD_NOT_ALLOWED);
}

#[test]
fn a_note_s_text_is_edited_in_place_and_a_blank_or_unknown_one_is_refused() {
    let repo = TempRepo::new("notes-edit");
    repo.write("a.txt", "x\ny\n");
    let site = repo.site();
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 2, 2, "two");
    let file = site.repo().unwrap().review_file();
    assert_eq!(
        post(&site, &format!("{NOTES}/resolve?id=N2"), "").0,
        StatusCode::OK
    );

    let (status, body) = patch(
        &site,
        &format!("{NOTES}?id=N2"),
        r#"{"body": "  second  "}"#,
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    let review: serde_json::Value = serde_json::from_str(&body).unwrap();
    let bodies: Vec<&str> = review["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, ["one", "second"], "trimmed, and only N2");
    assert_eq!(
        review["notes"][1]["state"],
        serde_json::json!({ "kind": "resolved" }),
        "an edit changes the text alone"
    );
    assert_eq!(
        anchors(&review)[1],
        anchor("N2", 2, 2, "found"),
        "its lines stay"
    );
    assert!(fs::read_to_string(&file).unwrap().contains("\"second\""));

    let before = fs::read_to_string(&file).unwrap();
    for (uri, body, status, why) in [
        (
            format!("{NOTES}?id=N1"),
            r#"{"body": "   "}"#,
            StatusCode::BAD_REQUEST,
            "no text",
        ),
        (
            format!("{NOTES}?id=N9"),
            r#"{"body": "x"}"#,
            StatusCode::NOT_FOUND,
            "no note N9",
        ),
        (
            format!("{NOTES}?id=N1"),
            r#"{"text": "x"}"#,
            StatusCode::BAD_REQUEST,
            "unreadable",
        ),
        (
            NOTES.to_owned(),
            r#"{"body": "x"}"#,
            StatusCode::BAD_REQUEST,
            "expected ?id=",
        ),
    ] {
        let (got, reply) = patch(&site, &uri, body);
        assert_eq!(got, status, "{uri} {body}: {reply}");
        assert!(reply.contains(why), "{uri} {body}: {reply}");
    }
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        before,
        "nothing refused was written"
    );
}

#[test]
fn a_deleted_note_is_gone_and_ids_above_the_highest_left_are_reused() {
    let repo = TempRepo::new("notes-delete");
    repo.write("a.txt", "x\ny\nz\n");
    let site = repo.site();
    for line in 1..=3 {
        post_note(&site, "a.txt", line, line, "note");
    }

    // Deleting N2 leaves N1 and N3 with their ids.
    let (status, body) = delete(&site, &format!("{NOTES}?id=N2"));
    assert_eq!(status, StatusCode::OK, "{body}");
    let review: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        anchors(&review),
        [anchor("N1", 1, 1, "found"), anchor("N3", 3, 3, "found")]
    );
    let stored = fs::read_to_string(site.repo().unwrap().review_file()).unwrap();
    assert!(!stored.contains("\"N2\""), "{stored}");

    // Once N3 is gone too, the highest left is N1: N2 is handed out again.
    delete(&site, &format!("{NOTES}?id=N3"));
    let review = post_note(&site, "a.txt", 2, 2, "again");
    assert_eq!(
        anchors(&review),
        [anchor("N1", 1, 1, "found"), anchor("N2", 2, 2, "found")]
    );
}

#[test]
fn a_note_is_resolved_with_its_run_and_reopened_to_where_it_was() {
    let repo = TempRepo::new("notes-resolve");
    repo.write("a.txt", "a\nb\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    post_note(&site, "a.txt", 2, 2, "two");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    let change = |action: &str, id: &str| {
        let (status, body) = post(&site, &format!("{NOTES}/{action}?id={id}"), "");
        assert_eq!(status, StatusCode::OK, "{action} {id}: {body}");
        let review: serde_json::Value = serde_json::from_str(&body).unwrap();
        review["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["state"].clone())
            .collect::<Vec<_>>()
    };
    let open = serde_json::json!({ "kind": "open" });
    let resolved_after_r1 = serde_json::json!({ "kind": "resolved", "run": "R1" });
    let resolved = serde_json::json!({ "kind": "resolved" });

    assert_eq!(
        change("resolve", "N1"),
        [resolved_after_r1.clone(), open.clone()]
    );
    assert_eq!(
        change("resolve", "N1"),
        [resolved_after_r1.clone(), open.clone()],
        "resolving twice changes nothing"
    );
    assert_eq!(
        change("resolve", "N2"),
        [resolved_after_r1, resolved.clone()]
    );

    // Only Reopen takes a note out of Resolved: a send is refused, and a run that takes
    // its turn after the engineer resolved the note leaves it resolved, with that run.
    let (status, body) = send(&site, &["N2"]);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("N2 is resolved: reopen it"), "{body}");
    notes::mark_sent(&site.repo().unwrap(), &["N1".to_owned()], "R9").unwrap();
    assert_eq!(
        states(&site),
        [
            serde_json::json!({ "kind": "resolved", "run": "R9" }),
            resolved.clone()
        ]
    );

    let sent = serde_json::json!({ "kind": "sent", "run": "R9" });
    assert_eq!(change("reopen", "N1"), [sent.clone(), resolved.clone()]);
    assert_eq!(
        change("reopen", "N1"),
        [sent.clone(), resolved],
        "reopening a sent note changes nothing"
    );
    assert_eq!(change("reopen", "N2"), [sent.clone(), open.clone()]);
    assert_eq!(
        change("reopen", "N2"),
        [sent, open],
        "reopening an open note changes nothing"
    );
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(site.repo().unwrap().review_file()).unwrap()).unwrap();
    assert_eq!(stored["version"], 4);
}

#[test]
fn changing_a_missing_or_malformed_id_says_so_and_changes_nothing() {
    let repo = TempRepo::new("notes-delete-bad");
    repo.write("a.txt", "x\n");
    let site = repo.site();
    post_note(&site, "a.txt", 1, 1, "one");
    let file = site.repo().unwrap().review_file();
    let before = fs::read_to_string(&file).unwrap();

    let (status, body) = delete(&site, &format!("{NOTES}?id=N9"));
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("there is no note N9"), "{body}");
    let (status, body) = delete(&site, &format!("{NOTES}?id=nine"));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("note id \"nine\""), "{body}");
    let (status, _) = delete(&site, NOTES);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for (uri, status) in [
        (format!("{NOTES}/resolve?id=N9"), StatusCode::NOT_FOUND),
        (format!("{NOTES}/reopen?id=nine"), StatusCode::BAD_REQUEST),
        (format!("{NOTES}/resolve"), StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(post(&site, &uri, "").0, status, "{uri}");
    }
    for action in ["resolve", "reopen"] {
        assert_eq!(
            get(&site, &format!("{NOTES}/{action}?id=N1")).0,
            StatusCode::METHOD_NOT_ALLOWED,
            "only a POST changes a note: {action}"
        );
    }

    assert_eq!(fs::read_to_string(&file).unwrap(), before);
}

#[test]
fn a_broken_review_file_is_reported_and_never_overwritten() {
    let repo = TempRepo::new("notes-broken");
    repo.write("a.txt", "x\n");
    let site = repo.site();
    let file = site.repo().unwrap().review_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let note = r#"{"id":"N1","path":"a.txt","lines":{"start":1,"end":1},"snippet":"x",
        "body":"b","anchor":"found"}"#;

    let cases = [
        ("{\"version\": 1, \"notes\": [", "cannot parse".to_owned()),
        (
            r#"{"version": 5, "notes": []}"#,
            "version 5, but this build of Postil reads versions 1 to 4".to_owned(),
        ),
        (r#"{"version": 0, "notes": []}"#, "version 0".to_owned()),
        (r#"{"notes": []}"#, "cannot parse".to_owned()),
        (
            &*format!(r#"{{"version": 1, "notes": [{note}, {note}]}}"#),
            "N1 is used twice".to_owned(),
        ),
        (
            &*format!(
                r#"{{"version": 1, "notes": [{}]}}"#,
                note.replace(r#""body":"b""#, r#""body":"b","stat":"open""#)
            ),
            "unknown field `stat`".to_owned(),
        ),
        (
            &*format!(
                r#"{{"version": 2, "notes": [{}]}}"#,
                note.replace(
                    r#""body":"b""#,
                    r#""body":"b","state":{"kind":"open","run":"R1"}"#
                )
            ),
            "unknown field `run`".to_owned(),
        ),
        // A target whose fields do not fit it: a hand edit gone wrong, refused, not guessed.
        (
            &*format!(
                r#"{{"version": 4, "notes": [{}]}}"#,
                note.replace(r#""body":"b""#, r#""body":"b","target":"folder""#)
            ),
            "N1: a note on a file or folder has no `lines` or `snippet`".to_owned(),
        ),
        (
            r#"{"version": 4, "notes": [{"id":"N1","path":"a.txt","target":"lines","body":"b"}]}"#,
            "N1: a note on lines needs `lines` and `snippet`".to_owned(),
        ),
        (
            &*format!(
                r#"{{"version": 4, "notes": [{}]}}"#,
                note.replace(r#""body":"b""#, r#""body":"b","target":"dir""#)
            ),
            "unknown variant `dir`".to_owned(),
        ),
    ];
    for (contents, expected) in cases {
        fs::write(&file, contents).unwrap();

        let (status, reply) = get(&site, NOTES);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{contents}");
        assert!(reply.contains(&expected), "{contents}: {reply}");
        assert!(reply.contains(&*file.to_string_lossy()), "{reply}");
        assert!(reply.contains("move it aside"), "{reply}");

        let request = &new_note("a.txt", 1, 1, "x", "b");
        let (status, _) = post(&site, NOTES, request);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{contents}");
        assert_eq!(fs::read_to_string(&file).unwrap(), contents);
    }
}

#[test]
fn a_review_with_the_highest_id_refuses_another_note_and_is_left_alone() {
    let repo = TempRepo::new("notes-ids");
    repo.write("a.txt", "x\n");
    let site = repo.site();
    let file = site.repo().unwrap().review_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let full = r#"{"version": 1, "notes": [{"id": "N4294967295", "path": "a.txt",
        "lines": {"start": 1, "end": 1}, "snippet": "x", "body": "b"}]}"#;
    fs::write(&file, full).unwrap();

    let (status, reply) = post(&site, NOTES, &new_note("a.txt", 1, 1, "x", "c"));
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{reply}");
    assert!(
        reply.contains("no note id is left after N4294967295"),
        "{reply}"
    );
    assert!(reply.contains("move it aside"), "{reply}");
    assert_eq!(fs::read_to_string(&file).unwrap(), full);
}

#[test]
fn notes_on_several_files_are_each_placed_in_their_own_file() {
    let repo = TempRepo::new("notes-files");
    // Each file's lines are in the other: a note placed in the wrong file is found there, at
    // another line.
    repo.write("src/a.rs", "x\ny\n");
    repo.write("src/b.rs", "y\nx\n");
    let site = repo.site();
    post_note(&site, "src/a.rs", 1, 1, "On x.");
    post_note(&site, "src/b.rs", 1, 1, "On y.");
    post_note(&site, "src/a.rs", 2, 2, "On y.");

    repo.write("src/a.rs", "new\nx\ny\n");
    assert_eq!(
        anchors(&get_json(&site, NOTES)),
        [
            anchor("N1", 2, 2, "found"),
            anchor("N2", 1, 1, "found"),
            anchor("N3", 3, 3, "found"),
        ]
    );

    // Adding a note reads its own file first: the other file's notes are placed in theirs.
    repo.write("src/a.rs", "new\nnew\nx\ny\n");
    assert_eq!(
        anchors(&post_note(&site, "src/b.rs", 2, 2, "On x.")),
        [
            anchor("N1", 3, 3, "found"),
            anchor("N2", 1, 1, "found"),
            anchor("N3", 4, 4, "found"),
            anchor("N4", 2, 2, "found"),
        ]
    );
}

#[test]
fn a_note_goes_where_its_shown_text_is_when_the_file_changed_meanwhile() {
    let repo = TempRepo::new("notes-shown");
    repo.write("a.txt", "a\nb\nc\n");
    let site = repo.site();

    // The page showed `b` on line 2; two lines were added above before the save.
    repo.write("a.txt", "new\nnew\na\nb\nc\n");
    let (status, reply) = post(&site, NOTES, &new_note("a.txt", 2, 2, "b", "On b."));
    assert_eq!(status, StatusCode::OK, "{reply}");
    let review: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(anchors(&review), [anchor("N1", 4, 4, "found")]);
    assert_eq!(review["notes"][0]["snippet"], "b");

    // Two copies, neither where it was shown: the note is refused rather than guessed.
    repo.write("a.txt", "b\nx\nx\nb\n");
    let (status, reply) = post(&site, NOTES, &new_note("a.txt", 2, 2, "b", "On b."));
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
}

#[test]
fn notes_added_at_once_from_two_windows_are_all_kept() {
    let repo = TempRepo::new("notes-race");
    repo.write("a.txt", "x\n");

    // Each thread stands for one app window: its own `Repo`, the same working tree.
    let windows: Vec<_> = (0..2)
        .map(|window| {
            let root = repo.root.clone();
            std::thread::spawn(move || {
                let repo = Repo::open(&root).unwrap();
                for index in 0..10 {
                    let body = format!("window {window} note {index}");
                    let new = serde_json::from_str(&new_note("a.txt", 1, 1, "x", &body));
                    notes::add(&repo, new.unwrap()).unwrap();
                }
            })
        })
        .collect();
    for window in windows {
        window.join().unwrap();
    }

    let review = get_json(&repo.site(), NOTES);
    let mut ids: Vec<u32> = review["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["id"].as_str().unwrap()[1..].parse().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, (1..=20).collect::<Vec<u32>>());
}

#[test]
fn a_change_gives_up_on_a_lock_another_process_keeps_and_names_it() {
    let repo = TempRepo::new("notes-stuck-lock");
    repo.write("a.txt", "x\n");
    let lock_file = repo.root.join(".git/postil/review.lock");
    fs::create_dir_all(lock_file.parent().unwrap()).unwrap();
    // A lock on a file of its own, as another process's would be: `flock` locks belong to
    // the open file, so this one conflicts with the request's in the same process.
    let held = fs::File::create(&lock_file).unwrap();
    held.lock().unwrap();

    // On its own thread, so a request that waits forever fails this test instead of
    // hanging it.
    let (sender, replies) = mpsc::channel();
    let root = repo.root.clone();
    std::thread::spawn(move || {
        let site = Site::new(None, None, no_agents(), quiet());
        site.open(Repo::open(&root).unwrap(), LiveRun::Stop)
            .unwrap();
        drop(sender.send(post(&site, NOTES, &new_note("a.txt", 1, 1, "x", "Hi."))));
    });
    let (status, reply) = replies
        .recv_timeout(Duration::from_secs(60))
        .expect("a reply within 60 s: the request thread panicked, or it waited on the lock");
    drop(held);

    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    // git names the git directory with links resolved (`/tmp` is `/private/tmp` on macOS).
    let expected = format!(
        "another writer held {} for 2 s, so the change was not saved",
        lock_file.canonicalize().unwrap().display()
    );
    assert!(reply.starts_with(&expected), "{reply}");
    assert!(!repo.root.join(".git/postil/review.json").exists());
}

/// The notes' `[id, path, target, anchor]`, and whether each has lines or a snippet.
fn targets(review: &serde_json::Value) -> Vec<(String, String, String, String, bool)> {
    review["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| {
            let field = |name: &str| note[name].as_str().unwrap().to_owned();
            let placed = note.get("lines").is_some() || note.get("snippet").is_some();
            (
                field("id"),
                field("path"),
                field("target"),
                field("anchor"),
                placed,
            )
        })
        .collect()
}

fn target(
    id: &str,
    path: &str,
    kind: &str,
    anchor: &str,
) -> (String, String, String, String, bool) {
    let placed = kind == "lines";
    (
        id.to_owned(),
        path.to_owned(),
        kind.to_owned(),
        anchor.to_owned(),
        placed,
    )
}

#[test]
fn notes_on_a_whole_file_or_folder_are_stored_beside_line_notes() {
    let repo = TempRepo::new("notes-paths");
    repo.write("src/a.rs", "fn a() {}\n");
    repo.write("README.md", "# Readme\n");
    let site = repo.site();

    post_note(&site, "src/a.rs", 1, 1, "On a line.");
    post_path_note(&site, "src", "folder", "  Split this module.\n");
    let review = post_path_note(&site, "README.md", "file", "Document the flag.");
    assert_eq!(
        targets(&review),
        [
            target("N1", "src/a.rs", "lines", "found"),
            target("N2", "src", "folder", "found"),
            target("N3", "README.md", "file", "found"),
        ]
    );
    assert_eq!(review["notes"][1]["body"], "Split this module.", "trimmed");
    assert_eq!(
        review["notes"][1]["state"],
        serde_json::json!({ "kind": "open" })
    );

    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(site.repo().unwrap().review_file()).unwrap()).unwrap();
    assert_eq!(stored["notes"], review["notes"]);
    assert_eq!(
        get_json(&repo.site(), NOTES),
        review,
        "a new process reads them the same"
    );
}

#[test]
fn a_file_or_folder_note_is_lost_while_its_path_is_gone_or_of_another_kind() {
    let repo = TempRepo::new("notes-path-anchor");
    fs::create_dir_all(repo.root.join("src/acp")).unwrap();
    repo.write("src/acp/wire.rs", "wire\n");
    repo.write("b.rs", "b\n");
    let site = repo.site();
    post_path_note(&site, "src/acp", "folder", "Split by message.");
    post_path_note(&site, "b.rs", "file", "Rename.");

    fs::rename(repo.root.join("src/acp"), repo.root.join("src/protocol")).unwrap();
    fs::remove_file(repo.root.join("b.rs")).unwrap();
    // A folder where the file was is not the file the note is on.
    fs::create_dir(repo.root.join("b.rs")).unwrap();
    assert_eq!(
        targets(&get_json(&site, NOTES)),
        [
            target("N1", "src/acp", "folder", "lost"),
            target("N2", "b.rs", "file", "lost"),
        ]
    );

    fs::rename(repo.root.join("src/protocol"), repo.root.join("src/acp")).unwrap();
    fs::remove_dir(repo.root.join("b.rs")).unwrap();
    repo.write("b.rs", "back\n");
    assert_eq!(
        targets(&get_json(&site, NOTES)),
        [
            target("N1", "src/acp", "folder", "found"),
            target("N2", "b.rs", "file", "found"),
        ]
    );
}

#[test]
fn a_file_or_folder_note_on_a_path_of_another_kind_or_none_is_refused() {
    let repo = TempRepo::new("notes-path-bad");
    repo.write("src/a.rs", "a\n");
    // Neither a file nor a folder.
    make_fifo(&repo.root.join("pipe"));
    let site = repo.site();

    let cases = [
        (path_note("pipe", "folder", "b"), "pipe is not a folder"),
        (path_note("pipe", "file", "b"), "pipe is not a file"),
        (
            path_note("src/a.rs", "folder", "b"),
            "src/a.rs is not a folder",
        ),
        (path_note("src", "file", "b"), "src is not a file"),
        (path_note("gone", "folder", "b"), "gone is not a folder"),
        (path_note("src", "folder", "  "), "no text"),
        // One spelling per path, as git writes it: the prompt shows the stored path.
        (path_note("src/", "folder", "b"), "inside the repository"),
        (path_note("./src", "folder", "b"), "inside the repository"),
        (path_note("src//a.rs", "file", "b"), "inside the repository"),
        (path_note("..", "folder", "b"), "inside the repository"),
        (path_note("", "folder", "b"), "inside the repository"),
        (path_note("src", "dir", "b"), "unknown variant `dir`"),
        (
            serde_json::json!({
                "path": "src",
                "target": "folder",
                "lines": { "start": 1, "end": 1 },
                "snippet": "a",
                "body": "b",
            })
            .to_string(),
            "has no `lines` or `snippet`",
        ),
        (
            r#"{"path":"src/a.rs","target":"lines","body":"b"}"#.to_owned(),
            "needs `lines` and `snippet`",
        ),
    ];
    for (request, expected) in cases {
        let (status, reply) = post(&site, NOTES, &request);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{request}: {reply}");
        assert!(reply.contains(expected), "{request}: {reply}");
    }
    assert!(!site.repo().unwrap().review_file().exists());
}
