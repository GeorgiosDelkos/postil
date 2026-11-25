//! Keeping the disk in bounds (`run/room.rs`): the cap on worktrees and the free-space floor,
//! met by evicting idle threads, an evicted thread made again by its next run, and the sweep
//! of worktrees no thread names when a project opens.

use super::threads::{end, thread, written};
use super::*;
use crate::run::{FLOOR, TestDisk};

/// The worktree of thread `id`.
fn worktree(site: &Site, id: &str) -> PathBuf {
    PathBuf::from(thread(site, id)["path"].as_str().unwrap())
}

/// Sends the follow-up "more" to run `of`, as the page does.
fn follow(site: &Site, of: &str) -> (StatusCode, String) {
    let body = serde_json::json!({ "of": of, "text": "more" }).to_string();
    post(site, &format!("{RUN}/follow"), &body)
}

/// Adds note `body` on `a.txt`, sends it as `note`, and waits for its run `id` to end well.
fn send_ended(site: &Site, nudges: &mpsc::Receiver<()>, note: &str, id: &str) {
    post_note(site, "a.txt", 1, 1, note);
    let ids = get_json(site, NOTES)["notes"].as_array().unwrap().len();
    let (status, body) = send(site, &[&format!("N{ids}")]);
    assert_eq!(status, StatusCode::OK, "{body}");
    let run = wait_for_run(site, nudges, id);
    assert_eq!(run["status"], "ended", "{run}");
}

