//! Team memory: finding and setting up hippius-mem (`/api/memory/*`, the settings' `memory`
//! and `agentMemory`, memory.rs).
//!
//! Most tests run a fake `hippius-mem`, a shell script on a temporary search path: the real
//! one cannot be made to hang or to be missing. One runs the real program, when it is
//! installed, in a temporary home.

use super::*;
use crate::memory::{self, Memory, State};
use crate::notes;
use crate::settings::TeamMemory;
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
// ---- The memory server ----------------------------------------------------------------------

const RECALL: &str = "postil://localhost/api/memory/recall";
const MEMORY: &str = "postil://localhost/api/memory";
const REFRESH: &str = "postil://localhost/api/memory/refresh";

/// `value` as a tool result's text: JSON in a JSON string, without its quotes, to go in a
/// shell script's single quotes.
fn tool_text(value: &serde_json::Value) -> String {
    let quoted = serde_json::to_string(&value.to_string()).unwrap();
    quoted[1..quoted.len() - 1].to_owned()
}

/// `recall`'s result: one strong pointer and one weak one.
fn pointers() -> String {
    tool_text(&serde_json::json!({
        "pointers": [
            { "id": "mem_01A", "summary": "a.rs parses lazily", "score": 0.9, "repo": "postil",
              "author": "5CrAuthor", "updated": 1_795_733_249_727_u64 },
            { "id": "mem_01B", "summary": "maybe related", "score": 0.1, "weak": true,
              "repo": "global", "author": "5CrOther", "updated": 1_795_733_249_000_u64 },
        ],
        "total_matched": 2,
        "returned": 2,
        "semantic": true,
    }))
}

/// `get`'s and `history`'s results for `mem_01A`: its first op not anchored, its second
/// anchored in the vault, its third on chain.
fn note_and_history() -> (String, String) {
    let note = serde_json::json!({
        "id": "mem_01A", "note_type": "gotcha", "repo": "postil", "author": "5CrAuthor",
        "created": 1, "updated": 2, "tags": [], "summary": "a.rs parses lazily",
        "body": "It *is* lazy.", "version": "ab",
    });
    let op = |id: &str, lamport: u64, kind: &str, anchor: serde_json::Value| {
        serde_json::json!({
            "op_id": id, "author": "5CrAuthor", "author_key": "22", "lamport": lamport,
            "kind": kind, "cid": "ab", "op_hash": "cd", "anchor": anchor,
        })
    };
    let anchored = serde_json::json!({
        "root": "00", "reference": { "Local": { "seq": 3 } }, "proof": {},
    });
    let on_chain = serde_json::json!({
        "root": "00",
        "reference": { "OnChain": { "block_hash": "0x1", "extrinsic_hash": "0x2" } },
        "proof": {},
    });
    let history = serde_json::json!({
        "note_id": "mem_01A", "tombstoned": false, "redacted": false, "links": [],
        "entries": [
            op("01", 1, "Remember", serde_json::Value::Null),
            op("02", 2, "Edit", anchored),
            op("03", 3, "Link", on_chain),
        ],
    });
    (tool_text(&note), tool_text(&history))
}

/// A `tools/list` result naming `tools`.
fn tool_list(tools: &[&str]) -> String {
    let tools: Vec<_> = tools
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    serde_json::json!({ "tools": tools }).to_string()
}

/// Writes a fake `hippius-mem` in `bin` ([`server_script`]).
fn server(bin: &TempDir, version: &str, arms: &str) -> PathBuf {
    program(bin, &server_script(bin.path(), version, arms))
}

/// The body of a fake `hippius-mem` that passes doctor, prints `version` on `--version`, and
/// with no arguments serves MCP over stdio as hippius-mem 0.7.1 does, line by line. Each server
/// appends its pid to `dir/servers` and each line it reads to `dir/calls`. `arms` are shell
/// `case` arms tried before the default ones, matched against the request line; in them `$id`
/// is the request's id, `reply ID RESULT` answers with a result and `text ID JSON` with a tool
/// result whose text is `JSON`. The recall finds one strong pointer and one weak one.
pub(super) fn server_script(dir: &Path, version: &str, arms: &str) -> String {
    let dir = dir.display();
    let initialized = serde_json::json!({
        "protocolVersion": "2025-06-18",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "rmcp", "version": "3.9.0" },
    });
    let tools = tool_list(&["recall", "get", "history", "remember", "refresh"]);
    let (note, history) = note_and_history();
    let pointers = pointers();
    let refreshed = tool_text(&serde_json::json!({ "indexed": 3 }));
    format!(
        r#"case "$*" in
--version) echo "{version}"; exit 0 ;;
doctor) exit 0 ;;
"") ;;
*) echo "unexpected arguments: $*" >&2; exit 9 ;;
esac
echo $$ >> {dir}/servers
reply() {{ printf '{{"jsonrpc":"2.0","id":%s,"result":%s}}\n' "$1" "$2"; }}
text() {{ reply "$1" "{{\"content\":[{{\"type\":\"text\",\"text\":\"$2\"}}],\"isError\":false}}"; }}
while IFS= read -r line; do
  printf '%s\n' "$line" >> {dir}/calls
  id=$(printf '%s' "$line" | /usr/bin/sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
{arms}
  *'"method":"initialize"'*) reply "$id" '{initialized}' ;;
  *'"method":"tools/list"'*) reply "$id" '{tools}' ;;
  *'"name":"recall"'*) text "$id" '{pointers}' ;;
  *'"name":"get"'*) text "$id" '{note}' ;;
  *'"name":"history"'*) text "$id" '{history}' ;;
  *'"name":"refresh"'*) text "$id" '{refreshed}' ;;
  esac
done"#
    )
}

/// A site whose hippius-mem is looked for in `bin` only, with `repo` open as its project.
fn server_site(repo: &TempRepo, bin: &TempDir, memory: impl FnOnce(Memory) -> Memory) -> Site {
    let site = memory_site(repo, bin, memory);
    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();
    site
}

/// Calls `method uri` until its answer is no longer pending, and returns it.
fn settle(site: &Site, method: &Method, uri: &str) -> serde_json::Value {
    loop {
        let (status, body) = call(site, method.clone(), uri, "");
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let reply: serde_json::Value = serde_json::from_str(&body).unwrap();
        if reply["state"] != "pending" {
            return reply;
        }
        site.memory.wait_idle();
    }
}

fn recall(site: &Site, path: &str) -> serde_json::Value {
    settle(site, &Method::GET, &format!("{RECALL}?path={path}"))
}

