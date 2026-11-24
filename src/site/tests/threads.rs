//! Threads (`thread.rs`) through `/api/thread*` and the runs that make them: the worktree a
//! send makes, Apply and Discard, the cloned build folders, and a thread that goes with its
//! last note.

use super::*;

const THREAD: &str = "postil://localhost/api/thread";

/// The thread whose first run is `id`, as `/api/thread` gives it.
fn thread(site: &Site, id: &str) -> serde_json::Value {
    get_json(site, &format!("{THREAD}?id={id}"))
}

/// Ends thread `id` by `ending` (`apply` or `discard`), as the page does.
fn end(site: &Site, ending: &str, id: &str) -> (StatusCode, String) {
    let body = serde_json::json!({ "thread": id }).to_string();
    post(site, &format!("{THREAD}/{ending}"), &body)
}

/// A repository with `a.txt`, a site whose agent writes `a.txt` and `new.txt`, and one run of
/// a note on `a.txt`, ended. Returns its worktree.
fn written(name: &str) -> (TempRepo, Site, mpsc::Receiver<()>, PathBuf) {
    let repo = TempRepo::new(name);
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "writes");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");
    let path = PathBuf::from(thread(&site, "R1")["path"].as_str().unwrap());
    (repo, site, nudges, path)
}

#[test]
fn apply_writes_the_thread_s_changes_uncommitted_and_frees_its_worktree() {
    let (repo, site, _nudges, worktree) = written("thread-apply");
    let branch = thread(&site, "R1")["branch"].as_str().unwrap().to_owned();

    let (status, body) = end(&site, "apply", "R1");
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(
        fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        "a\nprompt 1\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("new.txt")).unwrap(),
        "new\n"
    );
    // Uncommitted and unstaged: the engineer's index is left alone.
    assert_eq!(
        git_out(&repo.root, &["status", "--porcelain"]),
        "M a.txt\n?? new.txt"
    );
    assert_eq!(git_out(&repo.root, &["branch", "--show-current"]), "trunk");
    // The worktree is gone, from disk and from git; the branch stays with the notes.
    assert!(!worktree.exists(), "{worktree:?}");
    assert!(
        repo.home().join("trash").is_dir(),
        "moved into the trash beside it"
    );
    assert!(
        !git_out(&repo.root, &["worktree", "list"]).contains("r1"),
        "git forgot the worktree"
    );
    assert_eq!(
        git_out(&repo.root, &["branch", "--list", &branch]),
        branch.as_str()
    );
    assert_eq!(thread(&site, "R1")["state"], "applied");

    // An ended thread takes no more work.
    let (status, body) = end(&site, "discard", "R1");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("already ended"), "{body}");
    let follow = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/follow"), &follow);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("was applied"), "{body}");
}

#[test]
fn an_apply_that_does_not_fit_changes_nothing_and_discard_ends_the_thread() {
    let (repo, site, _nudges, worktree) = written("thread-conflict");
    // The engineer changed the same line meanwhile.
    repo.write("a.txt", "mine\n");

    let (status, body) = end(&site, "apply", "R1");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("do not fit your working tree") && body.contains("a.txt"),
        "{body}"
    );
    // All or nothing: the new file, which would fit, was not written either.
    assert_eq!(
        fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        "mine\n"
    );
    assert!(!repo.root.join("new.txt").exists());
    assert_eq!(thread(&site, "R1")["state"], "open");
    assert!(worktree.is_dir());

    let (status, body) = end(&site, "discard", "R1");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        "mine\n"
    );
    assert!(!repo.root.join("new.txt").exists());
    assert!(!worktree.exists());
    assert_eq!(thread(&site, "R1")["state"], "discarded");
}

