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
