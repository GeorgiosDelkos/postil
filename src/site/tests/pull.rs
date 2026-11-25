//! Pull requests from threads (`run/pull.rs`) through `/api/thread/pr`: the branch pushed to a
//! real remote (a bare repository beside the test's), and a fake `gh` first on the search path,
//! since GitHub cannot run here. The fake writes what it was asked to files beside it, prints
//! a pull request's URL or the state in its `state` file, waits while a `hold` file is there,
//! fails while a `fail` file is there, and says a pull request exists while `exists` is.

use super::threads::{end, thread};
use super::*;
use crate::agents::find_program;

const FAKE_GH: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
while [ -f "$dir/hold" ]; do sleep 0.02; done
if [ -f "$dir/fail" ]; then
  echo "gh: not logged in to github.com" >&2
  exit 1
fi
if [ -f "$dir/exists" ]; then
  echo "a pull request for branch \"x\" into branch \"trunk\" already exists:" >&2
  echo "https://github.com/owner/repo/pull/3" >&2
  exit 1
fi
case "$1 $2" in
"pr create") cat > "$dir/body"; echo "https://github.com/owner/repo/pull/7" ;;
"pr view") cat "$dir/state" 2>/dev/null || echo OPEN ;;
esac
"#;

/// A repository whose `origin` is a bare repository beside it, with `trunk` pushed, and a site
/// whose agent writes `a.txt` and whose search path finds the fake `gh` first. Returns the
/// folder of the fake `gh`, and the bare repository.
fn published(name: &str) -> (TempRepo, Site, mpsc::Receiver<()>, PathBuf, PathBuf) {
    let repo = TempRepo::new(name);
    repo.write("a.txt", "a\n");
    // In the site's home folder, which the repository's drop removes.
    let gh = repo.home().join("test-gh");
    fs::create_dir_all(&gh).unwrap();
    fs::write(gh.join("gh"), FAKE_GH).unwrap();
    fs::set_permissions(gh.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
    let bare = repo.home().join("remote.git");
    git_out(
        &repo.root,
        &["init", "--quiet", "--bare", bare.to_str().unwrap()],
    );
    let mut dirs = vec![gh.clone()];
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH is set"),
    ));
    let (site, nudges) = site_on(&repo, &dirs);
    repo.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
    repo.git(&["push", "--quiet", "origin", "trunk"]);
    (repo, site, nudges, gh, bare)
}

/// As `run_site` with the "writes" agent, with `dirs` as the search path.
fn site_on(repo: &TempRepo, dirs: &[PathBuf]) -> (Site, mpsc::Receiver<()>) {
    repo.commit();
    let path = std::env::join_paths(dirs).unwrap();
    let (sender, nudges) = mpsc::channel();
    let notify: Notify = Arc::new(move || sender.send(()).unwrap_or_default());
    let site = Site::new(
        Some(&repo.config()),
        Some(&repo.home()),
        None,
        Scanner::new(move || path.clone()),
        notify,
    );
    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();
    set_agents(&site, &[("Fake", &fake_agent("writes"))], "Fake");
    (site, nudges)
}

/// Sends a note `body` on `a.txt` and waits for its run to end.
fn sent(site: &Site, nudges: &mpsc::Receiver<()>, body: &str) {
    let note = post_note(site, "a.txt", 1, 1, body);
    let id = note["notes"].as_array().unwrap().last().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, reply) = send(site, &[&id]);
    assert_eq!(status, StatusCode::OK, "{reply}");
    wait_for(site, nudges, "ended");
}

/// Asks for thread `id`'s pull request to be opened, or its state read, as the page does.
fn ask(site: &Site, route: &str, id: &str) -> (StatusCode, String) {
    let body = serde_json::json!({ "thread": id }).to_string();
    post(
        site,
        &format!("postil://localhost/api/thread/{route}"),
        &body,
    )
}

/// Thread `id` once this window is done with its pull request, nudge by nudge.
fn settled(site: &Site, nudges: &mpsc::Receiver<()>, id: &str) -> serde_json::Value {
    loop {
        let record = thread(site, id);
        let work = &record["pullRequestWork"];
        if work.is_null() || work["kind"] == "failed" {
            return record;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("the pull request work never ended: {record}"));
    }
}