#[test]
fn a_worktree_gets_the_ignored_build_folders_cloned_but_not_a_virtual_environment() {
    let repo = TempRepo::new("thread-clones");
    repo.write(".gitignore", "target/\n.venv/\nbuild.log\n");
    repo.write("a.txt", "a\n");
    fs::create_dir_all(repo.root.join("target/debug")).unwrap();
    repo.write("target/debug/app", "built\n");
    fs::create_dir_all(repo.root.join(".venv")).unwrap();
    repo.write(".venv/pyvenv.cfg", "home = /usr/bin\n");
    repo.write("build.log", "an ignored file\n");
    let (site, nudges) = run_site(&repo, "plain");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");

    let made = thread(&site, "R1");
    let worktree = PathBuf::from(made["path"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("target/debug/app")).unwrap(),
        "built\n"
    );
    assert!(!worktree.join(".venv").exists());
    assert!(!worktree.join("build.log").exists(), "files are not cloned");
    assert_eq!(
        made["skipped"],
        serde_json::json!([".venv: a Python virtual environment"])
    );
    // A clone is the worktree's own: a build there leaves the engineer's as it was.
    fs::write(worktree.join("target/debug/app"), "rebuilt\n").unwrap();
    assert_eq!(
        fs::read_to_string(repo.root.join("target/debug/app")).unwrap(),
        "built\n"
    );
    // Ignored, so neither committed nor in the way of ending the thread.
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
}

#[test]
fn a_send_needs_a_commit_to_start_its_thread_from() {
    let repo = TempRepo::new("thread-no-commit");
    repo.write("a.txt", "a\n");
    let (site, _nudges) = run_site(&repo, "plain");
    // A new branch with no commit yet: `HEAD` names nothing.
    repo.git(&["checkout", "--quiet", "--orphan", "fresh"]);
    post_note(&site, "a.txt", 1, 1, "one");

    let (status, body) = send(&site, &["N1"]);

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("no commit yet"), "{body}");
    assert_eq!(get_json(&site, RUNS)["runs"], serde_json::json!([]));
}

#[test]
fn a_thread_goes_with_its_last_note_worktree_branch_and_record() {
    let (repo, site, _nudges, worktree) = written("thread-forget");
    let branch = thread(&site, "R1")["branch"].as_str().unwrap().to_owned();

    let (status, body) = delete(&site, "postil://localhost/api/notes?id=N1");
    assert_eq!(status, StatusCode::OK, "{body}");

    assert!(!worktree.exists());
    assert_eq!(git_out(&repo.root, &["branch", "--list", &branch]), "");
    assert_eq!(
        get(&site, &format!("{THREAD}?id=R1")),
        (StatusCode::OK, "null".to_owned())
    );
}

#[test]
fn a_record_pointing_outside_the_worktrees_never_gets_that_folder_deleted() {
    let (repo, site, _nudges, worktree) = written("thread-guard");
    // A hand edit points the thread at a folder of the engineer's.
    let decoy = repo.root.join("src");
    repo.write("src/keep.rs", "fn keep() {}\n");
    let record = repo.root.join(".git/postil/threads.json");
    let text = fs::read_to_string(&record).unwrap();
    let real = worktree.to_str().unwrap();
    fs::write(&record, text.replace(real, decoy.to_str().unwrap())).unwrap();

    let (status, body) = delete(&site, "postil://localhost/api/notes?id=N1");

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("is not a worktree of this repository"),
        "{body}"
    );
    assert!(decoy.join("keep.rs").exists(), "the folder is left alone");
    assert_eq!(
        thread(&site, "R1")["path"],
        decoy.to_str().unwrap(),
        "the thread stays in the record, to be fixed"
    );
}

#[test]
fn another_window_cannot_end_or_forget_a_thread_whose_run_goes_on() {
    let repo = TempRepo::new("thread-elsewhere");
    repo.write("src/a.rs", "one\ntwo\n");
    let (running, nudges) = run_site(&repo, "flood");
    post_note(&running, "src/a.rs", 1, 1, "one");
    send(&running, &["N1"]);
    wait_for(&running, &nudges, "running");
    let worktree = PathBuf::from(thread(&running, "R1")["path"].as_str().unwrap());
    // A second window on the same repository: its own slot holds nothing.
    let (other, _quiet) = run_site(&repo, "plain");

    // The window running it says so in its own words.
    let (status, body) = end(&running, "discard", "R1");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("still working on the last prompt"), "{body}");
    let (status, body) = end(&other, "discard", "R1");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("another Postil window"), "{body}");
    // Deleting the thread's note there leaves the worktree to the run: it is forgotten at a
    // later delete, once no run holds the lock.
    let (status, body) = delete(&other, "postil://localhost/api/notes?id=N1");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(worktree.is_dir());
    assert_eq!(thread(&other, "R1")["state"], "open");

    assert_eq!(
        post(&running, &format!("{RUN}/cancel"), "").0,
        StatusCode::OK
    );
    wait_for(&running, &nudges, "ended");
}

