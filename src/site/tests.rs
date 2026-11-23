//! Tests of the page's API ([`Site`]), through requests as the page sends them, one file per
//! area. The helpers here are shared by the areas and by the window's tests in `shell.rs`.
//! The `notes` and `settings` areas hide `crate::notes` and `crate::settings` from
//! `use super::*`: an area that needs those modules imports them by path.

mod files;
mod notes;
mod page_e2e;
mod projects;
mod runs;
mod settings;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use super::*;

// The page's files, as the tests that check its links and colours read them.
const INDEX_HTML: &str = include_str!("../../ui/index.html");
const DOM_JS: &str = include_str!("../../ui/dom.js");
pub(crate) const APP_CSS: &str = include_str!("../../ui/app.css");

/// A real git working tree in the temporary directory, removed on drop.
pub(crate) struct TempRepo {
    pub(crate) root: PathBuf,
}

impl TempRepo {
    /// `name` must differ between tests: they run in parallel in one process.
    pub(crate) fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("postil-{}-{name}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        let repo = Self { root };
        repo.git(&["init", "--quiet", "--initial-branch", "trunk"]);
        repo
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    pub(crate) fn write(&self, file: &str, text: &str) {
        fs::write(self.root.join(file), text).unwrap();
    }

    /// A site with this repository open.
    fn site(&self) -> Site {
        let site = self.empty_site();
        site.open(Repo::open(&self.root).unwrap(), LiveRun::Stop)
            .unwrap();
        site
    }

    /// A site with no project open, its configuration in [`Self::config`] and no agents
    /// installed.
    fn empty_site(&self) -> Site {
        Site::new(Some(&self.config()), None, no_agents(), quiet())
    }

    /// The configuration directory of this repository's sites: inside its git directory,
    /// where no file listing sees it and the drop below removes it.
    fn config(&self) -> PathBuf {
        self.root.join(".git/test-config")
    }
}

/// A page that is never told about runs.
fn quiet() -> Notify {
    Arc::new(|| {})
}

/// A scanner whose search path is empty: every agent is missing.
fn no_agents() -> Scanner {
    Scanner::new(OsString::new)
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        // Best effort: a leftover directory in the temporary directory fails no test.
        drop(fs::remove_dir_all(&self.root));
    }
}

fn get(site: &Site, uri: &str) -> (StatusCode, String) {
    let request = Request::builder().uri(uri).body(Vec::new()).unwrap();
    let reply = site.respond(&request);
    let body = String::from_utf8(reply.body().to_vec()).unwrap();
    (reply.status(), body)
}

fn get_json(site: &Site, uri: &str) -> serde_json::Value {
    let (status, body) = get(site, uri);
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    serde_json::from_str(&body).unwrap()
}

const NOTES: &str = "postil://localhost/api/notes";

fn post(site: &Site, uri: &str, body: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(body.as_bytes().to_vec())
        .unwrap();
    let reply = site.respond(&request);
    let body = String::from_utf8(reply.body().to_vec()).unwrap();
    (reply.status(), body)
}

pub(crate) fn post_note(
    site: &Site,
    path: &str,
    start: usize,
    end: usize,
    body: &str,
) -> serde_json::Value {
    let text = site.repo().unwrap().read(path).unwrap();
    let shown: Vec<&str> = text.lines().collect();
    let request = new_note(path, start, end, &shown[start - 1..end].join("\n"), body);
    let (status, reply) = post(site, NOTES, &request);
    assert_eq!(status, StatusCode::OK, "{reply}");
    serde_json::from_str(&reply).unwrap()
}

/// A `POST /api/notes` body, as the page builds it.
fn new_note(path: &str, start: usize, end: usize, snippet: &str, body: &str) -> String {
    serde_json::json!({
        "path": path,
        "lines": { "start": start, "end": end },
        "snippet": snippet,
        "body": body,
    })
    .to_string()
}

fn delete(site: &Site, uri: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(Method::DELETE)
        .uri(uri)
        .body(Vec::new())
        .unwrap();
    let reply = site.respond(&request);
    let body = String::from_utf8(reply.body().to_vec()).unwrap();
    (reply.status(), body)
}

