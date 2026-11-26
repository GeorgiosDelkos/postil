//! Team memory: finding and setting up hippius-mem (`/api/memory/*`, the settings' `memory`
//! and `agentMemory`, memory.rs).
//!
//! Most tests run a fake `hippius-mem`, a shell script on a temporary search path: the real
//! one cannot be made to hang or to be missing. One runs the real program, when it is
//! installed, in a temporary home.

use super::*;
use crate::memory::{self, Memory, State};
use crate::testing::TempDir;

const CHECK: &str = "postil://localhost/api/memory/check";
const SETUP: &str = "postil://localhost/api/memory/setup";
const RESCAN: &str = "postil://localhost/api/agents/rescan";

/// A site with no project open whose agents and hippius-mem are looked for in `bin` only, its
/// memory made by `memory` from the plain one.
fn memory_site(repo: &TempRepo, bin: &TempDir, memory: impl FnOnce(Memory) -> Memory) -> Site {
    let path = bin.path().as_os_str().to_owned();
    let scanner = Scanner::new(move || path.clone());
    Site::new(Some(&repo.config()), None, None, scanner.clone(), quiet())
        .with_memory(memory(Memory::new(scanner)))
}

/// Writes the fake `hippius-mem` in `bin`: a shell script running `body`. It runs with the
/// search path as its `PATH`, which is `bin` alone, so `body` names other programs by their
/// absolute paths.
fn program(bin: &TempDir, body: &str) -> PathBuf {
    let path = bin.path().join(memory::PROGRAM);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The settings' `memory` once no check or set up runs.
fn settled(site: &Site) -> serde_json::Value {
    site.memory.wait();
    get_json(site, SETTINGS)["memory"].clone()
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

#[test]
fn the_state_follows_the_search_path_doctor_s_exit_code_and_the_program_s_removal() {
    let repo = TempRepo::new("memory-states");
    let bin = TempDir::new("memory-states-bin");
    let site = memory_site(&repo, &bin, |memory| memory);

    let view = settled(&site);
    assert_eq!(view["state"], serde_json::json!({ "kind": "missing" }));
    assert_eq!(view["pending"], false);
    assert_eq!(view["setup"], serde_json::Value::Null);

    // Exit 1 with nothing printed: no reason to show.
    let path = program(&bin, "exit 1");
    let path = path.to_str().unwrap();
    assert_eq!(post(&site, CHECK, "").0, StatusCode::OK);
    assert_eq!(
        settled(&site)["state"],
        serde_json::json!({ "kind": "unconfigured", "path": path, "problem": null })
    );

    // hippius-mem exits 1 for every error: a configured vault whose probe fails says so in
    // doctor's own words, which the page shows instead of offering Set up.
    let probe_failed = "INFO profile: team\nError: live encryption-boundary probe failed";
    program(&bin, &format!("printf '{probe_failed}\\n' >&2\nexit 1"));
    post(&site, CHECK, "");
    assert_eq!(settled(&site)["state"]["problem"], probe_failed);

    // Another exit code with nothing printed says the code.
    program(&bin, "exit 7");
    post(&site, CHECK, "");
    assert_eq!(
        settled(&site)["state"]["problem"],
        "hippius-mem doctor exited with 7"
    );

    // A rescan of the agents checks again too.
    program(&bin, "exit 0");
    assert_eq!(post(&site, RESCAN, "").0, StatusCode::OK);
    assert_eq!(
        settled(&site)["state"],
        serde_json::json!({ "kind": "ready", "path": path })
    );

    // Removed while Postil runs: the next read says so, with no check.
    fs::remove_file(path).unwrap();
    let view = get_json(&site, SETTINGS)["memory"].clone();
    assert_eq!(view["state"], serde_json::json!({ "kind": "missing" }));
    assert_eq!(view["pending"], false);
}

#[test]
fn a_doctor_that_hangs_is_killed_at_the_limit_with_what_it_left_running() {
    let repo = TempRepo::new("memory-hang");
    let bin = TempDir::new("memory-hang-bin");
    // Like a doctor stuck on the network, with a child of its own holding its output open.
    let pid_file = bin.path().join("child.pid");
    program(
        &bin,
        &format!("/bin/sleep 30 &\necho $! > {}\nwait", pid_file.display()),
    );
    let site = memory_site(&repo, &bin, |memory| {
        memory.with_doctor_timeout(Duration::from_secs(1))
    });

    // The settings answer while doctor hangs.
    let view = get_json(&site, SETTINGS)["memory"].clone();
    assert_eq!(view["pending"], true);
    assert_eq!(view["state"], serde_json::Value::Null);

    let State::Unconfigured { problem, .. } = site.memory.wait() else {
        panic!("a hanging doctor is not ready: {:?}", site.memory.wait());
    };
    assert_eq!(
        problem.as_deref(),
        Some("hippius-mem doctor did not answer within 1 s")
    );

    let pid = fs::read_to_string(&pid_file).unwrap();
    assert!(!pid.trim().is_empty(), "doctor wrote its child's pid");
    // Killed with the group; launchd reaps it a moment later.
    let deadline = Instant::now() + WAIT;
    while alive(pid.trim()) {
        assert!(Instant::now() < deadline, "doctor's child still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_older_check_that_ends_after_a_newer_one_does_not_replace_its_state() {
    let repo = TempRepo::new("memory-stale");
    let bin = TempDir::new("memory-stale-bin");
    let entered = bin.path().join("entered");
    let gate = bin.path().join("gate");
    let first = bin.path().join("first");
    make_fifo(&entered);
    make_fifo(&gate);
    // The first check says it started, waits for the gate and fails; any later one passes.
    program(
        &bin,
        &format!(
            "if [ ! -f {first} ]; then\n  /usr/bin/touch {first}\n  echo in > {entered}\n  \
             /bin/cat {gate} > /dev/null\n  exit 1\nfi\nexit 0",
            first = first.display(),
            entered = entered.display(),
            gate = gate.display(),
        ),
    );
    let site = memory_site(&repo, &bin, |memory| memory);

    get_json(&site, SETTINGS);
    read_fifo(&entered, WAIT);
    assert_eq!(post(&site, CHECK, "").0, StatusCode::OK);
    assert!(matches!(site.memory.wait(), State::Ready { .. }));

    fs::write(&gate, "go").unwrap();
    site.memory.wait_finished(2);
    assert_eq!(
        settled(&site)["state"]["kind"],
        "ready",
        "the older check's exit 1 is dropped"
    );
}

#[test]
fn doctor_runs_in_the_open_project_and_again_when_another_opens() {
    let repo = TempRepo::new("memory-cwd");
    let other = TempRepo::new("memory-cwd-other");
    let bin = TempDir::new("memory-cwd-bin");
    let cwd = bin.path().join("cwd");
    program(&bin, &format!("/bin/pwd -P > {}\nexit 0", cwd.display()));
    let site = memory_site(&repo, &bin, |memory| memory);
    let ran_in = || fs::read_to_string(&cwd).unwrap().trim().to_owned();
    let root = |repo: &TempRepo| fs::canonicalize(&repo.root).unwrap().display().to_string();

    // No project: the temporary directory, never Postil's own working directory.
    site.memory.wait();
    let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
    assert_eq!(ran_in(), temp.display().to_string());

    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();
    site.memory.wait();
    assert_eq!(
        ran_in(),
        root(&repo),
        "opening a project checks again there"
    );

    site.open(Repo::open(&other.root).unwrap(), LiveRun::Stop)
        .unwrap();
    site.memory.wait();
    assert_eq!(ran_in(), root(&other));
}

#[test]
fn set_up_runs_quickstart_no_wire_once_and_shows_what_it_printed_as_it_printed_it() {
    let repo = TempRepo::new("memory-setup");
    let bin = TempDir::new("memory-setup-bin");
    let dir = bin.path().display();
    let started = bin.path().join("started");
    let gate = bin.path().join("gate");
    make_fifo(&started);
    make_fifo(&gate);
    // `cat` ends only once standard input is closed; the first set up then says it started
    // and waits for the gate, so the test can ask again while it runs.
    program(
        &bin,
        &format!(
            r#"case "$*" in
doctor) [ -f {dir}/configured ] && exit 0; exit 1 ;;
"quickstart --no-wire")
  /bin/cat > /dev/null
  if [ -f {dir}/configured ]; then
    echo "Error: a config already exists at /tmp/c; run doctor" >&2
    exit 1
  fi
  echo started > {dir}/started
  /bin/cat {dir}/gate > /dev/null
  /usr/bin/touch {dir}/configured
  echo "INFO probe passed" >&2
  echo "Trial vault ready at /tmp/v."
  echo "  1. Ask an agent to remember something."
  exit 0 ;;
*) echo "unexpected arguments: $*" >&2; exit 9 ;;
esac"#
        ),
    );
    let site = memory_site(&repo, &bin, |memory| memory);
    assert!(matches!(site.memory.wait(), State::Unconfigured { .. }));

    let (status, reply) = post(&site, SETUP, "");
    assert_eq!(status, StatusCode::OK, "{reply}");
    let view: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(
        view["memory"]["setup"],
        serde_json::json!({ "status": "running" })
    );
    assert_eq!(view["memory"]["pending"], true);
    read_fifo(&started, WAIT);

    // One set up at a time, and no check beside it: its doctor would race the set up.
    for uri in [SETUP, CHECK] {
        let (status, reply) = post(&site, uri, "");
        assert_eq!(status, StatusCode::CONFLICT, "{uri}: {reply}");
        assert!(reply.contains("set up is still running"), "{reply}");
    }
    assert_eq!(
        post(&site, RESCAN, "").0,
        StatusCode::OK,
        "a rescan still runs"
    );
    fs::write(&gate, "go").unwrap();

    let view = settled(&site);
    assert_eq!(view["state"]["kind"], "ready", "{view}");
    assert_eq!(
        view["setup"],
        serde_json::json!({
            "status": "succeeded",
            "output": "Trial vault ready at /tmp/v.\n  1. Ask an agent to remember something.",
        }),
        "its standard output, which names the paths, and nothing else"
    );

    // A second set up is refused by quickstart itself, in its own words.
    assert_eq!(post(&site, SETUP, "").0, StatusCode::OK);
    let view = settled(&site);
    assert_eq!(
        view["setup"],
        serde_json::json!({
            "status": "failed",
            "error": "Error: a config already exists at /tmp/c; run doctor",
        })
    );
    assert_eq!(view["state"]["kind"], "ready");
}

#[test]
fn set_up_without_the_program_says_it_is_not_installed() {
    let repo = TempRepo::new("memory-setup-missing");
    let bin = TempDir::new("memory-setup-missing-bin");
    let site = memory_site(&repo, &bin, |memory| memory);

    assert_eq!(post(&site, SETUP, "").0, StatusCode::OK);
    let view = settled(&site);
    assert_eq!(view["state"], serde_json::json!({ "kind": "missing" }));
    assert_eq!(
        view["setup"],
        serde_json::json!({ "status": "failed", "error": "hippius-mem is not installed on PATH" })
    );
}

#[test]
fn team_memory_turns_on_only_when_ready_and_stays_on_when_the_program_goes() {
    let repo = TempRepo::new("memory-switch");
    let bin = TempDir::new("memory-switch-bin");
    let file = repo.config().join("settings.json");
    let site = memory_site(&repo, &bin, |memory| memory);
    let refused = |expected: &str| {
        let (status, reply) = save(&site, r#"{"agentMemory": true}"#);
        assert_eq!(status, StatusCode::CONFLICT, "{reply}");
        assert!(
            reply.contains(&format!("cannot turn team memory on: {expected}")),
            "{reply}"
        );
    };

    site.memory.wait();
    refused("hippius-mem is not installed on PATH");
    assert!(!file.exists(), "nothing is written");

    program(&bin, "exit 1");
    post(&site, CHECK, "");
    site.memory.wait();
    refused("hippius-mem is not set up");

    // While a check runs, the state is not known yet.
    let gate = bin.path().join("gate");
    make_fifo(&gate);
    program(
        &bin,
        &format!("/bin/cat {} > /dev/null\nexit 0", gate.display()),
    );
    post(&site, CHECK, "");
    refused("hippius-mem is still being checked");
    fs::write(&gate, "go").unwrap();
    assert!(matches!(site.memory.wait(), State::Ready { .. }));

    let (status, reply) = save(&site, r#"{"agentMemory": true}"#);
    assert_eq!(status, StatusCode::OK, "{reply}");
    let stored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(stored["version"], 4);
    assert_eq!(stored["settings"]["agentMemory"], true);

    // Uninstalled later: other changes still save, and it can be turned off, not on again.
    fs::remove_file(bin.path().join(memory::PROGRAM)).unwrap();
    let (status, reply) = save(&site, r#"{"agentMemory": true, "theme": "dark"}"#);
    assert_eq!(status, StatusCode::OK, "{reply}");
    let (status, reply) = save(&site, r#"{"agentMemory": false}"#);
    assert_eq!(status, StatusCode::OK, "{reply}");
    refused("hippius-mem is not installed on PATH");
}

#[test]
fn the_real_hippius_mem_sets_up_a_trial_vault_in_a_temporary_home_that_doctor_reads_as_ready() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(installed) = agents::find_program(memory::PROGRAM, &path) else {
        eprintln!("skipped: hippius-mem is not installed on PATH");
        return;
    };
    let repo = TempRepo::new("memory-real");
    let home = TempDir::new("memory-real-home");
    // The engineer's copy of the embedding model, when there is one, so the test does not
    // download 129 MB. hippius-mem takes lock files in it, and downloads the model into it
    // when the model is missing.
    let model_cache = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache/hippius-mem/fastembed"))
        .filter(|cache| cache.is_dir());
    let scanner = Scanner::new(move || path.clone());
    let site = Site::new(Some(&repo.config()), None, None, scanner.clone(), quiet())
        .with_memory(Memory::new(scanner).with_home(home.path(), model_cache.as_deref()));

    let State::Unconfigured { path, problem } = site.memory.wait() else {
        panic!("a fresh home has no configuration");
    };
    assert_eq!(path, installed);
    // The words the page offers Set up on (`canSetUp` in ui/settings.js).
    let problem = problem.unwrap_or_default();
    assert!(
        problem.contains("failed to load configuration"),
        "{problem}"
    );

    assert_eq!(post(&site, SETUP, "").0, StatusCode::OK);
    let view = settled(&site);
    let error = view["setup"]["error"].as_str().unwrap_or("");
    if error.contains("embedding model") {
        eprintln!("skipped: hippius-mem could not get its embedding model (no network?): {error}");
        return;
    }
    assert_eq!(view["setup"]["status"], "succeeded", "{view}");
    let output = view["setup"]["output"].as_str().unwrap();
    assert!(
        output.contains(home.path().to_str().unwrap()),
        "the paths it printed are in the temporary home: {output}"
    );
    assert_eq!(view["state"]["kind"], "ready", "{view}");
    assert!(
        !home.path().join(".claude").exists(),
        "--no-wire leaves Claude Code's configuration alone"
    );

    assert_eq!(post(&site, CHECK, "").0, StatusCode::OK);
    assert_eq!(settled(&site)["state"]["kind"], "ready");

    assert_eq!(post(&site, SETUP, "").0, StatusCode::OK);
    let view = settled(&site);
    assert_eq!(view["setup"]["status"], "failed");
    let error = view["setup"]["error"].as_str().unwrap();
    assert!(error.contains("a config already exists"), "{error}");
}