#[test]
fn a_branch_moved_off_postil_s_commits_is_refused_and_one_moved_past_them_is_taken() {
    let (repo, site, _nudges, worktree) = written("thread-moved");
    let record = thread(&site, "R1");
    let fork = record["fork"].as_str().unwrap();
    let tip = record["tip"].as_str().unwrap().to_owned();

    // History rewritten in the worktree: Postil's commit is gone from the branch.
    git_out(&worktree, &["reset", "--quiet", "--hard", fork]);
    fs::write(worktree.join("a.txt"), "elsewhere\n").unwrap();
    git_out(
        &worktree,
        &["commit", "--quiet", "--all", "--message", "elsewhere"],
    );
    let (status, body) = end(&site, "apply", "R1");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("does not follow"), "{body}");
    assert_eq!(fs::read_to_string(repo.root.join("a.txt")).unwrap(), "a\n");

    // Back on Postil's commit, and one more of the engineer's after it: Apply takes both.
    git_out(&worktree, &["reset", "--quiet", "--hard", &tip]);
    fs::write(worktree.join("more.txt"), "more\n").unwrap();
    git_out(&worktree, &["add", "more.txt"]);
    git_out(&worktree, &["commit", "--quiet", "--message", "more"]);
    let (status, body) = end(&site, "apply", "R1");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fs::read_to_string(repo.root.join("more.txt")).unwrap(),
        "more\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        "a\nprompt 1\n"
    );
}

#[test]
fn work_left_in_the_worktree_is_committed_before_a_run_and_moves_the_tip_even_if_it_fails() {
    let (repo, site, nudges, worktree) = written("thread-before");
    fs::write(worktree.join("mine.txt"), "by hand\n").unwrap();
    // The follow-up's agent never starts, so its run takes no snapshot after it.
    set_agents(&site, &[("Fake", &fake_agent("exit-on-start"))], "Fake");
    let body = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
    assert_eq!(
        post(&site, &format!("{RUN}/follow"), &body).0,
        StatusCode::OK
    );
    wait_for_run(&site, &nudges, "R2");

    let record = thread(&site, "R1");
    assert_eq!(record["tip"], git_out(&worktree, &["rev-parse", "HEAD"]));
    assert_eq!(
        git_out(&worktree, &["log", "-1", "--format=%s"]),
        "Postil: work left in the worktree before R2"
    );
    let (status, body) = end(&site, "apply", "R1");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fs::read_to_string(repo.root.join("mine.txt")).unwrap(),
        "by hand\n"
    );
}

#[test]
fn a_folder_ignored_only_by_an_uncommitted_rule_is_not_cloned_or_committed() {
    let repo = TempRepo::new("thread-uncommitted-ignore");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "writes");
    // Ignored in the engineer's tree, by a rule the commit the thread starts from lacks.
    repo.write(".gitignore", "target/\n");
    fs::create_dir_all(repo.root.join("target")).unwrap();
    repo.write("target/app", "built\n");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");

    let record = thread(&site, "R1");
    let worktree = PathBuf::from(record["path"].as_str().unwrap());
    assert!(!worktree.join("target").exists());
    assert_eq!(
        record["skipped"],
        serde_json::json!(["target: not ignored by the committed .gitignore"])
    );
    let committed = git_out(&worktree, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!committed.contains("target/"), "{committed}");
}