/// The pids of the servers started, in order.
fn servers(bin: &TempDir) -> Vec<String> {
    fs::read_to_string(bin.path().join("servers"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The lines the servers read, in order.
fn calls(bin: &TempDir) -> String {
    fs::read_to_string(bin.path().join("calls")).unwrap_or_default()
}

/// Waits until the process `pid` is gone and reaped.
fn wait_gone(pid: &str) {
    let deadline = Instant::now() + WAIT;
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_file_s_recall_is_pending_then_kept_and_names_the_repository_as_hippius_mem_does() {
    let repo = TempRepo::new("memory-recall");
    repo.git(&["remote", "add", "origin", "git@github.com:Owner/postil.git"]);
    let bin = TempDir::new("memory-recall-bin");
    server(&bin, "hippius-mem 0.7.1", "");
    let site = server_site(&repo, &bin, |memory| memory);

    // A path with a space and a character outside ASCII, as the page encodes it.
    let uri = format!("{RECALL}?path=src%2Fcaf%C3%A9%20notes.rs");
    let (status, body) = get(&site, &uri);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"state":"pending"}"#);
    let reply = settle(&site, &Method::GET, &uri);
    assert_eq!(
        reply,
        serde_json::json!({
            "state": "ready",
            "pointers": [
                { "id": "mem_01A", "summary": "a.rs parses lazily", "weak": false,
                  "author": "5CrAuthor", "updated": 1_795_733_249_727_u64 },
                { "id": "mem_01B", "summary": "maybe related", "weak": true,
                  "author": "5CrOther", "updated": 1_795_733_249_000_u64 },
            ],
        })
    );
    let sent = calls(&bin);
    assert!(
        sent.contains(r#""arguments":{"repo":"postil","text":"src/café notes.rs"}"#),
        "the repository is named, and the path arrives whole: {sent}"
    );
    assert!(
        sent.contains(r#""method":"notifications/initialized""#),
        "{sent}"
    );

    // Kept for the session: no second call.
    assert_eq!(get_json(&site, &uri)["state"], "ready");
    assert_eq!(calls(&bin).matches(r#""name":"recall""#).count(), 1);
    assert_eq!(servers(&bin).len(), 1);
}

#[test]
fn a_memory_is_read_with_its_history_and_refresh_drops_what_was_kept() {
    let repo = TempRepo::new("memory-detail");
    let bin = TempDir::new("memory-detail-bin");
    server(&bin, "hippius-mem 0.7.1", "");
    let site = server_site(&repo, &bin, |memory| memory);

    let reply = settle(&site, &Method::GET, &format!("{MEMORY}?id=mem_01A"));
    assert_eq!(
        reply,
        serde_json::json!({
            "state": "ready",
            "id": "mem_01A",
            "noteType": "gotcha",
            "author": "5CrAuthor",
            "created": 1,
            "updated": 2,
            "tags": [],
            "summary": "a.rs parses lazily",
            "body": "It *is* lazy.",
            "history": [
                { "kind": "Remember", "author": "5CrAuthor", "anchor": "none" },
                { "kind": "Edit", "author": "5CrAuthor", "anchor": "local" },
                { "kind": "Link", "author": "5CrAuthor", "anchor": "chain" },
            ],
        })
    );
    // No repository is named without an origin, as hippius-mem binds none.
    assert_eq!(recall(&site, "a.rs")["state"], "ready");
    assert!(calls(&bin).contains(r#""arguments":{"text":"a.rs"}"#));
    // An empty node is none: the same recall, kept. A node's label joins the path.
    assert_eq!(recall(&site, "a.rs&node=")["state"], "ready");
    assert_eq!(calls(&bin).matches(r#""name":"recall""#).count(), 1);
    assert_eq!(recall(&site, "a.rs&node=Note%20parser")["state"], "ready");
    assert!(calls(&bin).contains(r#""arguments":{"text":"a.rs Note parser"}"#));

    let (status, body) = post(&site, REFRESH, "");
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"state":"pending"}"#)
    );
    let refreshed = settle(&site, &Method::POST, REFRESH);
    assert_eq!(
        refreshed,
        serde_json::json!({ "state": "ready", "indexed": 3 })
    );
    // Read once: the next Refresh pulls again.
    assert_eq!(post(&site, REFRESH, "").1, r#"{"state":"pending"}"#);
    site.memory.wait_idle();
    assert_eq!(calls(&bin).matches(r#""name":"refresh""#).count(), 2);

    assert_eq!(recall(&site, "a.rs")["state"], "ready");
    assert_eq!(
        calls(&bin).matches(r#""name":"recall""#).count(),
        3,
        "the recall runs again after a refresh"
    );
}

#[test]
fn a_dead_memory_server_restarts_once_and_halts_on_a_second_death_until_rescan() {
    let repo = TempRepo::new("memory-dies");
    let bin = TempDir::new("memory-dies-bin");
    // Each server answers one recall and exits.
    server(
        &bin,
        "hippius-mem 0.7.1",
        &format!(
            "  *'\"name\":\"recall\"'*) text \"$id\" '{}'; exit 0 ;;",
            pointers()
        ),
    );
    let site = server_site(&repo, &bin, |memory| memory);

    assert_eq!(recall(&site, "a.rs")["state"], "ready");
    assert_eq!(recall(&site, "b.rs")["state"], "ready", "restarted at once");
    assert_eq!(servers(&bin).len(), 2);

    let reply = recall(&site, "c.rs");
    assert_eq!(reply["state"], "failed", "{reply}");
    let error = reply["error"].as_str().unwrap();
    assert!(
        error.contains("hippius-mem exited twice within 60 s"),
        "{error}"
    );
    assert_eq!(recall(&site, "d.rs"), reply, "not retried");
    assert_eq!(servers(&bin).len(), 2, "no third server");

    assert_eq!(post(&site, RESCAN, "").0, StatusCode::OK);
    // The rescan's check clears the halt when it ends.
    site.memory.wait();
    assert_eq!(recall(&site, "d.rs")["state"], "ready");
    assert_eq!(servers(&bin).len(), 3);
}

#[test]
fn deaths_further_apart_than_the_window_each_restart_the_server() {
    let repo = TempRepo::new("memory-dies-apart");
    let bin = TempDir::new("memory-dies-apart-bin");
    server(
        &bin,
        "hippius-mem 0.7.1",
        &format!(
            "  *'\"name\":\"recall\"'*) text \"$id\" '{}'; exit 0 ;;",
            pointers()
        ),
    );
    // No two deaths fall within an empty window.
    let site = server_site(&repo, &bin, |memory| {
        memory.with_restart_window(Duration::ZERO)
    });

    for path in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        assert_eq!(recall(&site, path)["state"], "ready", "{path}");
    }
    assert_eq!(servers(&bin).len(), 4);
}

#[test]
fn a_call_that_kills_every_server_is_reported_after_one_restart() {
    let repo = TempRepo::new("memory-dies-each-call");
    let bin = TempDir::new("memory-dies-each-call-bin");
    // Each server exits on a recall, with no answer.
    server(
        &bin,
        "hippius-mem 0.7.1",
        "  *'\"name\":\"recall\"'*) echo 'Error: index corrupt' >&2; exit 1 ;;",
    );
    // Deaths far apart never halt; the call still ends.
    let site = server_site(&repo, &bin, |memory| {
        memory.with_restart_window(Duration::ZERO)
    });

    assert_eq!(
        recall(&site, "a.rs"),
        serde_json::json!({
            "state": "failed",
            "error": "hippius-mem exited: Error: index corrupt",
        })
    );
    assert_eq!(servers(&bin).len(), 2, "one restart");
}

#[test]
fn a_server_that_exits_before_its_handshake_is_a_death_and_not_a_missing_configuration() {
    let repo = TempRepo::new("memory-dies-at-start");
    let bin = TempDir::new("memory-dies-at-start-bin");
    server(
        &bin,
        "hippius-mem 0.7.1",
        "  *'\"method\":\"initialize\"'*) echo 'Error: vault locked' >&2; exit 1 ;;",
    );
    let site = server_site(&repo, &bin, |memory| memory);

    assert_eq!(
        recall(&site, "a.rs"),
        serde_json::json!({
            "state": "failed",
            "error": "hippius-mem exited twice within 60 s, and is not started again until \
                      Rescan in Settings: Error: vault locked",
        })
    );
    assert_eq!(servers(&bin).len(), 2, "one restart");
}

#[test]
fn the_first_call_on_a_new_server_may_wait_out_its_warmup_and_later_ones_may_not() {
    let repo = TempRepo::new("memory-warmup");
    let bin = TempDir::new("memory-warmup-bin");
    // Every recall for a slow file takes 1.5 s, as a first recall waits for the warmup.
    server(
        &bin,
        "hippius-mem 0.7.1",
        &format!("  *slow*) /bin/sleep 1.5; text \"$id\" '{}' ;;", pointers()),
    );
    let site = server_site(&repo, &bin, |memory| {
        memory
            .with_call_timeout(Duration::from_secs(1))
            .with_first_call_timeout(Duration::from_secs(30))
    });

    assert_eq!(recall(&site, "slow-first.rs")["state"], "ready");
    assert_eq!(
        recall(&site, "slow-second.rs"),
        serde_json::json!({
            "state": "failed",
            "error": "hippius-mem did not answer within 1 s",
        })
    );
}

#[test]
fn a_tool_error_a_json_rpc_error_and_an_unreadable_result_are_each_reported() {
    let repo = TempRepo::new("memory-errors");
    let bin = TempDir::new("memory-errors-bin");
    let tool_error = serde_json::json!({
        "content": [{ "type": "text", "text": "note mem_01Z not found" }],
        "isError": true,
    });
    server(
        &bin,
        "hippius-mem 0.7.1",
        &format!(
            "  *tool-error*) reply \"$id\" '{tool_error}' ;;\n  \
             *rpc-error*) printf '{{\"jsonrpc\":\"2.0\",\"id\":%s,\"error\":\
             {{\"code\":-32602,\"message\":\"invalid params\"}}}}\\n' \"$id\" ;;\n  \
             *not-json*) text \"$id\" 'not JSON' ;;"
        ),
    );
    let site = server_site(&repo, &bin, |memory| memory);

    let error = |path: &str| {
        let reply = recall(&site, path);
        assert_eq!(reply["state"], "failed", "{path}: {reply}");
        reply["error"].as_str().unwrap().to_owned()
    };
    assert_eq!(error("tool-error.rs"), "note mem_01Z not found");
    assert_eq!(error("rpc-error.rs"), "invalid params");
    assert!(
        error("not-json.rs").starts_with("hippius-mem's recall: "),
        "names the tool"
    );
    // None of them is a death: the one server answers on.
    assert_eq!(recall(&site, "a.rs")["state"], "ready");
    assert_eq!(servers(&bin).len(), 1);
}

#[test]
fn a_start_that_fails_after_a_new_check_does_not_mark_the_server_unusable() {
    let repo = TempRepo::new("memory-check-race");
    let bin = TempDir::new("memory-check-race-bin");
    let entered = bin.path().join("entered");
    let gate = bin.path().join("gate");
    let first = bin.path().join("first");
    make_fifo(&entered);
    make_fifo(&gate);
    // The first server holds its handshake at the gate, then exits as one with no
    // configuration does; every later one answers.
    server(
        &bin,
        "hippius-mem 0.7.1",
        &format!(
            "  *'\"method\":\"initialize\"'*)\n    \
             if [ ! -f {first} ]; then\n      \
             /usr/bin/touch {first}; echo in > {entered}; /bin/cat {gate} > /dev/null\n      \
             echo 'Error: failed to load configuration' >&2; exit 1\n    \
             fi\n    \
             reply \"$id\" '{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}},\
             \"serverInfo\":{{\"name\":\"rmcp\",\"version\":\"3.9.0\"}}}}' ;;",
            first = first.display(),
            entered = entered.display(),
            gate = gate.display(),
        ),
    );
    let site = server_site(&repo, &bin, |memory| memory);
    site.memory.wait();

    assert_eq!(
        get_json(&site, &format!("{RECALL}?path=a.rs"))["state"],
        "pending"
    );
    read_fifo(&entered, WAIT);
    // The engineer set the configuration up meanwhile, and checks again.
    assert_eq!(post(&site, CHECK, "").0, StatusCode::OK);
    assert!(matches!(site.memory.wait(), State::Ready { .. }));
    fs::write(&gate, "go").unwrap();

    assert_eq!(recall(&site, "a.rs")["state"], "ready");
}

#[test]
fn closing_the_window_stops_a_server_whose_handshake_hangs() {
    let repo = TempRepo::new("memory-hung-start");
    let bin = TempDir::new("memory-hung-start-bin");
    let entered = bin.path().join("entered");
    make_fifo(&entered);
    server(
        &bin,
        "hippius-mem 0.7.1",
        &format!(
            "  *'\"method\":\"initialize\"'*) echo in > {}; /bin/sleep 60 & wait ;;",
            entered.display()
        ),
    );
    let site = server_site(&repo, &bin, |memory| memory);

    assert_eq!(
        get_json(&site, &format!("{RECALL}?path=a.rs"))["state"],
        "pending"
    );
    read_fifo(&entered, WAIT);
    let started = Instant::now();
    drop(site);
    wait_gone(&servers(&bin)[0]);
    // Far below the start's limit of 120 s: the close stopped it, not the limit.
    assert!(started.elapsed() < WAIT, "{:?}", started.elapsed());
}

#[test]
fn a_call_over_the_time_limit_is_reported_kills_the_hung_server_and_the_queue_moves_on() {
    let repo = TempRepo::new("memory-slow");
    let bin = TempDir::new("memory-slow-bin");
    let sleeper = bin.path().join("sleeper");
    // slow.rs hangs with a child of its own; fast.rs answers and its server exits after.
    server(
        &bin,
        "hippius-mem 0.7.1",
        &format!(
            "  *slow.rs*) /bin/sleep 30 & echo $! > {sleeper}; wait ;;\n  \
             *fast.rs*) text \"$id\" '{pointers}'; exit 0 ;;",
            sleeper = sleeper.display(),
            pointers = pointers(),
        ),
    );
    let site = server_site(&repo, &bin, |memory| {
        memory
            .with_call_timeout(Duration::from_secs(2))
            .with_first_call_timeout(Duration::from_secs(2))
    });

    let slow = format!("{RECALL}?path=slow.rs");
    let fast = format!("{RECALL}?path=fast.rs");
    assert_eq!(get_json(&site, &slow)["state"], "pending");
    assert_eq!(get_json(&site, &fast)["state"], "pending");
    site.memory.wait_idle();
    assert_eq!(
        get_json(&site, &slow),
        serde_json::json!({
            "state": "failed",
            "error": "hippius-mem did not answer within 2 s",
        })
    );
    assert_eq!(get_json(&site, &fast)["state"], "ready");
    let started = servers(&bin);
    assert_eq!(started.len(), 2, "a fresh server answered fast.rs");
    wait_gone(&started[0]);
    wait_gone(fs::read_to_string(&sleeper).unwrap().trim());

    // fast.rs's server exited: restarting it is the first death, so the timeout counted for
    // nothing.
    assert_eq!(recall(&site, "third.rs")["state"], "ready");
}

#[test]
fn a_server_missing_a_required_tool_reports_both_versions() {
    let repo = TempRepo::new("memory-old");
    let bin = TempDir::new("memory-old-bin");
    server(
        &bin,
        "hippius-mem 0.6.0",
        &format!(
            "  *'\"method\":\"tools/list\"'*) reply \"$id\" '{}' ;;",
            tool_list(&["recall", "get", "refresh"])
        ),
    );
    let site = server_site(&repo, &bin, |memory| memory);

    let reply = recall(&site, "a.rs");
    assert_eq!(
        reply,
        serde_json::json!({
            "state": "incompatible",
            "serverVersion": "0.6.0",
            "postilVersion": env!("CARGO_PKG_VERSION"),
            "missing": ["history"],
        })
    );
    assert_eq!(recall(&site, "b.rs"), reply, "not asked again");
    assert!(!calls(&bin).contains("tools/call"), "no tool is called");
    assert_eq!(servers(&bin).len(), 1);
}

#[test]
fn a_recall_without_a_usable_hippius_mem_says_why_until_a_new_check() {
    let repo = TempRepo::new("memory-unusable");
    let bin = TempDir::new("memory-unusable-bin");
    let site = server_site(&repo, &bin, |memory| memory);
    assert_eq!(
        recall(&site, "a.rs"),
        serde_json::json!({
            "state": "unavailable",
            "reason": "missing",
            "message": "hippius-mem is not installed on PATH",
        })
    );

    // Installed, with no configuration: its own words.
    let no_config = "Error: failed to load configuration; run hippius-mem quickstart";
    program(&bin, &format!("echo \"{no_config}\" >&2\nexit 1"));
    assert_eq!(
        recall(&site, "a.rs")["reason"],
        "missing",
        "until a new check"
    );
    assert_eq!(post(&site, CHECK, "").0, StatusCode::OK);
    site.memory.wait();
    assert_eq!(
        recall(&site, "a.rs"),
        serde_json::json!({
            "state": "unavailable",
            "reason": "unconfigured",
            "message": no_config,
        })
    );
}

#[test]
fn a_full_queue_drops_the_oldest_recall_for_a_closed_file_and_keeps_the_open_file() {
    let repo = TempRepo::new("memory-full");
    let bin = TempDir::new("memory-full-bin");
    let entered = bin.path().join("entered");
    let gate = bin.path().join("gate");
    make_fifo(&entered);
    make_fifo(&gate);
    server(
        &bin,
        "hippius-mem 0.7.1",
        &format!(
            "  *first.rs*) echo in > {entered}; /bin/cat {gate} > /dev/null; \
             text \"$id\" '{pointers}' ;;",
            entered = entered.display(),
            gate = gate.display(),
            pointers = pointers(),
        ),
    );
    let site = server_site(&repo, &bin, |memory| memory);
    let ask = |uri: String| assert_eq!(get_json(&site, &uri)["state"], "pending", "{uri}");

    ask(format!("{RECALL}?path=first.rs"));
    read_fifo(&entered, WAIT);
    // The engineer opens open.rs, then 31 more files, which fills the queue; then comes back
    // to open.rs, whose recall is the oldest queued.
    ask(format!("{RECALL}?path=open.rs"));
    for number in 1..=31 {
        ask(format!("{RECALL}?path=f{number}.rs"));
    }
    ask(format!("{RECALL}?path=open.rs"));
    // One more task: the oldest recall for a file that is not open goes.
    ask(format!("{MEMORY}?id=mem_01A"));
    fs::write(&gate, "go").unwrap();
    site.memory.wait_idle();

    let sent = calls(&bin);
    assert!(!sent.contains(r#""text":"f1.rs""#), "f1.rs was dropped");
    for kept in ["open.rs", "f2.rs", "f31.rs"] {
        assert!(
            sent.contains(&format!(r#""text":"{kept}""#)),
            "{kept}: {sent}"
        );
    }
    assert!(sent.contains(r#""name":"get""#));
    assert_eq!(
        sent.matches(r#""text":"open.rs""#).count(),
        1,
        "queued once"
    );
    assert_eq!(recall(&site, "open.rs")["state"], "ready");
}

#[test]
fn memory_ids_and_paths_are_checked_before_use() {
    let repo = TempRepo::new("memory-ids");
    let bin = TempDir::new("memory-ids-bin");
    server(&bin, "hippius-mem 0.7.1", "");
    let site = server_site(&repo, &bin, |memory| memory);

    for query in [
        "?id=mem_lowercase",
        "?id=mem_",
        "?id=..%2Fmem_01A",
        "?id=N1",
        "?id=mem_01A%0A",
        "?id=",
        "",
    ] {
        let (status, body) = get(&site, &format!("{MEMORY}{query}"));
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
    for query in ["?path=..%2Fsecret", "?path=%2Fetc%2Fpasswd", "?path=", ""] {
        let (status, body) = get(&site, &format!("{RECALL}{query}"));
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
    assert!(servers(&bin).is_empty(), "no server started");
}

#[test]
fn switching_the_project_or_closing_the_window_stops_the_memory_server() {
    let repo = TempRepo::new("memory-stop");
    let other = TempRepo::new("memory-stop-other");
    let bin = TempDir::new("memory-stop-bin");
    server(&bin, "hippius-mem 0.7.1", "");
    let site = server_site(&repo, &bin, |memory| memory);

    assert_eq!(recall(&site, "a.rs")["state"], "ready");
    site.open(Repo::open(&other.root).unwrap(), LiveRun::Stop)
        .unwrap();
    wait_gone(&servers(&bin)[0]);

    // The other project's recalls are its own, from a server started there.
    assert_eq!(recall(&site, "a.rs")["state"], "ready");
    let started = servers(&bin);
    assert_eq!(started.len(), 2);
    drop(site);
    wait_gone(&started[1]);
}

#[test]
fn a_run_s_recall_is_for_its_notes_and_the_files_it_edited() {
    let repo = TempRepo::new("memory-run");
    repo.write("a.txt", "a\n");
    let bin = TempDir::new("memory-run-bin");
    server(&bin, "hippius-mem 0.7.1", "");
    let (site, nudges) = run_site(&repo, "checks");
    let path = bin.path().as_os_str().to_owned();
    let site = site.with_memory(Memory::new(Scanner::new(move || path.clone())));
    post_note(&site, "a.txt", 1, 1, "check it");
    let (status, body) = send(&site, &["N1"]);
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_for(&site, &nudges, "ended");

    let reply = settle(&site, &Method::GET, &format!("{RECALL}?run=R1"));
    assert_eq!(reply["state"], "ready", "{reply}");
    let sent = calls(&bin);
    let text = sent
        .lines()
        .find(|line| line.contains(r#""name":"recall""#))
        .unwrap();
    assert!(
        text.contains(r#""text":"a.txt\n\n"#),
        "the edited file first: {text}"
    );
    assert!(text.contains("check it"), "the note: {text}");
    assert!(
        !text.contains("These are review notes"),
        "without the words every prompt starts with: {text}"
    );

    let (status, body) = get(&site, &format!("{RECALL}?run=R9"));
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[test]
fn the_real_hippius_mem_recalls_by_path_and_reads_a_memory_and_its_history() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    if agents::find_program(memory::PROGRAM, &path).is_none() {
        eprintln!("skipped: hippius-mem is not installed on PATH");
        return;
    }
    let repo = TempRepo::new("memory-real-server");
    // hippius-mem names the repository from its origin: memory-real.
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/team/memory-real.git",
    ]);
    let home = TempDir::new("memory-real-server-home");
    let model_cache = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache/hippius-mem/fastembed"))
        .filter(|cache| cache.is_dir());
    let scanner = Scanner::new(move || path.clone());
    // A first start loads the embedding model: allow more than a call has in the app.
    let memory = Memory::new(scanner.clone())
        .with_home(home.path(), model_cache.as_deref())
        .with_call_timeout(Duration::from_secs(120));
    let site = Site::new(Some(&repo.config()), None, None, scanner, quiet()).with_memory(memory);
    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();
    site.memory.wait();
    assert_eq!(post(&site, SETUP, "").0, StatusCode::OK);
    let view = settled(&site);
    let error = view["setup"]["error"].as_str().unwrap_or("");
    if error.contains("embedding model") {
        eprintln!("skipped: hippius-mem could not get its embedding model (no network?): {error}");
        return;
    }
    assert_eq!(view["state"]["kind"], "ready", "{view}");

    // hippius-mem 0.7.1 has no remember command: the notes go in through its server, before
    // Postil's starts.
    let remember = |summary: &str, body: &str| {
        let note = serde_json::json!({
            "note_type": "gotcha",
            "summary": summary,
            "body": body,
            "repo": "memory-real",
        });
        site.memory.remember(&repo.root, &note)
    };
    let body = "# Lazy\n\nThe *parser* in `src/café notes.rs` waits for the first read.";
    let id = remember(
        "src/café notes.rs parses its notes lazily, on first use",
        body,
    );
    remember(
        "the build script regenerates icons.svg from the assets folder",
        "Run it after adding an icon.",
    );
    remember(
        "release tags are signed with the team's key",
        "Use git tag -s.",
    );

    let refreshed = settle(&site, &Method::POST, REFRESH);
    assert_eq!(refreshed["state"], "ready", "{refreshed}");

    let reply = recall(&site, "src%2Fcaf%C3%A9%20notes.rs");
    assert_eq!(reply["state"], "ready", "{reply}");
    let pointers = reply["pointers"].as_array().unwrap();
    assert!(
        pointers.iter().any(|pointer| pointer["id"] == id.as_str()),
        "{reply}"
    );

    let detail = settle(&site, &Method::GET, &format!("{MEMORY}?id={id}"));
    assert_eq!(detail["state"], "ready", "{detail}");
    assert_eq!(detail["body"], body);
    let history = detail["history"].as_array().unwrap();
    assert_eq!(history[0]["kind"], "Remember", "{detail}");
    assert_eq!(history[0]["author"], detail["author"]);
    assert!(
        ["none", "local", "chain"].contains(&history[0]["anchor"].as_str().unwrap()),
        "{detail}"
    );
}

// ---- Agents get it --------------------------------------------------------------------------

/// What [`memory_runs`] sets up besides the site, kept until the test ends.
struct Agents {
    /// The fake hippius-mem, in a folder whose name has a space.
    program: PathBuf,
    /// Where it logs ([`server_script`]), and `configs`, a line per start: its arguments and
    /// the configuration file it was told to read, split by `|`.
    log: TempDir,
    /// The home whose configuration file the program is told to read.
    home: TempDir,
    _dir: TempDir,
}

impl Agents {
    /// The server a session gets: the program by its absolute path, spaces and all.
    fn server(&self) -> serde_json::Value {
        serde_json::json!({
            "name": "hippius-mem",
            "command": self.program,
            "args": [],
            "env": [
                { "name": "PATH", "value": self.program.parent().unwrap() },
                { "name": "NO_COLOR", "value": "1" },
                { "name": "HIPPIUS_MEM_CONFIG", "value": self.config() },
            ],
        })
    }

    fn config(&self) -> PathBuf {
        self.home
            .path()
            .join(".config/hippius-mem/hippius-mem.toml")
    }

    /// The fake server's script ([`server_script`]).
    fn serving(&self) -> String {
        server_script(self.log.path(), "hippius-mem 0.7.1", "")
    }

    /// Writes the program, running `body` after it logs its start.
    fn write(&self, body: &str) {
        let script = format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$*\" \"$HIPPIUS_MEM_CONFIG\" >> {}/configs\n{body}\n",
            self.log.path().display()
        );
        fs::write(&self.program, script).unwrap();
        fs::set_permissions(&self.program, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A site on `repo` whose agent, and only reviewer, is the fake agent in `scenario`, and whose
/// hippius-mem is a fake that passes doctor and serves MCP, Ready, with team memory `memory`.
fn memory_runs(
    repo: &TempRepo,
    scenario: &str,
    memory: TeamMemory,
) -> (Site, mpsc::Receiver<()>, Agents) {
    // Named after the repository, which each test names apart.
    let name = repo
        .root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let dir = TempDir::new(&format!("{name}-tools"));
    let tools = dir.path().join("team tools");
    fs::create_dir_all(&tools).unwrap();
    let agents = Agents {
        program: tools.join(memory::PROGRAM),
        log: TempDir::new(&format!("{name}-log")),
        home: TempDir::new(&format!("{name}-home")),
        _dir: dir,
    };
    agents.write(&agents.serving());
    let (site, nudges) = run_site(repo, scenario);
    let search_path = tools.into_os_string();
    let scanner = Scanner::new(move || search_path.clone());
    let site = site.with_memory(Memory::new(scanner).with_home(agents.home.path(), None));
    assert!(matches!(site.memory.wait(), State::Ready { .. }));
    set_memory_agent(&site, scenario, memory);
    (site, nudges, agents)
}

/// Saves the fake agent in `scenario` as the agent and the only reviewer, with team memory
/// `memory`.
fn set_memory_agent(site: &Site, scenario: &str, memory: TeamMemory) {
    let settings = serde_json::json!({
        "customAgents": [{ "name": "Fake", "command": fake_agent(scenario) }],
        "enabledAgents": ["Fake"],
        "agent": { "name": "Fake" },
        "reviewers": [{ "name": "Fake" }],
        "agentMemory": memory == TeamMemory::On,
    });
    let (status, body) = save(site, &settings.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The text of run `run`'s first message that starts with `start`, without it.
fn said<'a>(run: &'a serde_json::Value, start: &str) -> &'a str {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["text"].as_str())
        .find_map(|text| text.strip_prefix(start))
        .unwrap_or_else(|| panic!("no message {start:?} in {run}"))
}

/// The `session/new` params the fake agent of `run` received, in order.
fn sessions(run: &serde_json::Value) -> Vec<serde_json::Value> {
    serde_json::from_str(said(run, "sessions ")).unwrap()
}

/// The MCP servers of the one session the fake agent of `run` opened.
fn servers_of(run: &serde_json::Value) -> serde_json::Value {
    let sessions = sessions(run);
    assert_eq!(sessions.len(), 1, "{run}");
    sessions[0]["mcpServers"].clone()
}

/// Waits until run `id` has ended or failed, and returns it.
fn run_ended(site: &Site, nudges: &mpsc::Receiver<()>, id: &str) -> serde_json::Value {
    loop {
        let run = get_json(site, &format!("{RUN}?id={id}"));
        if ["ended", "failed"].contains(&run["status"].as_str().unwrap_or("")) {
            return run;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("no change while waiting for {id}: {run}"));
    }
}

/// Sends note `note` and returns its run once it ended.
fn sent(site: &Site, nudges: &mpsc::Receiver<()>, note: &str) -> serde_json::Value {
    let (status, body) = send(site, &[note]);
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = get_json(site, RUN)["id"].as_str().unwrap().to_owned();
    run_ended(site, nudges, &id)
}

#[test]
fn a_run_gives_the_agent_team_memory() {
    let repo = TempRepo::new("memory-agents");
    repo.write("a.txt", "a\n");
    repo.write("src/a.rs", "fn a() {\n    one();\n}\n");
    repo.write("src/b.rs", "// b\n\nfn two() {\n}\n");
    let (site, nudges, agents) = memory_runs(&repo, "echo-session", TeamMemory::On);
    post_note(&site, "a.txt", 1, 1, "check it");

    let preview = get_json(&site, &format!("{RUN}/preview?notes=N1"));
    let prompt = preview["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with(&format!("{}{}", notes::INTRO, notes::TEAM_MEMORY)),
        "the line right after the introduction: {prompt}"
    );
    assert!(
        !prompt.to_lowercase().contains("remember"),
        "remembering stays with the engineer: {prompt}"
    );

    let run = sent(&site, &nudges, "N1");
    assert_eq!(run["status"], "ended", "{run}");
    assert_eq!(
        run["prompt"], preview["prompt"],
        "the preview is the prompt sent"
    );
    assert!(said(&run, "got ").contains(notes::TEAM_MEMORY), "{run}");
    let sessions = sessions(&run);
    assert_eq!(sessions.len(), 1, "{run}");
    let keys: Vec<&String> = sessions[0].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["cwd", "mcpServers"], "{run}");
    assert_eq!(
        sessions[0]["mcpServers"],
        serde_json::json!([agents.server()])
    );
    assert_eq!(run["memoryNotice"], serde_json::Value::Null, "{run}");

    // The run dialog's recall leaves the line out, as it leaves out the introduction.
    let reply = settle(&site, &Method::GET, &format!("{RECALL}?run=R1"));
    assert_eq!(reply["state"], "ready", "{reply}");
    let recalled = calls(&agents.log);
    let recalled = recalled
        .lines()
        .find(|line| line.contains(r#""name":"recall""#))
        .unwrap();
    assert!(recalled.contains("check it"), "{recalled}");
    assert!(!recalled.contains("Recall team memory"), "{recalled}");

    // Every hippius-mem Postil started, doctor and its own server, read the engineer's file.
    let config = agents.config();
    let starts = fs::read_to_string(agents.log.path().join("configs")).unwrap();
    for start in starts.lines() {
        assert!(
            start.ends_with(&format!("|{}", config.display())),
            "{starts}"
        );
    }
    assert!(
        starts
            .lines()
            .any(|start| start == format!("doctor|{}", config.display()))
    );
    assert!(
        starts
            .lines()
            .any(|start| start == format!("|{}", config.display()))
    );
}

#[test]
fn readers_reviewers_and_fresh_follow_ups_get_it_too() {
    let repo = TempRepo::new("memory-agents-others");
    repo.write("a.txt", "a\n");
    repo.write("src/a.rs", "fn a() {\n    one();\n}\n");
    repo.write("src/b.rs", "// b\n\nfn two() {\n}\n");
    let (site, nudges, agents) = memory_runs(&repo, "echo-session", TeamMemory::On);
    post_note(&site, "a.txt", 1, 1, "check it");
    sent(&site, &nudges, "N1");

    // A read-only run gets it too: recall is a read. An ask:
    let ask = serde_json::json!({
        "note": {
            "path": "a.txt",
            "lines": { "start": 1, "end": 1 },
            "snippet": "a",
            "body": "Why a?",
        },
    });
    let (status, body) = post(&site, &format!("{RUN}/ask"), &ask.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
    let asked = run_ended(&site, &nudges, "R2");
    assert_eq!(servers_of(&asked), serde_json::json!([agents.server()]));

    // An analysis:
    let (status, body) = post(
        &site,
        "postil://localhost/api/analysis",
        r#"{"kind": "analyze"}"#,
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    let analysis = run_ended(&site, &nudges, "R3");
    assert_eq!(servers_of(&analysis), serde_json::json!([agents.server()]));

    // A reviewer, whose quote of the author's prompt leaves the line out:
    let of = serde_json::json!({ "of": "R1" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/review"), &of);
    assert_eq!(status, StatusCode::OK, "{body}");
    let reviewer = run_ended(&site, &nudges, "R4");
    assert_eq!(servers_of(&reviewer), serde_json::json!([agents.server()]));
    // Up to the author's reply, in which the fake author echoes its whole prompt.
    let (quoted, _) = reviewer["prompt"]
        .as_str()
        .unwrap()
        .split_once("The agent's reply:")
        .unwrap();
    assert!(quoted.contains("check it"), "{quoted}");
    assert!(!quoted.contains(notes::TEAM_MEMORY), "{quoted}");

    // A fresh agent's follow-up (the review took the thread's kept agent), whose quote of the
    // first prompt leaves it out too.
    let follow = serde_json::json!({ "of": "R1", "text": "And b?" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/follow"), &follow);
    assert_eq!(status, StatusCode::OK, "{body}");
    let followed = run_ended(&site, &nudges, "R5");
    assert_eq!(servers_of(&followed), serde_json::json!([agents.server()]));
    let (fresh, _) = followed["prompt"]
        .as_str()
        .unwrap()
        .split_once("It answered:")
        .unwrap();
    assert!(fresh.starts_with("You are continuing work"), "{fresh}");
    assert!(fresh.contains("check it"), "{fresh}");
    assert!(!fresh.contains(notes::TEAM_MEMORY), "{fresh}");
}

#[test]
fn the_setting_off_gives_none_and_no_prompt_line() {
    let repo = TempRepo::new("memory-agents-off");
    repo.write("a.txt", "a\n");
    let (site, nudges, _agents) = memory_runs(&repo, "echo-session", TeamMemory::Off);
    post_note(&site, "a.txt", 1, 1, "check it");

    let preview = get_json(&site, &format!("{RUN}/preview?notes=N1"));
    let prompt = preview["prompt"].as_str().unwrap();
    assert!(!prompt.contains(notes::TEAM_MEMORY), "{prompt}");

    let run = sent(&site, &nudges, "N1");
    assert_eq!(servers_of(&run), serde_json::json!([]));
    assert!(!said(&run, "got ").contains(notes::TEAM_MEMORY), "{run}");
    // Off is the engineer's choice: nothing to say about it.
    assert_eq!(run["memoryNotice"], serde_json::Value::Null, "{run}");
}

#[test]
fn a_refused_server_list_is_retried_without() {
    let repo = TempRepo::new("memory-agents-refused");
    repo.write("a.txt", "a\n");
    let (site, nudges, agents) = memory_runs(&repo, "refuse-servers", TeamMemory::On);
    post_note(&site, "a.txt", 1, 1, "check it");
    post_note(&site, "a.txt", 1, 1, "and again");

    let run = sent(&site, &nudges, "N1");
    assert_eq!(run["status"], "ended", "the run goes on without: {run}");
    let sessions = sessions(&run);
    assert_eq!(sessions.len(), 2, "one retry: {run}");
    assert_eq!(
        sessions[0]["mcpServers"],
        serde_json::json!([agents.server()])
    );
    assert_eq!(sessions[1]["mcpServers"], serde_json::json!([]));
    assert_eq!(
        run["memoryNotice"],
        "team memory was not given: the agent refused session/new with its MCP servers: MCP \
         servers are disabled (-32602)"
    );
    // The agent without memory is not told to recall it. The run's record keeps the prompt as
    // it was built when the run was sent.
    assert!(!said(&run, "got ").contains(notes::TEAM_MEMORY), "{run}");
    let recorded = run["prompt"].as_str().unwrap();
    assert!(recorded.contains(notes::TEAM_MEMORY), "{recorded}");

    // Refused again without servers: the run fails with the agent's refusal.
    set_memory_agent(&site, "refuse-sessions", TeamMemory::On);
    let run = sent(&site, &nudges, "N2");
    assert_eq!(run["status"], "failed", "{run}");
    let failed = run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "failed")
        .unwrap();
    assert_eq!(
        failed["error"], "the agent refused session/new: no sessions today (-32602)",
        "{run}"
    );
}

#[test]
fn unready_or_removed_memory_starts_runs_without_it_and_warns_once() {
    let repo = TempRepo::new("memory-agents-unready");
    repo.write("a.txt", "a\n");
    let (site, nudges, agents) = memory_runs(&repo, "echo-session", TeamMemory::On);
    for body in ["one", "two", "three", "four", "five"] {
        post_note(&site, "a.txt", 1, 1, body);
    }
    let not_given = |run: &serde_json::Value| {
        assert_eq!(servers_of(run), serde_json::json!([]));
        assert!(!said(run, "got ").contains(notes::TEAM_MEMORY), "{run}");
        let recorded = run["prompt"].as_str().unwrap();
        assert!(!recorded.contains(notes::TEAM_MEMORY), "{recorded}");
    };

    // Not set up: the first run says so, the next one does not.
    agents.write("echo 'Error: failed to load configuration' >&2\nexit 1");
    assert_eq!(post(&site, CHECK, "").0, StatusCode::OK);
    assert!(matches!(site.memory.wait(), State::Unconfigured { .. }));
    let run = sent(&site, &nudges, "N1");
    not_given(&run);
    assert_eq!(
        run["memoryNotice"],
        "team memory was not given: Error: failed to load configuration"
    );
    let run = sent(&site, &nudges, "N2");
    not_given(&run);
    assert_eq!(
        run["memoryNotice"],
        serde_json::Value::Null,
        "said once: {run}"
    );

    // Ready again, then removed while Postil runs: said again, once.
    agents.write(&agents.serving());
    assert_eq!(post(&site, CHECK, "").0, StatusCode::OK);
    assert!(matches!(site.memory.wait(), State::Ready { .. }));
    let run = sent(&site, &nudges, "N3");
    assert_eq!(servers_of(&run), serde_json::json!([agents.server()]));
    fs::remove_file(&agents.program).unwrap();
    let run = sent(&site, &nudges, "N4");
    not_given(&run);
    assert_eq!(
        run["memoryNotice"],
        "team memory was not given: hippius-mem is not installed on PATH"
    );
    let run = sent(&site, &nudges, "N5");
    not_given(&run);
    assert_eq!(
        run["memoryNotice"],
        serde_json::Value::Null,
        "said once: {run}"
    );
}

/// The fake server whose `doctor` and `quickstart` wait until the file `gate` exists.
fn gated(agents: &Agents, gate: &Path) -> String {
    format!(
        "case \"$*\" in\ndoctor|'quickstart --no-wire') while [ ! -e {} ]; do /bin/sleep 0.05; \
         done ;;\nesac\n{}",
        gate.display(),
        agents.serving()
    )
}

#[test]
fn a_run_sent_while_a_check_runs_gets_the_line_with_the_server() {
    let repo = TempRepo::new("memory-agents-race");
    repo.write("a.txt", "a\n");
    let (site, nudges, agents) = memory_runs(&repo, "echo-session", TeamMemory::On);
    post_note(&site, "a.txt", 1, 1, "check it");
    let gate = agents.log.path().join("gate");
    agents.write(&gated(&agents, &gate));
    assert_eq!(post(&site, CHECK, "").0, StatusCode::OK);

    // Not Ready while doctor runs: the preview, and the prompt as built, have no line.
    let preview = get_json(&site, &format!("{RUN}/preview?notes=N1"));
    assert!(
        !preview["prompt"]
            .as_str()
            .unwrap()
            .contains(notes::TEAM_MEMORY)
    );
    let (status, body) = send(&site, &["N1"]);
    assert_eq!(status, StatusCode::OK, "{body}");
    // The start waits for the check, which then finds it Ready.
    fs::write(&gate, "").unwrap();
    let id = get_json(&site, RUN)["id"].as_str().unwrap().to_owned();
    let run = run_ended(&site, &nudges, &id);
    assert_eq!(servers_of(&run), serde_json::json!([agents.server()]));
    assert!(
        said(&run, "got ").contains(&format!("{}{}", notes::INTRO, notes::TEAM_MEMORY)),
        "the agent that has the server is told of it: {run}"
    );
    assert_eq!(run["memoryNotice"], serde_json::Value::Null, "{run}");
}

#[test]
fn a_run_does_not_wait_for_a_set_up() {
    let repo = TempRepo::new("memory-agents-setup");
    repo.write("a.txt", "a\n");
    let (site, nudges, agents) = memory_runs(&repo, "echo-session", TeamMemory::On);
    post_note(&site, "a.txt", 1, 1, "check it");
    let gate = agents.log.path().join("gate");
    agents.write(&gated(&agents, &gate));
    // A check runs, then a set up, whose quickstart may take minutes and drops the check.
    assert_eq!(post(&site, CHECK, "").0, StatusCode::OK);
    assert_eq!(post(&site, SETUP, "").0, StatusCode::OK);

    let started = Instant::now();
    let run = sent(&site, &nudges, "N1");
    // Far below the doctor's limit, 10 s, which bounds every other wait.
    assert!(started.elapsed() < Duration::from_secs(5), "{run}");
    assert_eq!(servers_of(&run), serde_json::json!([]));
    assert_eq!(
        run["memoryNotice"],
        "team memory was not given: hippius-mem is still being checked"
    );
    fs::write(&gate, "").unwrap();
    site.memory.wait();
}

#[test]
fn an_agent_that_asks_for_a_sign_in_is_not_asked_again_without_servers() {
    let repo = TempRepo::new("memory-agents-auth");
    repo.write("a.txt", "a\n");
    let (site, nudges, _agents) = memory_runs(&repo, "auth", TeamMemory::On);
    post_note(&site, "a.txt", 1, 1, "check it");

    let run = sent(&site, &nudges, "N1");
    assert_eq!(run["status"], "failed", "{run}");
    let failed = run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "failed")
        .unwrap();
    let error = failed["error"].as_str().unwrap();
    assert!(error.contains("needs you to sign in"), "{error}");
    assert!(error.ends_with("(session/new 1)"), "asked once: {error}");
}

#[test]
fn a_repository_s_configuration_file_does_not_override_the_engineer_s() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    if agents::find_program(memory::PROGRAM, &path).is_none() {
        eprintln!("skipped: hippius-mem is not installed on PATH");
        return;
    }
    let repo = TempRepo::new("memory-real-config");
    // hippius-mem reads a hippius-mem.toml in its working directory first, unless told which
    // file to read; one with a key it does not know fails every command that reads it.
    repo.write("hippius-mem.toml", "chosen_by_the_repository = true\n");
    let home = TempDir::new("memory-real-config-home");
    let model_cache = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache/hippius-mem/fastembed"))
        .filter(|cache| cache.is_dir());
    let scanner = Scanner::new(move || path.clone());
    let memory = Memory::new(scanner.clone()).with_home(home.path(), model_cache.as_deref());
    let site = Site::new(Some(&repo.config()), None, None, scanner, quiet()).with_memory(memory);
    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();

    // doctor runs in the project's root, and finds no configuration of the engineer's.
    let State::Unconfigured { problem, .. } = site.memory.wait() else {
        panic!("a fresh home has no configuration");
    };
    let problem = problem.unwrap_or_default();
    assert!(
        problem.contains("failed to load configuration"),
        "{problem}"
    );
    assert!(!problem.contains("chosen_by_the_repository"), "{problem}");

    assert_eq!(post(&site, SETUP, "").0, StatusCode::OK);
    let view = settled(&site);
    let error = view["setup"]["error"].as_str().unwrap_or("");
    if error.contains("embedding model") {
        eprintln!("skipped: hippius-mem could not get its embedding model (no network?): {error}");
        return;
    }
    assert_eq!(view["state"]["kind"], "ready", "{view}");
    let written = home.path().join(".config/hippius-mem/hippius-mem.toml");
    assert!(
        written.is_file(),
        "set up wrote the engineer's file: {view}"
    );

    // Postil's own server, in the project's root, reads the engineer's file too.
    let reply = recall(&site, "a.txt");
    assert_eq!(reply["state"], "ready", "{reply}");
}