#[test]
fn open_pull_request_pushes_the_branch_and_opens_a_draft_into_the_base_branch() {
    let (repo, site, nudges, gh, bare) = published("pull-open");
    sent(&site, &nudges, "Fix the parser\nIt drops paths.");

    let (status, body) = ask(&site, "pr", "R1");
    assert_eq!(status, StatusCode::OK, "{body}");
    let record = settled(&site, &nudges, "R1");

    assert_eq!(
        record["pullRequest"],
        serde_json::json!({ "url": "https://github.com/owner/repo/pull/7", "state": "open" }),
        "{record}"
    );
    let branch = record["branch"].as_str().unwrap();
    assert_eq!(
        git_out(&bare, &["rev-parse", &format!("refs/heads/{branch}")]),
        git_out(&repo.root, &["rev-parse", branch]),
        "the branch is on the remote at its tip"
    );
    let calls = fs::read_to_string(gh.join("calls")).unwrap();
    assert!(
        calls.contains(&format!(
            "pr create --draft --base trunk --head {branch} --title Fix the parser --body-file -"
        )),
        "{calls}"
    );
    let pr_body = fs::read_to_string(gh.join("body")).unwrap();
    assert!(pr_body.starts_with("**N1**, `a.txt:1`"), "{pr_body}");
    assert!(pr_body.contains("It drops paths."), "{pr_body}");
    // Not the agent's prompt: no instructions, and no quoted code, which can hold a secret.
    assert!(
        !pr_body.contains("Address every note") && !pr_body.contains("> a"),
        "{pr_body}"
    );
    assert!(
        pr_body.ends_with("Opened from Postil thread R1."),
        "{pr_body}"
    );

    let (status, body) = ask(&site, "pr", "R1");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("has a pull request already"), "{body}");

    // Merged on GitHub: read again when the page asks, as it does when the window regains
    // focus.
    fs::write(gh.join("state"), "MERGED\n").unwrap();
    assert_eq!(ask(&site, "pr/state", "R1").0, StatusCode::OK);
    let record = settled(&site, &nudges, "R1");
    assert_eq!(record["pullRequest"]["state"], "merged", "{record}");
    assert_eq!(
        record["state"], "open",
        "a merged pull request does not end the thread"
    );
}

#[test]
fn a_pull_request_needs_the_base_branch_on_the_remote_with_every_commit_the_thread_starts_from() {
    let (repo, site, nudges, _gh, bare) = published("pull-refused");
    sent(&site, &nudges, "one");
    // No remote at all.
    repo.git(&["remote", "remove", "origin"]);
    let (status, body) = ask(&site, "pr", "R1");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("trunk tracks no remote, and there is no origin"),
        "{body}"
    );

    // A remote without the base branch.
    repo.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
    let (status, body) = ask(&site, "pr", "R1");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("trunk is not on origin: push trunk first"),
        "{body}"
    );

    // A thread started from a commit of trunk that origin lacks: the pull request would show it.
    repo.git(&["fetch", "--quiet", "origin"]);
    repo.write("b.txt", "b\n");
    repo.git(&["add", "b.txt"]);
    repo.git(&["commit", "--quiet", "--message", "Only here"]);
    sent(&site, &nudges, "two");
    let (status, body) = ask(&site, "pr", "R2");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("Only here") && body.contains("push trunk first"),
        "{body}"
    );
    assert!(thread(&site, "R2")["pullRequestWork"].is_null());
}

#[test]
fn a_failed_gh_shows_on_the_thread_and_open_pull_request_can_be_asked_again() {
    let (_repo, site, nudges, gh, _bare) = published("pull-fails");
    sent(&site, &nudges, "one");
    fs::write(gh.join("fail"), "").unwrap();

    assert_eq!(ask(&site, "pr", "R1").0, StatusCode::OK);
    let record = settled(&site, &nudges, "R1");

    assert!(record["pullRequest"].is_null(), "{record}");
    let error = record["pullRequestWork"]["error"].as_str().unwrap();
    assert!(
        error.contains("gh pr create failed") && error.contains("not logged in"),
        "{record}"
    );

    fs::remove_file(gh.join("fail")).unwrap();
    assert_eq!(ask(&site, "pr", "R1").0, StatusCode::OK);
    let record = settled(&site, &nudges, "R1");
    assert_eq!(record["pullRequest"]["state"], "open", "{record}");
    assert!(record["pullRequestWork"].is_null(), "{record}");
}