#[test]
fn apply_carries_edits_deletions_renames_and_binary_files() {
    let repo = TempRepo::new("thread-reshape");
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    repo.write("c.txt", "c\n");
    let (site, nudges) = run_site(&repo, "reshapes");
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "ended");

    let (status, body) = end(&site, "apply", "R1");

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        "a\nedited\n"
    );
    assert!(!repo.root.join("b.txt").exists());
    assert!(!repo.root.join("c.txt").exists());
    assert_eq!(fs::read_to_string(repo.root.join("d.txt")).unwrap(), "c\n");
    assert_eq!(
        fs::read(repo.root.join("bin.dat")).unwrap(),
        [0, 1, 2, 255, 0, 7]
    );
}

#[test]
fn deleting_the_last_note_of_an_ended_thread_deletes_its_branch_and_record() {
    let (repo, site, _nudges, _worktree) = written("thread-forget-ended");
    let branch = thread(&site, "R1")["branch"].as_str().unwrap().to_owned();
    assert_eq!(end(&site, "apply", "R1").0, StatusCode::OK);

    let (status, body) = delete(&site, "postil://localhost/api/notes?id=N1");

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(git_out(&repo.root, &["branch", "--list", &branch]), "");
    assert_eq!(
        get(&site, &format!("{THREAD}?id=R1")),
        (StatusCode::OK, "null".to_owned())
    );
}

#[test]
fn a_record_naming_a_branch_outside_postil_s_never_gets_it_deleted() {
    let (repo, site, _nudges, _worktree) = written("thread-branch-guard");
    let record = repo.root.join(".git/postil/threads.json");
    let branch = thread(&site, "R1")["branch"].as_str().unwrap().to_owned();
    let text = fs::read_to_string(&record).unwrap();
    fs::write(&record, text.replace(&format!("\"{branch}\""), "\"trunk\"")).unwrap();

    let (status, body) = delete(&site, "postil://localhost/api/notes?id=N1");

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        git_out(&repo.root, &["branch", "--list", "trunk"]),
        "* trunk"
    );
}

#[test]
fn an_unreadable_thread_record_fails_the_send_and_is_never_written_over() {
    let repo = TempRepo::new("thread-bad-record");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    let record = repo.root.join(".git/postil/threads.json");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    fs::write(&record, "{ not json").unwrap();
    post_note(&site, "a.txt", 1, 1, "one");

    let (status, body) = send(&site, &["N1"]);

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("cannot read the threads"), "{body}");
    assert_eq!(fs::read_to_string(&record).unwrap(), "{ not json");
    let failed = wait_for(&site, &nudges, "failed");
    assert_eq!(failed["id"], "R1");

    // A record that cannot be read at all is not an empty one.
    fs::remove_file(&record).unwrap();
    fs::create_dir(&record).unwrap();
    let (status, body) = send(&site, &["N1"]);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("cannot read the threads"), "{body}");
}

#[test]
fn the_engineer_s_hooks_run_neither_for_the_checkout_nor_for_postil_s_commits() {
    let repo = TempRepo::new("thread-hooks");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "writes");
    let hooks = repo.root.join(".git/hooks");
    let marker = repo.home().with_extension("checked-out");
    for (hook, script) in [
        ("prepare-commit-msg", "#!/bin/sh\nexit 1\n".to_owned()),
        (
            "post-checkout",
            format!("#!/bin/sh\ntouch {}\n", marker.display()),
        ),
    ] {
        fs::write(hooks.join(hook), script).unwrap();
        fs::set_permissions(hooks.join(hook), fs::Permissions::from_mode(0o755)).unwrap();
    }
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);
    let ended = wait_for(&site, &nudges, "ended");

    assert_eq!(ended["problem"], serde_json::Value::Null, "{ended}");
    assert_eq!(
        thread(&site, "R1")["tip"],
        git_out(&repo.root, &["rev-parse", "postil/r1-one"])
    );
    assert!(!marker.exists(), "post-checkout ran");
    drop(fs::remove_file(marker));
}