/// A plain folder in the temporary directory, outside any repository.
pub(crate) fn plain_folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("postil-{}-plain-{name}", std::process::id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn open_request(path: &Path) -> String {
    serde_json::json!({ "path": path }).to_string()
}

/// The root as git reports it, which resolves symbolic links such as /var to
/// /private/var on macOS.
fn site_root(dir: &Path) -> String {
    Repo::open(dir).unwrap().root().to_str().unwrap().to_owned()
}

const SETTINGS: &str = "postil://localhost/api/settings";

fn put(site: &Site, uri: &str, body: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .body(body.as_bytes().to_vec())
        .unwrap();
    let reply = site.respond(&request);
    let body = String::from_utf8(reply.body().to_vec()).unwrap();
    (reply.status(), body)
}

/// Saves `settings` (JSON text, possibly malformed) as the page does: naming the
/// revision of the file it last read.
fn save(site: &Site, settings: &str) -> (StatusCode, String) {
    let revision = get_json(site, SETTINGS)["revision"].clone();
    put(
        site,
        SETTINGS,
        &format!("{{\"revision\": {revision}, \"settings\": {settings}}}"),
    )
}

const RUN: &str = "postil://localhost/api/run";
const RUNS: &str = "postil://localhost/api/runs";

/// Long enough for Node to start on a loaded machine; a test that waits this long has
/// failed (it bounds a hang and times nothing).
const WAIT: Duration = Duration::from_secs(20);

/// The command that starts the fake agent in `scenario`.
fn fake_agent(scenario: &str) -> String {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-agent.mjs");
    format!("node {} {scenario}", script.display())
}

/// A site on `repo` whose agent is the fake agent in `scenario`, called Fake, with this
/// process's PATH (to find `node`). The receiver gets one message per page nudge.
pub(crate) fn run_site(repo: &TempRepo, scenario: &str) -> (Site, mpsc::Receiver<()>) {
    let (sender, nudges) = mpsc::channel();
    // The receiver is gone once the test finished.
    let notify: Notify = Arc::new(move || sender.send(()).unwrap_or_default());
    (agent_site(repo, scenario, notify), nudges)
}

/// A site on `repo` whose agent is the fake agent in `scenario`, called Fake, with this
/// process's PATH (to find `node`), telling `notify` when a run changes.
fn agent_site(repo: &TempRepo, scenario: &str, notify: Notify) -> Site {
    let path = std::env::var_os("PATH").expect("PATH is set");
    let site = Site::new(
        Some(&repo.config()),
        None,
        Scanner::new(move || path.clone()),
        notify,
    );
    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();
    set_agents(&site, &[("Fake", &fake_agent(scenario))], "Fake");
    site
}

/// Saves custom agents `agents`, all turned on, with `chosen` as the agent.
fn set_agents(site: &Site, agents: &[(&str, &str)], chosen: &str) {
    let custom: Vec<_> = agents
        .iter()
        .map(|(name, command)| serde_json::json!({ "name": name, "command": command }))
        .collect();
    let names: Vec<_> = agents.iter().map(|(name, _)| *name).collect();
    let settings = serde_json::json!({
        "customAgents": custom,
        "enabledAgents": names,
        "agent": chosen,
    });
    let (status, body) = save(site, &settings.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Waits, nudge by nudge, until the run's status is `status`, and returns the run.
pub(crate) fn wait_for(
    site: &Site,
    nudges: &mpsc::Receiver<()>,
    status: &str,
) -> serde_json::Value {
    loop {
        let run = get_json(site, RUN);
        if run["status"] == status {
            return run;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("no change while waiting for {status}: {run}"));
    }
}

pub(crate) fn send(site: &Site, notes: &[&str]) -> (StatusCode, String) {
    post(
        site,
        RUN,
        &serde_json::json!({ "notes": notes }).to_string(),
    )
}

fn states(site: &Site) -> Vec<serde_json::Value> {
    get_json(site, NOTES)["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["state"].clone())
        .collect()
}