#[test]
fn a_pull_request_is_refused_from_a_detached_head_or_over_work_not_committed() {
    let (repo, site, nudges, _gh, _bare) = published("pull-local");
    sent(&site, &nudges, "one");
    let worktree = PathBuf::from(thread(&site, "R1")["path"].as_str().unwrap());
    fs::write(worktree.join("mine.txt"), "mine\n").unwrap();

    let (status, body) = ask(&site, "pr", "R1");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("mine.txt"),
        "the push would leave it out: {body}"
    );

    repo.git(&["checkout", "--quiet", "--detach"]);
    sent(&site, &nudges, "two");
    let (status, body) = ask(&site, "pr", "R2");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("detached HEAD"), "{body}");
}

#[test]
fn a_thread_cannot_be_ended_while_its_pull_request_is_being_opened() {
    let (_repo, site, nudges, gh, _bare) = published("pull-held");
    sent(&site, &nudges, "one");
    fs::write(gh.join("hold"), "").unwrap();
    assert_eq!(ask(&site, "pr", "R1").0, StatusCode::OK);
    // The push may still be running; gh waits on `hold` once it starts.
    let deadline = Instant::now() + WAIT;
    while !gh.join("calls").exists() {
        assert!(Instant::now() < deadline, "gh never ran");
        std::thread::sleep(Duration::from_millis(20));
    }

    let (status, body) = end(&site, "discard", "R1");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("opening its pull request"), "{body}");

    fs::remove_file(gh.join("hold")).unwrap();
    let record = settled(&site, &nudges, "R1");
    assert_eq!(record["pullRequest"]["state"], "open", "{record}");
    let (status, body) = end(&site, "discard", "R1");
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn a_pull_request_github_has_already_is_recorded_from_a_version_2_record() {
    let (repo, site, nudges, gh, _bare) = published("pull-exists");
    sent(&site, &nudges, "one");
    let file = repo.root.join(".git/postil/threads.json");
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    record["version"] = 2.into();
    fs::write(&file, record.to_string()).unwrap();
    fs::write(gh.join("exists"), "").unwrap();

    assert_eq!(ask(&site, "pr", "R1").0, StatusCode::OK);
    let record = settled(&site, &nudges, "R1");

    assert_eq!(
        record["pullRequest"]["url"], "https://github.com/owner/repo/pull/3",
        "{record}"
    );
    assert!(record["pullRequestWork"].is_null(), "{record}");
}

#[test]
fn a_base_branch_tracking_another_name_is_the_base_by_that_name() {
    let (repo, site, nudges, gh, _bare) = published("pull-upstream");
    repo.git(&["push", "--quiet", "origin", "trunk:develop"]);
    repo.git(&[
        "branch",
        "--quiet",
        "--set-upstream-to",
        "origin/develop",
        "trunk",
    ]);
    sent(&site, &nudges, "one");

    assert_eq!(ask(&site, "pr", "R1").0, StatusCode::OK);
    settled(&site, &nudges, "R1");

    let calls = fs::read_to_string(gh.join("calls")).unwrap();
    assert!(calls.contains("--base develop "), "{calls}");
}

#[test]
fn without_gh_the_thread_says_how_to_install_it() {
    let (repo, _site, _nudges, _gh, _bare) = published("pull-no-gh");
    // A search path with node, for the agent, and git, and no gh.
    let bin = repo.home().join("bin-without-gh");
    fs::create_dir_all(&bin).unwrap();
    let path = std::env::var_os("PATH").expect("PATH is set");
    for program in ["node", "git"] {
        let found = find_program(program, &path).expect("node and git are installed");
        std::os::unix::fs::symlink(found, bin.join(program)).unwrap();
    }
    let (site, nudges) = site_on(&repo, &[bin]);
    // The site committed the files again: origin must have that commit too.
    repo.git(&["push", "--quiet", "origin", "trunk"]);
    sent(&site, &nudges, "one");

    assert_eq!(ask(&site, "pr", "R1").0, StatusCode::OK);
    let record = settled(&site, &nudges, "R1");

    let error = record["pullRequestWork"]["error"].as_str().unwrap();
    assert!(error.contains("brew install gh"), "{record}");
}