#[test]
fn a_worktree_deleted_by_hand_takes_no_more_work_and_still_goes_with_its_note() {
    let (repo, site, nudges, worktree) = written("thread-gone");
    let branch = thread(&site, "R1")["branch"].as_str().unwrap().to_owned();
    // The follow-up's commit fails (git refuses an empty name): R2 has no snapshot after it,
    // so its changes are read from the worktree's files as they are.
    repo.git(&["config", "user.name", ""]);
    let follow = |of: &str| {
        let body = serde_json::json!({ "of": of, "text": "more" }).to_string();
        post(&site, &format!("{RUN}/follow"), &body)
    };
    assert_eq!(follow("R1").0, StatusCode::OK);
    wait_for_run(&site, &nudges, "R2");
    let changes = get_json(&site, &format!("{RUN}/changes?from=R2"));
    assert!(
        changes["patch"].as_str().unwrap().contains("+prompt 2"),
        "{changes}"
    );
    repo.git(&["config", "user.name", "Postil Test"]);

    fs::remove_dir_all(&worktree).unwrap();

    let (status, body) = follow("R2");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("is gone"), "{body}");
    let (status, body) = get(&site, &format!("{RUN}/changes?from=R2"));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("is gone"), "{body}");
    // Git still records the worktree, which keeps its branch checked out: deleting the note
    // removes that record, then the branch.
    let (status, body) = delete(&site, "postil://localhost/api/notes?id=N1");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(git_out(&repo.root, &["branch", "--list", &branch]), "");
    assert_eq!(
        git_out(&repo.root, &["worktree", "list"]).lines().count(),
        1
    );
}

#[test]
fn a_branch_deleted_by_hand_does_not_keep_its_thread_from_going_with_its_note() {
    let (repo, site, _nudges, _worktree) = written("thread-branch-gone");
    let branch = thread(&site, "R1")["branch"].as_str().unwrap().to_owned();
    assert_eq!(end(&site, "discard", "R1").0, StatusCode::OK);
    repo.git(&["branch", "--quiet", "-D", &branch]);

    let (status, body) = delete(&site, "postil://localhost/api/notes?id=N1");

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        get(&site, &format!("{THREAD}?id=R1")),
        (StatusCode::OK, "null".to_owned())
    );
}

#[test]
fn a_worktree_that_cannot_be_made_fails_its_run_and_leaves_no_thread() {
    let repo = TempRepo::new("thread-no-worktree");
    repo.write("a.txt", "a\n");
    let (site, nudges) = run_site(&repo, "plain");
    // The branch the thread would make, there already: `git worktree add -b` refuses.
    repo.git(&["branch", "postil/r1-one"]);
    post_note(&site, "a.txt", 1, 1, "one");
    send(&site, &["N1"]);

    let failed = wait_for(&site, &nudges, "failed");

    let error = failed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "failed")
        .unwrap()["error"]
        .clone();
    assert!(
        error.as_str().unwrap().contains("already exists"),
        "{failed}"
    );
    assert_eq!(
        get(&site, &format!("{THREAD}?id=R1")),
        (StatusCode::OK, "null".to_owned()),
        "a thread that claims the branch would delete it with its note"
    );
    assert_eq!(
        git_out(&repo.root, &["branch", "--list", "postil/r1-one"]),
        "postil/r1-one"
    );
}

#[test]
fn a_patch_or_record_that_is_not_what_git_wrote_is_refused() {
    let repo = TempRepo::new("thread-repo-guards");
    repo.write("a.txt", "a\n");
    repo.commit();
    let tree = Repo::open(&repo.root).unwrap();
    // An id from the record that git would read as an option, which writes a file.
    let out = repo.root.join("out.txt");
    let option = format!("--output={}", out.display());
    let error = tree.patch(&option, "HEAD").unwrap_err();
    assert!(matches!(error, Error::BadRequest(_)), "{error:?}");
    assert!(!out.exists());
    // A folder whose `.git` is a directory is a repository of its own, not a worktree.
    let own = repo.home().join("own");
    fs::create_dir_all(own.join(".git")).unwrap();
    let error = tree.worktree_record(&own).unwrap_err();
    assert!(matches!(error, Error::NotAWorktree { .. }), "{error:?}");
}