/// The error a failed run recorded.
fn failure(run: &serde_json::Value) -> String {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "failed")
        .unwrap_or_else(|| panic!("no failure in {run}"))["error"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn over_the_cap_an_idle_thread_is_evicted_and_its_follow_up_makes_it_again() {
    let (repo, site, nudges, first) = written("room-cap");
    site.runner().unwrap().set_test_disk(TestDisk {
        max_worktrees: Some(1),
        ..TestDisk::default()
    });
    let porcelain = git_out(&repo.root, &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains("locked Postil's worktree"),
        "git keeps it from `worktree remove` and `prune`: {porcelain}"
    );

    send_ended(&site, &nudges, "two", "R2");

    let evicted = thread(&site, "R1");
    assert_eq!(evicted["state"], "evicted", "{evicted}");
    assert!(!first.exists());
    assert!(
        !git_out(&repo.root, &["worktree", "list"]).contains("/r1"),
        "git forgot it"
    );
    let branch = evicted["branch"].as_str().unwrap();
    assert_eq!(git_out(&repo.root, &["branch", "--list", branch]), branch);

    // R2 is idle now, and goes for R1's worktree, made again from R1's branch.
    let second = worktree(&site, "R2");
    let (status, body) = follow(&site, "R1");
    assert_eq!(status, StatusCode::OK, "{body}");
    let run = wait_for_run(&site, &nudges, "R3");
    assert_eq!(run["status"], "ended", "{run}");
    // Its changes view starts at the branch's commit the worktree was made at.
    let tip = git_out(&first, &["rev-parse", "HEAD"]);
    assert!(
        run["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["point"] == "before" && entry["tree"] == tip.as_str()),
        "{run}"
    );
    assert_eq!(thread(&site, "R1")["state"], "open");
    assert_eq!(thread(&site, "R2")["state"], "evicted");
    assert!(!second.exists());
    assert!(
        git_out(&first, &["log", "--format=%s"]).contains("Postil R1: notes N1"),
        "made from the branch, not the fork"
    );
    assert_eq!(git_out(&first, &["branch", "--show-current"]), branch);

    // An evicted thread's changes are on its branch: Apply needs no worktree.
    let (status, body) = end(&site, "apply", "R2");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fs::read_to_string(repo.root.join("new.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(thread(&site, "R2")["state"], "applied");
}

#[test]
fn a_follow_up_in_a_thread_another_window_evicted_makes_its_worktree_again() {
    // Window A ran R1 and keeps its agent.
    let (repo, a, a_nudges, first) = written("room-two-windows");
    // Window B, over a cap of 1, evicts R1, which A's agent cannot see.
    let (b, b_nudges) = run_site(&repo, "writes");
    b.runner().unwrap().set_test_disk(TestDisk {
        max_worktrees: Some(1),
        ..TestDisk::default()
    });
    send_ended(&b, &b_nudges, "two", "R2");
    assert!(!first.exists());

    assert_eq!(follow(&a, "R1").0, StatusCode::OK);
    let run = wait_for_run(&a, &a_nudges, "R3");

    // A fresh agent, in a worktree made again; A's kept one would work in the deleted folder.
    assert_eq!(run["status"], "ended", "{run}");
    assert!(first.is_dir(), "made again");
    assert_eq!(thread(&a, "R1")["state"], "open");
    let prompt = run["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("R1 was asked")
            && prompt.ends_with(&format!(
                "now asks:\n\nmore\n\n{}",
                crate::run::REPORT_AGAIN
            )),
        "the fresh agent is told the thread so far: {prompt}"
    );
}

#[test]
fn the_idle_thread_whose_latest_run_is_oldest_is_evicted_first() {
    let (_repo, site, nudges, first) = written("room-order");
    send_ended(&site, &nudges, "two", "R2");
    // R1's follow-up makes R2's run the older of the two.
    assert_eq!(follow(&site, "R1").0, StatusCode::OK);
    wait_for_run(&site, &nudges, "R3");
    site.runner().unwrap().set_test_disk(TestDisk {
        max_worktrees: Some(2),
        ..TestDisk::default()
    });

    send_ended(&site, &nudges, "three", "R4");

    assert_eq!(thread(&site, "R2")["state"], "evicted");
    assert_eq!(thread(&site, "R1")["state"], "open");
    assert!(first.is_dir());
}

#[test]
fn a_thread_with_work_not_committed_is_kept_and_a_send_past_the_cap_is_refused() {
    let (repo, site, nudges, first) = written("room-kept");
    site.runner().unwrap().set_test_disk(TestDisk {
        max_worktrees: Some(1),
        ..TestDisk::default()
    });
    fs::write(first.join("mine.txt"), "mine\n").unwrap();
    post_note(&site, "a.txt", 1, 1, "two");

    send(&site, &["N2"]);
    let failed = wait_for(&site, &nudges, "failed");

    let error = failure(&failed);
    assert!(
        error.contains("has 1 worktrees")
            && error.contains("kept: R1 (")
            && error.contains("mine.txt"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(first.join("mine.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(thread(&site, "R1")["state"], "open");
    assert_eq!(
        get(&site, "postil://localhost/api/thread?id=R2"),
        (StatusCode::OK, "null".to_owned()),
        "a new thread whose worktree was not made leaves no thread"
    );
    assert!(!repo.root.join(".git/postil/threads/R2.lock").exists());
}

#[test]
fn short_of_space_an_idle_thread_loses_its_build_folders_then_its_worktree() {
    let repo = TempRepo::new("room-floor");
    repo.write(".gitignore", "target/\n");
    repo.write("a.txt", "a\n");
    fs::create_dir_all(repo.root.join("target/debug")).unwrap();
    repo.write("target/debug/app", "built\n");
    let (site, nudges) = run_site(&repo, "writes");
    send_ended(&site, &nudges, "one", "R1");
    let first = worktree(&site, "R1");
    let built = first.join("target");
    assert!(built.is_dir(), "cloned into the worktree");
    let runner = site.runner().unwrap();
    // Short of space until R1's build folder is gone.
    let watched = built.clone();
    runner.set_test_disk(TestDisk {
        free_space: Some(Box::new(
            move || {
                if watched.exists() { FLOOR - 1 } else { FLOOR }
            },
        )),
        ..TestDisk::default()
    });

    send_ended(&site, &nudges, "two", "R2");

    assert!(!built.exists());
    let trash: Vec<_> = fs::read_dir(repo.home().join("trash"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(trash.is_empty(), "deleted, not only moved: {trash:?}");
    assert!(first.join("a.txt").is_file(), "the worktree stays");
    assert_eq!(thread(&site, "R1")["state"], "open");

    // Short whatever goes: every idle thread is evicted, then the run is refused.
    let second = worktree(&site, "R2");
    runner.set_test_disk(TestDisk {
        free_space: Some(Box::new(|| FLOOR - 1)),
        ..TestDisk::default()
    });
    post_note(&site, "a.txt", 1, 1, "three");
    send(&site, &["N3"]);
    let failed = wait_for(&site, &nudges, "failed");

    let error = failure(&failed);
    assert!(
        error.contains("19 GB is free") && error.contains("under the 20 GB"),
        "{error}"
    );
    assert!(!first.exists());
    assert!(!second.exists());
    assert_eq!(thread(&site, "R1")["state"], "evicted");
    assert_eq!(thread(&site, "R2")["state"], "evicted");
}

#[test]
fn a_run_in_a_worktree_on_disk_evicts_nothing_under_the_floor() {
    let (_repo, site, nudges, first) = written("room-made-floor");
    send_ended(&site, &nudges, "two", "R2");
    site.runner().unwrap().set_test_disk(TestDisk {
        free_space: Some(Box::new(|| FLOOR - 1)),
        ..TestDisk::default()
    });

    // R2's follow-up makes no worktree, so it takes none of R1's space.
    assert_eq!(follow(&site, "R2").0, StatusCode::OK);
    let run = wait_for_run(&site, &nudges, "R3");

    assert_eq!(run["status"], "ended", "{run}");
    assert_eq!(thread(&site, "R1")["state"], "open");
    assert!(first.is_dir());
}

#[test]
fn an_evicted_thread_whose_worktree_cannot_be_made_again_stays_evicted() {
    let (repo, site, nudges, _first) = written("room-remake-fails");
    site.runner().unwrap().set_test_disk(TestDisk {
        max_worktrees: Some(1),
        ..TestDisk::default()
    });
    send_ended(&site, &nudges, "two", "R2");
    let branch = thread(&site, "R1")["branch"].as_str().unwrap().to_owned();
    // Checked out in the engineer's tree: git makes no worktree on it.
    repo.git(&["checkout", "--quiet", &branch]);

    assert_eq!(follow(&site, "R1").0, StatusCode::OK);
    let run = wait_for_run(&site, &nudges, "R3");

    assert_eq!(run["status"], "failed", "{run}");
    assert!(failure(&run).contains("already"), "{run}");
    assert_eq!(thread(&site, "R1")["state"], "evicted");
}

#[test]
fn apply_on_an_evicted_thread_takes_the_engineer_s_commit_on_its_branch() {
    let (repo, site, nudges, first) = written("room-apply-commit");
    // The engineer's own commit in the worktree, past Postil's last.
    fs::write(first.join("mine.txt"), "mine\n").unwrap();
    git_out(&first, &["add", "mine.txt"]);
    git_out(&first, &["commit", "--quiet", "--message", "mine"]);
    site.runner().unwrap().set_test_disk(TestDisk {
        max_worktrees: Some(1),
        ..TestDisk::default()
    });
    send_ended(&site, &nudges, "two", "R2");
    assert_eq!(thread(&site, "R1")["state"], "evicted");

    let (status, body) = end(&site, "apply", "R1");

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fs::read_to_string(repo.root.join("mine.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn a_record_of_version_1_is_read_and_written_back_as_the_current_version() {
    let (repo, site, nudges, _first) = written("room-version");
    let file = repo.root.join(".git/postil/threads.json");
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    record["version"] = 1.into();
    record["threads"][0]
        .as_object_mut()
        .unwrap()
        .remove("lastRun")
        .expect("version 2 writes it");
    fs::write(&file, record.to_string()).unwrap();

    assert_eq!(follow(&site, "R1").0, StatusCode::OK);
    let run = wait_for_run(&site, &nudges, "R2");

    assert_eq!(run["status"], "ended", "{run}");
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(record["version"], 3);
    assert!(
        record["threads"][0]["lastRun"].as_u64().unwrap() > 0,
        "{record}"
    );
}

#[test]
fn opening_a_project_frees_a_clean_worktree_no_thread_names_and_keeps_one_with_work() {
    let (repo, site, nudges, first) = written("room-sweep");
    send_ended(&site, &nudges, "two", "R2");
    let second = worktree(&site, "R2");
    let branch = thread(&site, "R1")["branch"].as_str().unwrap().to_owned();
    fs::write(second.join("mine.txt"), "mine\n").unwrap();
    // Both threads taken out of the record by hand.
    let record = repo.root.join(".git/postil/threads.json");
    fs::write(&record, r#"{"version": 2, "threads": []}"#).unwrap();
    drop(site);

    let (_site, _nudges) = run_site(&repo, "plain");

    // Freed once git's record of it is gone too, which follows the folder's move.
    let listed = || git_out(&repo.root, &["worktree", "list"]).contains("/r1\n");
    let deadline = Instant::now() + WAIT;
    while first.exists() || listed() {
        assert!(Instant::now() < deadline, "{first:?} was not freed");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::read_to_string(second.join("mine.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        git_out(&repo.root, &["branch", "--list", &branch]),
        branch,
        "the branch is not freed with the folder"
    );
}
