//! The API the page calls: [`Site`], which answers each request on the `postil://` scheme
//! with the page's own files or JSON, and the request and reply types of its routes.
//!
//! `shell.rs` owns the window and hands every request to the one thread that owns the
//! `Site` (`spawn_site_worker`); this module never touches the window.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::SystemTime;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wry::http::header::CONTENT_TYPE;
use wry::http::{Method, Request, Response, StatusCode};

use crate::agents::{self, BUILT_IN, Scanner};
use crate::analysis::{self, AnalysisJob, AnalyzeRequest, UpdateRequest, Verdict};
use crate::definitions::Definitions;
use crate::error::{Error, Result, error_chain};
use crate::files::config_dir;
use crate::folders::{self, Folders};
use crate::highlight::{self, HighlightedLines, Highlighter};
use crate::hub::HubManager;
use crate::lane::{self, Lane, WorkKind};
use crate::layout::{self, Layout, RawLayout};
use crate::memory::{self, BackendInfo, Memory, MemoryId, RememberRequest, UpgradeS3Request};
use crate::models::{ModelProber, ModelReport};
use crate::notes::{self, NewNote, Review};
use crate::recent;
use crate::repo::Repo;
use crate::run::pipeline::{
    ApproveGateRequest, ReviseGateRequest, StartPipelineRequest, SubmitPrReviewRequest,
};
use crate::run::{Ending, Notify, Runner, StartedUpdate};
use crate::settings::{self, CustomAgent, RawSettings, Settings, TeamMemory};
use crate::usage::{
    ledger::{self, AgentActivity, Range, Scope, Totals},
    now_secs,
    quota::{QuotaFetch, QuotaPoller, QuotaReport, QuotaStatus},
    scanner::SessionScanner,
};

/// The page's files, served at these paths with these types: compiled into the binary as
/// they are, so the app needs no files beside it. A module the page imports must be listed
/// here (`every_page_module_is_served` checks).
const PAGE: &[(&str, &str, &[u8])] = &[
    ("/", "text/html", include_bytes!("../ui/index.html")),
    ("/app.css", "text/css", include_bytes!("../ui/app.css")),
    ("/app.js", "text/javascript", include_bytes!("../ui/app.js")),
    (
        "/fuzzy.js",
        "text/javascript",
        include_bytes!("../ui/fuzzy.js"),
    ),
    (
        "/tree.js",
        "text/javascript",
        include_bytes!("../ui/tree.js"),
    ),
    (
        "/definitions.js",
        "text/javascript",
        include_bytes!("../ui/definitions.js"),
    ),
    (
        "/definitions-dialog.js",
        "text/javascript",
        include_bytes!("../ui/definitions-dialog.js"),
    ),
    (
        "/dictation.js",
        "text/javascript",
        include_bytes!("../ui/dictation.js"),
    ),
    (
        "/voice.js",
        "text/javascript",
        include_bytes!("../ui/voice.js"),
    ),
    (
        "/icons.js",
        "text/javascript",
        include_bytes!("../ui/icons.js"),
    ),
    (
        "/settings.js",
        "text/javascript",
        include_bytes!("../ui/settings.js"),
    ),
    (
        "/settings-controls.js",
        "text/javascript",
        include_bytes!("../ui/settings-controls.js"),
    ),
    (
        "/settings-dialog.js",
        "text/javascript",
        include_bytes!("../ui/settings-dialog.js"),
    ),
    (
        "/usage.js",
        "text/javascript",
        include_bytes!("../ui/usage.js"),
    ),
    (
        "/usage-bar.js",
        "text/javascript",
        include_bytes!("../ui/usage-bar.js"),
    ),
    (
        "/usage-dialog.js",
        "text/javascript",
        include_bytes!("../ui/usage-dialog.js"),
    ),
    (
        "/wizard.js",
        "text/javascript",
        include_bytes!("../ui/wizard.js"),
    ),
    (
        "/wizard-dialog.js",
        "text/javascript",
        include_bytes!("../ui/wizard-dialog.js"),
    ),
    ("/run.js", "text/javascript", include_bytes!("../ui/run.js")),
    (
        "/patch.js",
        "text/javascript",
        include_bytes!("../ui/patch.js"),
    ),
    (
        "/changes-view.js",
        "text/javascript",
        include_bytes!("../ui/changes-view.js"),
    ),
    (
        "/run-panel.js",
        "text/javascript",
        include_bytes!("../ui/run-panel.js"),
    ),
    (
        "/run-store.js",
        "text/javascript",
        include_bytes!("../ui/run-store.js"),
    ),
    (
        "/run-tab.js",
        "text/javascript",
        include_bytes!("../ui/run-tab.js"),
    ),
    (
        "/run-dialog.js",
        "text/javascript",
        include_bytes!("../ui/run-dialog.js"),
    ),
    (
        "/panes.js",
        "text/javascript",
        include_bytes!("../ui/panes.js"),
    ),
    (
        "/status.js",
        "text/javascript",
        include_bytes!("../ui/status.js"),
    ),
    (
        "/note-dialog.js",
        "text/javascript",
        include_bytes!("../ui/note-dialog.js"),
    ),
    (
        "/note-place.js",
        "text/javascript",
        include_bytes!("../ui/note-place.js"),
    ),
    (
        "/path-note-dialog.js",
        "text/javascript",
        include_bytes!("../ui/path-note-dialog.js"),
    ),
    (
        "/question.js",
        "text/javascript",
        include_bytes!("../ui/question.js"),
    ),
    (
        "/question-form.js",
        "text/javascript",
        include_bytes!("../ui/question-form.js"),
    ),
    ("/dom.js", "text/javascript", include_bytes!("../ui/dom.js")),
    (
        "/dialog-parts.js",
        "text/javascript",
        include_bytes!("../ui/dialog-parts.js"),
    ),
    (
        "/bench.js",
        "text/javascript",
        include_bytes!("../ui/bench.js"),
    ),
    (
        "/code-view.js",
        "text/javascript",
        include_bytes!("../ui/code-view.js"),
    ),
    (
        "/context-menu.js",
        "text/javascript",
        include_bytes!("../ui/context-menu.js"),
    ),
    (
        "/dividers.js",
        "text/javascript",
        include_bytes!("../ui/dividers.js"),
    ),
    (
        "/file-list.js",
        "text/javascript",
        include_bytes!("../ui/file-list.js"),
    ),
    (
        "/memory.js",
        "text/javascript",
        include_bytes!("../ui/memory.js"),
    ),
    (
        "/memory-tab.js",
        "text/javascript",
        include_bytes!("../ui/memory-tab.js"),
    ),
    (
        "/memory-dialog.js",
        "text/javascript",
        include_bytes!("../ui/memory-dialog.js"),
    ),
    (
        "/notes-tray.js",
        "text/javascript",
        include_bytes!("../ui/notes-tray.js"),
    ),
    (
        "/welcome.js",
        "text/javascript",
        include_bytes!("../ui/welcome.js"),
    ),
    ("/hub.js", "text/javascript", include_bytes!("../ui/hub.js")),
    (
        "/hub-view.js",
        "text/javascript",
        include_bytes!("../ui/hub-view.js"),
    ),
    (
        "/plan-gate.js",
        "text/javascript",
        include_bytes!("../ui/plan-gate.js"),
    ),
    (
        "/pr-review.js",
        "text/javascript",
        include_bytes!("../ui/pr-review.js"),
    ),
    (
        "/diagram.js",
        "text/javascript",
        include_bytes!("../ui/diagram.js"),
    ),
    (
        "/diagram-view.js",
        "text/javascript",
        include_bytes!("../ui/diagram-view.js"),
    ),
    // The map's layout library (ui/vendor/dagre.LICENSE), loaded by a script element when the
    // map first opens, not imported.
    (
        "/vendor/dagre.min.js",
        "text/javascript",
        include_bytes!("../ui/vendor/dagre.min.js"),
    ),
    (
        "/icons.svg",
        "image/svg+xml",
        include_bytes!("../ui/icons.svg"),
    ),
    (
        "/agent-logos.svg",
        "image/svg+xml",
        include_bytes!("../ui/agent-logos.svg"),
    ),
    (
        "/fonts/Geist-Variable.woff2",
        "font/woff2",
        include_bytes!("../ui/fonts/Geist-Variable.woff2"),
    ),
    (
        "/fonts/GeistMono-Variable.woff2",
        "font/woff2",
        include_bytes!("../ui/fonts/GeistMono-Variable.woff2"),
    ),
];

/// The settings' file name in the configuration directory.
const SETTINGS_FILE: &str = "settings.json";

/// The site the page talks to, with the start-up project open (see [`startup_repo`]), and
/// the settings the window starts with.
pub(crate) fn start_site(
    dir: Option<&Path>,
    bench: Option<&str>,
    notify: Notify,
) -> Result<(Site, Settings)> {
    // Without a configuration directory the recent list and the settings are off, which the
    // page says: both are conveniences, so they never stop the app from starting.
    let config = config_dir().ok();
    let settings = config.as_ref().map_or_else(Settings::default, |dir| {
        settings::load(&dir.join(SETTINGS_FILE)).settings
    });
    // A benchmark measures the window, so it does not spend a core asking the login shell.
    let agents = match bench {
        Some(_) => Scanner::new(OsString::new),
        None => Scanner::new(agents::search_path),
    };
    let user_home = std::env::var_os("HOME")
        // An empty HOME would put the worktrees in the current folder.
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let home = user_home.as_deref().map(|dir| dir.join(".postil"));
    let sessions = match bench {
        Some(_) => SessionScanner::disabled(),
        None => SessionScanner::new(user_home),
    };
    let site = Site::new(config.as_deref(), home.as_deref(), bench, agents, notify)
        .with_session_scanner(sessions);
    if let Some(repo) = startup_repo(dir, &settings, site.recent_file.as_deref())? {
        site.open_with_settings(repo, LiveRun::Stop, Some(&settings))?;
    }
    // A benchmark of a file that cannot be read would time an empty pane and report it as a
    // result. Fail here instead, before the window opens.
    if let Some(file) = bench {
        site.repo()?.read(file)?;
    }
    Ok((site, settings))
}

/// The project to open at start: the one containing `dir` when given, else the most recent
/// project when the settings ask for it and it still opens, else none (the welcome screen).
///
/// # Errors
///
/// As [`Repo::open`], for `dir` only: a recent project that no longer opens is skipped, and
/// the welcome screen lists it as missing.
fn startup_repo(
    dir: Option<&Path>,
    settings: &Settings,
    recent_file: Option<&Path>,
) -> Result<Option<Repo>> {
    if let Some(dir) = dir {
        return Repo::open(dir).map(Some);
    }
    if !settings.reopen_last_project() {
        return Ok(None);
    }
    let recent = recent_file.map_or_else(Vec::new, recent::list);
    let last = recent.first();
    Ok(last.and_then(|root| Repo::open(Path::new(root)).ok()))
}

/// Everything the page can ask for.
///
/// Shared across the request worker pool (`spawn_site_worker`), so the open project sits behind
/// an `RwLock`: read-only routes take a brief read lock to clone `Arc` handles, while project
/// open/close takes the write lock.
pub(crate) struct Site {
    /// The open project, if any.
    project: RwLock<Option<Project>>,
    /// Serializes `open_with_settings` so slow project setup runs outside `project.write()`.
    open_lock: Mutex<()>,
    highlighter: Highlighter,
    /// The file the benchmark opens, when this is a benchmark run.
    bench: Option<String>,
    /// Where the recent projects are listed (`recent.rs`); `None` when the configuration
    /// directory is unknown, which turns the list off. `config_dir` fails only with
    /// `Error::NoHome`, which is the reason `open` gives for a `None` here.
    recent_file: Option<PathBuf>,
    /// Where the settings are kept (`settings.rs`); `None`, for the same reason as
    /// `recent_file`, turns them off: the defaults apply and saving fails.
    settings_file: Option<PathBuf>,
    /// Cached loaded settings keyed by `(modified, len)` so repeated polls skip re-parsing.
    settings_cache: Mutex<Option<SettingsCache>>,
    /// Where the pane widths are kept (`layout.rs`); `None`, for the same reason as
    /// `recent_file`, keeps them in each window only, until it closes.
    layout_file: Option<PathBuf>,
    /// Where completed runs' usage records are appended (`usage/ledger.rs`).
    usage_file: Option<PathBuf>,
    /// Cached parsed records of `usage_file`, keyed by `(modified, len)` so repeated usage
    /// polls do not re-read and re-parse `usage.jsonl`.
    ledger_cache: Mutex<Option<LedgerCache>>,
    /// Postil's home folder, `~/.postil`, under which threads' worktrees live (`thread.rs`);
    /// `None` when the home folder is unknown, which refuses every send.
    home: Option<PathBuf>,
    /// Where agents' programs are looked for, found in the background.
    agents: Scanner,
    /// On-demand background probes for live provider quotas (`usage/quota.rs`).
    quotas: QuotaPoller,
    /// Read-only background scanner for local CLI session transcripts (`usage/scanner.rs`).
    sessions: SessionScanner,
    /// On-demand background probes for the models each agent offers (`models.rs`).
    models: ModelProber,
    /// Whether hippius-mem is installed and set up, checked in the background (`memory.rs`).
    memory: Memory,
    /// Multi-repo Agentic Developer Hub (`hub.rs`): monitored repos, GitHub sync, priority triage.
    hub: HubManager,
    /// Tells the page a run changed (`run.rs`).
    notify: Notify,
}

/// Cached parsed `usage.jsonl` records, invalidated when `(modified, len)` changes.
struct LedgerCache {
    modified: Option<SystemTime>,
    parsed_offset: u64,
    dev: u64,
    ino: u64,
    records: Arc<[ledger::LedgerRecord]>,
}

/// Cached `settings.json` load result, invalidated when `(modified, len)` changes.
struct SettingsCache {
    /// `None` when the file did not exist at the cached check.
    stamp: Option<(SystemTime, u64)>,
    loaded: settings::Loaded,
}

/// Bounded LRU cache of serialized `GET /api/file` JSON responses for the open project.
const FILE_CACHE_CAP: usize = 24;
/// Maximum total bytes held by [`FileCache`] across all cached entries.
const FILE_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
/// Largest single serialized `GET /api/file` response eligible for caching.
const FILE_CACHE_MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Default)]
struct FileCache {
    entries: Vec<CachedFile>,
    total_bytes: usize,
}

#[derive(Debug)]
struct CachedFile {
    path: String,
    modified: SystemTime,
    len: u64,
    json: Arc<[u8]>,
}

impl FileCache {
    fn get(&mut self, path: &str, modified: SystemTime, len: u64) -> Option<Arc<[u8]>> {
        let index = self.entries.iter().position(|entry| {
            entry.path == path && entry.modified == modified && entry.len == len
        })?;
        let entry = self.entries.remove(index);
        let json = Arc::clone(&entry.json);
        self.entries.push(entry);
        Some(json)
    }

    fn insert(&mut self, path: String, modified: SystemTime, len: u64, json: Arc<[u8]>) {
        if json.len() > FILE_CACHE_MAX_ENTRY_BYTES {
            return;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
            let old = self.entries.remove(index);
            self.total_bytes = self.total_bytes.saturating_sub(old.json.len());
        }
        while !self.entries.is_empty()
            && (self.entries.len() >= FILE_CACHE_CAP
                || self.total_bytes.saturating_add(json.len()) > FILE_CACHE_MAX_BYTES)
        {
            let evicted = self.entries.remove(0);
            self.total_bytes = self.total_bytes.saturating_sub(evicted.json.len());
        }
        self.total_bytes = self.total_bytes.saturating_add(json.len());
        self.entries.push(CachedFile {
            path,
            modified,
            len,
            json,
        });
    }
}

/// Cached `GET /api/notes` serialized JSON keyed by `review.json` stamp and referenced file stamps.
#[derive(Debug)]
struct NotesCache {
    review_stamp: Option<(SystemTime, u64)>,
    path_stamps: Vec<(String, PathStamp)>,
    json: Arc<[u8]>,
}

#[derive(Debug, PartialEq, Eq)]
enum PathStamp {
    File(Option<(SystemTime, u64)>),
    Kind(Option<crate::repo::PathKind>),
}

impl NotesCache {
    fn matches(&self, repo: &Repo) -> bool {
        if file_stamp(&repo.review_file()) != self.review_stamp {
            return false;
        }
        self.path_stamps
            .iter()
            .all(|(path, expected)| match expected {
                PathStamp::File(expected_stamp) => {
                    let current = repo.inside(path).ok().and_then(|full| file_stamp(&full));
                    current == *expected_stamp
                }
                PathStamp::Kind(expected_kind) => repo.kind(path).ok().flatten() == *expected_kind,
            })
    }

    fn capture(repo: &Repo, review: &notes::Review, json: Arc<[u8]>) -> Self {
        let review_stamp = file_stamp(&repo.review_file());
        let mut seen_files = std::collections::HashSet::new();
        let mut seen_paths = std::collections::HashSet::new();
        let mut path_stamps = Vec::new();
        for (path, is_line) in review.referenced_targets() {
            if is_line {
                if seen_files.insert(path.to_owned()) {
                    let stamp = repo.inside(path).ok().and_then(|full| file_stamp(&full));
                    path_stamps.push((path.to_owned(), PathStamp::File(stamp)));
                }
            } else if seen_paths.insert(path.to_owned()) {
                let kind = repo.kind(path).ok().flatten();
                path_stamps.push((path.to_owned(), PathStamp::Kind(kind)));
            }
        }
        Self {
            review_stamp,
            path_stamps,
            json,
        }
    }
}

fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    meta.modified().ok().map(|modified| (modified, meta.len()))
}

/// The open project.
#[derive(Debug, Clone)]
struct Project {
    repo: Arc<Repo>,
    /// The project's runs and the agent kept between them.
    runner: Arc<Runner>,
    /// Where each name is defined, indexed on a thread of its own.
    definitions: Definitions,
    /// Why the recent projects could not be updated when this project opened, shown under
    /// the branch name. Kept with the project, so every page load of it shows the line.
    warning: Option<String>,
    /// Bounded cache of highlighted file JSON payloads, validated by `(modified, len)`.
    file_cache: Arc<Mutex<FileCache>>,
    /// Cached `GET /api/notes` response keyed by `review.json` and referenced file stamps.
    notes_cache: Arc<Mutex<Option<NotesCache>>>,
}

/// The reply to `/api/repo` when a project is open; `null` when none is.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RepoInfo<'a> {
    name: String,
    root: String,
    branch: String,
    files: Vec<String>,
    bench: Option<&'a str>,
    warning: Option<String>,
    /// The repository's name in team memory ([`Repo::memory_name`]); `None` without an
    /// `origin` remote, when the Remember dialog offers only the whole team's memory.
    memory_repo: Option<String>,
}

/// An entry of the reply to `/api/recent`.
#[derive(Debug, Serialize)]
struct RecentProject {
    path: String,
    name: String,
    /// Whether the folder is still there; a moved or deleted one is listed, greyed out.
    exists: bool,
}

/// The reply to `/api/settings`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView<'a> {
    settings: &'a Settings,
    /// Why the settings file is not in use; saving is refused while this is set.
    problem: Option<String>,
    /// The file's revision, which a save names (`settings::save`).
    revision: String,
    /// The settings file, for the dialog to name; `None` without a home directory.
    file: Option<String>,
    /// Whether there is a configuration directory but no settings file in it yet, so the
    /// startup wizard opens on first launch.
    first_run: bool,
    /// Every agent, built-in ones first; `None` while the agent scan runs.
    agents: Option<Vec<AgentView<'a>>>,
    /// Team memory: whether hippius-mem is installed and set up.
    memory: MemoryView,
    /// External tools found on the search path; `None` while the agent scan runs.
    tools: Option<ToolsView>,
    /// The smallest and largest code font size.
    font_sizes: [u8; 2],
    tab_widths: [u8; 3],
}

/// Team memory as the settings dialog shows it.
#[derive(Debug, Serialize)]
struct MemoryView {
    /// Whether a check or a set up runs, so the page asks again shortly.
    pending: bool,
    /// `None` while a check runs.
    state: Option<MemoryStateView>,
    /// The last set up; `None` before the first.
    setup: Option<memory::Setup>,
    /// Detected storage backend (`local` trial vault vs paid remote `s3` bucket).
    backend: BackendInfo,
}

/// [`memory::State`] with its path as text.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum MemoryStateView {
    Missing,
    Unconfigured {
        path: String,
        problem: Option<String>,
    },
    Ready {
        path: String,
    },
}

impl From<memory::Status> for MemoryView {
    fn from(status: memory::Status) -> Self {
        let text = |path: &Path| path.to_string_lossy().into_owned();
        let state = status.state.map(|state| match state {
            memory::State::Missing => MemoryStateView::Missing,
            memory::State::Unconfigured { path, problem } => MemoryStateView::Unconfigured {
                path: text(&path),
                problem,
            },
            memory::State::Ready { path } => MemoryStateView::Ready { path: text(&path) },
        });
        Self {
            pending: state.is_none() || status.setup == Some(memory::Setup::Running),
            state,
            setup: status.setup,
            backend: status.backend,
        }
    }
}

/// External tools found on the search path, for the startup wizard's readiness check.
#[derive(Debug, Serialize)]
struct ToolsView {
    git: Option<String>,
    npx: Option<String>,
    gh: Option<String>,
}

/// The body of `PUT /api/settings`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveSettings {
    /// The revision of the settings file the change started from.
    revision: String,
    settings: RawSettings,
}

/// The body of `POST /api/agents/models`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelsRequest {
    /// The agents whose models the page shows, by name.
    agents: Vec<String>,
    /// The page's custom agents, saved or not: the wizard saves only when it finishes. A
    /// name found here is probed with this command, before the saved settings' one.
    #[serde(default)]
    custom_agents: Vec<CustomAgent>,
}

/// The reply to `POST /api/agents/models`.
#[derive(Debug, Serialize)]
struct ModelsView {
    /// Whether any report is still loading, so the page asks again shortly.
    loading: bool,
    /// Each asked-for agent's report, by name.
    models: BTreeMap<String, ModelReport>,
}

/// An agent as the settings dialog lists it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentView<'a> {
    name: &'a str,
    command: &'a str,
    built_in: bool,
    /// Where the agent's program is; `None` when it is not installed.
    found: Option<String>,
}

/// The reply to `GET /api/usage` and `POST /api/usage/refresh`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageView {
    scope: Scope,
    has_project: bool,
    project_name: Option<String>,
    range: Range,
    scanning: bool,
    polling: bool,
    ledger_file: Option<String>,
    summary: Totals,
    /// Installed built-in agents, installed/used custom agents, and any other agent with
    /// recorded usage in the selected window; `None` while the agent scan runs.
    agents: Option<Vec<UsageAgentView>>,
}

/// One agent's live quota and filtered Postil activity in [`UsageView`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageAgentView {
    name: String,
    built_in: bool,
    found: Option<String>,
    enabled: bool,
    quota: QuotaReport,
    activity: AgentActivity,
}

/// The reply to `GET /api/layout`.
#[derive(Debug, Serialize)]
struct LayoutView {
    #[serde(flatten)]
    layout: Layout,
    /// The widths a double-click on a divider resets to.
    defaults: Layout,
    limits: LayoutLimits,
}

/// The smallest and largest width of each resizable pane, in CSS px.
#[derive(Debug, Serialize)]
struct LayoutLimits {
    sidebar: [u32; 2],
    tray: [u32; 2],
    map: [u32; 2],
}

/// The body of `POST /api/run`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendRun {
    /// The agent's name; the settings' agent when absent.
    agent: Option<String>,
    /// The ids of the notes to send, in order.
    notes: Vec<String>,
}

/// The body of `POST /api/run/permission`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerPermission {
    /// The run that asks, `R3`.
    run: String,
    /// The request's number, as the run's log gives it (the agent's, counting on across the
    /// turns of a kept agent).
    request: usize,
    /// The id of the option chosen, one the agent offered.
    option: String,
}

/// The body of `POST /api/run/cancel`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelRun {
    /// The run to stop, `R3`.
    run: String,
}

/// The body of `POST /api/run/review`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRun {
    /// The run to review, `R3`.
    of: String,
}

/// The body of `PATCH /api/notes?id=N1`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditNote {
    /// The note's new text.
    body: String,
}

/// The body of `POST /api/memory/draft`: the note a memory is drafted from.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftRequest {
    note: String,
}

/// The reply to `POST /api/memory/draft`. `fallback` is the note's text, which the dialog opens
/// with when no draft comes.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum DraftReply {
    /// No draft run: the note was never sent, or its thread ended or has no worktree record.
    Fallback { body: String },
    /// The draft run started; its view's `draft` holds the draft once it is over.
    Run { run: String, fallback: String },
}

/// The body of `POST /api/run/follow`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowRun {
    /// The run to follow up, the latest of its thread: `R3`.
    of: String,
    /// What the engineer asks.
    text: String,
}

/// The body of the `POST` routes under `/api/thread`: `apply`, `discard`, `pr`, `pr/state`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadBody {
    /// The thread, by its first run: `R3`.
    thread: String,
}
/// The body of `POST /api/run/question`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerQuestion {
    /// The run that asks, `R3`.
    run: String,
    /// The question's number, as the run's log gives it.
    request: usize,
    /// The form's values by field, or `null` to skip the question.
    content: Option<serde_json::Map<String, serde_json::Value>>,
}

/// The reply to `/api/run/preview`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewView {
    /// The prompt's text, exactly as the agent will receive it.
    prompt: String,
    /// The files linked after the text, relative to the repository root. A folder note's
    /// folder is not among them (`notes::Prompt::files`).
    files: Vec<String>,
    #[serde(flatten)]
    uncommitted: UncommittedView,
}

/// The reply to `/api/run/uncommitted`, and part of `/api/run/preview`'s.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UncommittedView {
    /// How many changes the engineer's working tree has since the last commit, as `git status`
    /// lists them, which the thread, started from that commit, does not see (docs/design.md,
    /// "Threads"); `None` when the status could not be read, which the page says.
    uncommitted: Option<usize>,
    /// The files asked about among them (the linked files of a preview, the file of an ask):
    /// their notes' lines may not match the agent's copy.
    uncommitted_noted: Vec<String>,
    /// Why `git status` failed, when `uncommitted` is `None`.
    uncommitted_error: Option<String>,
}

/// The body of `POST /api/run/ask`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskRun {
    /// The question, a new note as `POST /api/notes` takes one.
    note: NewNote,
}

/// The body of `POST /api/memory/join`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinBundleBody {
    bundle: String,
}

/// The body of `POST /api/hub/repos`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HubRepoRequest {
    action: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    selected: Option<bool>,
}

/// The body of `POST /api/hub/evaluate`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HubEvaluateRequest {
    repo: String,
    kind: WorkKind,
    number: u64,
}

/// The body of `POST /api/open` and `POST /api/init`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderRequest {
    path: PathBuf,
    /// Whether to stop a run under way in another project; refused (409) without it. The
    /// page sends it once the engineer agreed.
    #[serde(default)]
    stop: bool,
}

/// What opening another project does while the open one has a run under way.
#[derive(Debug, Clone, Copy)]
enum LiveRun {
    /// Refuses to open it, so the page can ask first.
    Refuse,
    /// Stops the run.
    Stop,
}

/// The reply to `POST /api/analysis/check` ([`Site::check_analysis`]), as `{"state": ...}`.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
enum CheckView {
    /// `checked` is `HEAD`, or another window started the update for it.
    Same,
    /// `checked` moved to `HEAD` with no run.
    Untouched,
    /// `checked` is gone, or no ancestor of `HEAD`: the pane offers Update.
    Diverged,
    /// The forced update `run` started at the commit whose short id is `commit`.
    Updated { run: String, commit: String },
    /// The update could not start, for `reason`; the next check tries again.
    Blocked { reason: String },
}

/// The reply to `/api/file`.
#[derive(Debug, Serialize)]
struct FileView<'a> {
    path: &'a str,
    /// Whether the file's language has definitions to jump to.
    definitions: bool,
    lines: HighlightedLines<'a>,
}

pub(crate) type Reply = Response<Cow<'static, [u8]>>;

impl Site {
    /// A site with no project open, keeping its files in `config` (`None` turns the recent
    /// list and the settings off), and threads' worktrees under `home` (`None` refuses every
    /// send).
    fn new(
        config: Option<&Path>,
        home: Option<&Path>,
        bench: Option<&str>,
        agents: Scanner,
        notify: Notify,
    ) -> Self {
        // A test that sets no memory up must not run the machine's hippius-mem, whose `doctor`
        // writes a probe into the engineer's vault: its memory finds no program.
        #[cfg(not(test))]
        let memory = Memory::new(agents.clone());
        #[cfg(test)]
        let memory = Memory::new(Scanner::new(OsString::new));
        let hub = HubManager::new(
            config.map(|dir| dir.join("hub.json")),
            agents.clone(),
            memory.clone(),
            Arc::clone(&notify),
        );
        Self {
            project: RwLock::new(None),
            open_lock: Mutex::new(()),
            highlighter: Highlighter::new(),
            bench: bench.map(str::to_owned),
            recent_file: config.map(|dir| dir.join("recent.json")),
            settings_file: config.map(|dir| dir.join(SETTINGS_FILE)),
            settings_cache: Mutex::new(None),
            layout_file: config.map(|dir| dir.join("layout.json")),
            usage_file: config.map(|dir| dir.join("usage.jsonl")),
            ledger_cache: Mutex::new(None),
            home: home.map(Path::to_owned),
            models: ModelProber::live(agents.clone()),
            memory,
            hub,
            agents,
            quotas: QuotaPoller::live(),
            sessions: SessionScanner::disabled(),
            notify,
        }
    }

    pub(crate) fn with_session_scanner(mut self, sessions: SessionScanner) -> Self {
        self.sessions = sessions;
        self
    }

    /// Replaces the memory; it runs in the open project, when one is open, which is opened
    /// again so that its runner hands this memory to its runs.
    #[cfg(test)]
    pub(crate) fn with_memory(mut self, memory: Memory) -> Self {
        self.memory = memory;
        let open = self
            .project
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(open) = open {
            let repo = Repo::open(open.repo.root()).expect("the open project opens again");
            open.definitions.close();
            open.runner.close();
            self.open(repo, LiveRun::Stop)
                .expect("no project is open, so none refuses");
        }
        self
    }

    #[cfg(test)]
    pub(crate) fn with_quota_poller(mut self, quotas: QuotaPoller) -> Self {
        self.quotas = quotas;
        self
    }

    /// The open project's repository.
    fn repo(&self) -> Result<Arc<Repo>> {
        self.project
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|project| Arc::clone(&project.repo))
            .ok_or(Error::NoProject)
    }

    /// Makes `repo` the open project and puts it first among the recent projects. A
    /// benchmark does not count as opening: a scripted run must not reorder the list.
    ///
    /// Opening the project that is open keeps its runner, and with it a run under way and
    /// the kept agent: the page only reloads. Opening another one stops the open project's
    /// agent, and `live` says whether to do that during a run (decided with the owner on
    /// 2026-11-23: the page asks first). No page request starts a run between the check and
    /// the switch, because requests are answered one at a time, on one thread
    /// (`spawn_site_worker`); a review's next stage, which starts without one, counts as
    /// under way ([`Runner::busy`]). Takes the runner's slot lock while it borrows the
    /// project; the runner never reaches back into the site.
    ///
    /// # Errors
    ///
    /// [`Error::RunInOpenProject`] when `live` is [`LiveRun::Refuse`], `repo` is another
    /// project, and the open project has a run or a review under way.
    fn open(&self, repo: Repo, live: LiveRun) -> Result<()> {
        self.open_with_settings(repo, live, None)
    }

    fn open_with_settings(
        &self,
        repo: Repo,
        live: LiveRun,
        settings: Option<&Settings>,
    ) -> Result<()> {
        // Serialize concurrent `open` calls without holding `self.project.write()` across disk
        // I/O, settings parsing, and runner/definitions construction.
        let _opening = self
            .open_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let same = {
            let project = self.project.read().unwrap_or_else(PoisonError::into_inner);
            let open = project.as_ref();
            let same = open.is_some_and(|open| open.repo.root() == repo.root());
            if !same
                && matches!(live, LiveRun::Refuse)
                && open.is_some_and(|open| open.runner.busy())
            {
                return Err(Error::RunInOpenProject);
            }
            same
        };
        let warning = match (&self.recent_file, &self.bench) {
            (_, Some(_)) => None,
            (None, None) => Some(format!("recent projects are off: {}", Error::NoHome)),
            (Some(file), None) => recent::record(file, repo.root())
                .err()
                .map(|error| error_chain(&error)),
        };
        if same {
            let mut project = self.project.write().unwrap_or_else(PoisonError::into_inner);
            if let Some(open) = project.as_mut() {
                open.warning = warning;
                return Ok(());
            }
        }
        let repo = Arc::new(repo);
        let team_memory = settings.map_or_else(
            || self.current_settings().team_memory(),
            Settings::team_memory,
        );
        let runner = Runner::new(
            Arc::clone(&repo),
            self.agents.clone(),
            self.memory.clone(),
            Arc::clone(&self.notify),
            self.home.clone(),
            self.usage_file.clone(),
        );
        let definitions = Definitions::new(Arc::clone(&repo));
        let next = Project {
            repo: Arc::clone(&repo),
            runner,
            definitions,
            warning,
            file_cache: Arc::new(Mutex::new(FileCache::default())),
            notes_cache: Arc::new(Mutex::new(None)),
        };
        let old = {
            let mut project = self.project.write().unwrap_or_else(PoisonError::into_inner);
            if matches!(live, LiveRun::Refuse)
                && project.as_ref().is_some_and(|open| open.runner.busy())
            {
                drop(project);
                next.definitions.close();
                next.runner.close();
                return Err(Error::RunInOpenProject);
            }
            project.replace(next)
        };
        // Switch `Memory` only after `project.replace(next)` commits so a refused open never
        // switches team memory away from the still-open busy project.
        self.memory.open_project(&repo);
        // A run gives its agent team memory only once a check found hippius-mem Ready, and the
        // first check would otherwise wait for the first send or the settings dialog: the send
        // step's preview would leave out the line about it that the send then has.
        if team_memory == TeamMemory::On {
            drop(self.memory.status());
        }
        // Stops the index and the agent of the project this one replaces, while still holding
        // `open_lock` so a concurrent `POST /api/open` cannot overlap with `old.runner.close()`.
        if let Some(old) = old {
            old.definitions.close();
            old.runner.close();
        }
        Ok(())
    }

    /// The reply to `request`: a page file, or the API route its path names. Each route is
    /// listed once, by its path, and takes the methods its arm lists; any other method gets
    /// 405 with what the route takes.
    pub(crate) fn respond(&self, request: &Request<Vec<u8>>) -> Reply {
        let uri = request.uri();
        if let Some(asset) = page_asset(uri.path()) {
            return asset;
        }
        let method = request.method();
        let query = uri.query().unwrap_or("");
        let body = request.body();
        match uri.path() {
            "/api/repo" => match *method {
                Method::GET => json_or_error(self.repo_info()),
                _ => not_allowed("GET"),
            },
            "/api/recent" => match *method {
                Method::GET => json_or_error(Ok(self.recent())),
                _ => not_allowed("GET"),
            },
            "/api/open" => match *method {
                Method::POST => json_or_error(self.open_folder(body, Repo::open)),
                _ => not_allowed("POST {\"path\": ...}"),
            },
            "/api/init" => match *method {
                Method::POST => json_or_error(self.open_folder(body, Repo::init)),
                _ => not_allowed("POST {\"path\": ...}"),
            },
            "/api/file" => match *method {
                Method::GET => match query_value(query, "path") {
                    Some(path) => json_or_error(self.file_view(&path)),
                    None => bad_request("expected ?path=<percent-encoded path>"),
                },
                _ => not_allowed("GET"),
            },
            path if path == "/api/definitions" || path.starts_with("/api/definitions/") => {
                self.respond_definitions(path, method, query)
            }
            "/api/notes" => self.respond_notes(method, query, body),
            "/api/notes/resolve" => match *method {
                Method::POST => self.change_note(query, |project, id| {
                    // An ask's thread ends first, freeing its worktree; refused while it runs.
                    if notes::is_ask(&project.repo, id)? {
                        project.runner.end_ask(id)?;
                    }
                    notes::resolve(&project.repo, id)
                }),
                _ => not_allowed("POST ?id=<note id> with no body"),
            },
            "/api/notes/reopen" => match *method {
                Method::POST => {
                    self.change_note(query, |project, id| notes::reopen(&project.repo, id))
                }
                _ => not_allowed("POST ?id=<note id> with no body"),
            },
            "/api/folders" => match *method {
                Method::GET => json_or_error(
                    self.repo()
                        .map(|repo| to_json(&folders::load(&repo.folders_file()))),
                ),
                Method::PUT => json_or_error(self.save_folders(body)),
                _ => not_allowed("GET the open folders or PUT them"),
            },
            "/api/settings" => match *method {
                Method::GET => reply(StatusCode::OK, "application/json", self.settings()),
                Method::PUT => json_or_error(self.save_settings(body)),
                _ => not_allowed("GET the settings or PUT new ones"),
            },
            "/api/settings/reset" => match *method {
                Method::POST => json_or_error(self.reset_settings()),
                _ => not_allowed("POST with no body"),
            },
            path if path.starts_with("/api/agents/") => self.respond_agents(path, method, body),
            path if path == "/api/memory" || path.starts_with("/api/memory/") => {
                self.respond_memory(request)
            }
            "/api/usage" => match *method {
                Method::GET => json_or_error(self.usage_view(query, QuotaFetch::Cached)),
                _ => not_allowed("GET"),
            },
            "/api/usage/refresh" => match *method {
                Method::POST => json_or_error(self.usage_view(query, QuotaFetch::Refresh)),
                _ => not_allowed("POST"),
            },
            "/api/layout" => match *method {
                Method::GET => reply(StatusCode::OK, "application/json", self.layout()),
                Method::PUT => json_or_error(self.save_layout(body)),
                _ => not_allowed("GET the pane widths or PUT new ones"),
            },
            path if path.starts_with("/api/analysis") => self.respond_analysis(request),
            "/api/runs" => match *method {
                Method::GET => json_or_error(self.runs()),
                _ => not_allowed("GET"),
            },
            path if path == "/api/run" || path.starts_with("/api/run/") => {
                self.respond_run(path, method, query, body)
            }
            path if path == "/api/thread" || path.starts_with("/api/thread/") => {
                self.respond_thread(path, method, query, body)
            }
            path if is_hub_or_lane_path(path) => {
                self.respond_hub_or_lane(path, method, query, body)
            }
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    /// The open project, or `null`: the page then shows the welcome screen.
    fn repo_info(&self) -> Result<Vec<u8>> {
        let Some((repo, warning)) = self
            .project
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|project| (Arc::clone(&project.repo), project.warning.clone()))
        else {
            return Ok(b"null".to_vec());
        };
        let (branch, files, memory_repo) = std::thread::scope(|scope| {
            let files_handle = scope.spawn(|| repo.files());
            let memory_handle = scope.spawn(|| repo.memory_name());
            let branch = repo.branch();
            let files = files_handle
                .join()
                .expect("repo.files worker does not panic");
            let memory_repo = memory_handle
                .join()
                .expect("repo.memory_name worker does not panic");
            (branch, files, memory_repo)
        });
        let info = RepoInfo {
            name: repo.name(),
            root: repo.root().to_string_lossy().into_owned(),
            branch: branch?,
            files: files?,
            bench: self.bench.as_deref(),
            warning,
            memory_repo,
        };
        Ok(to_json(&info))
    }

    fn recent(&self) -> Vec<u8> {
        let listed = self.recent_file.as_deref().map(recent::list);
        // `exists` checks each folder on the request thread. A folder on a network volume that
        // is gone can make that slow; accepted, as ten checks on local disks take microseconds.
        let projects: Vec<RecentProject> = listed
            .unwrap_or_default()
            .into_iter()
            .map(|path| {
                let folder = Path::new(&path);
                RecentProject {
                    name: folder
                        .file_name()
                        .map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned()),
                    exists: folder.is_dir(),
                    path,
                }
            })
            .collect();
        to_json(&projects)
    }

    /// The reply to `POST /api/open` and `POST /api/init`: makes the folder in the body the
    /// open project, through `open_repo` (`Repo::open`, or `Repo::init` to make it a
    /// repository first), and answers `{}`. The page then reloads.
    fn open_folder(&self, body: &[u8], open_repo: fn(&Path) -> Result<Repo>) -> Result<Vec<u8>> {
        let request: FolderRequest = parse_body(body, Error::BadRequest)?;
        let live = if request.stop {
            LiveRun::Stop
        } else {
            LiveRun::Refuse
        };
        self.open(open_repo(&request.path)?, live)?;
        Ok(b"{}".to_vec())
    }

    fn file_view(&self, path: &str) -> Result<Vec<u8>> {
        let (repo, file_cache) = {
            let guard = self.project.read().unwrap_or_else(PoisonError::into_inner);
            let project = guard.as_ref().ok_or(Error::NoProject)?;
            (Arc::clone(&project.repo), Arc::clone(&project.file_cache))
        };
        let read_stamp = || repo.inside(path).ok().and_then(|full| file_stamp(&full));
        if let Some((modified, len)) = read_stamp()
            && let Some(cached) = file_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(path, modified, len)
        {
            return Ok(cached.to_vec());
        }
        let (text, stamp) = repo.read_with_stamp(path)?;
        let lines = self.highlighter.lines(path, &text)?;
        let view = FileView {
            path,
            definitions: highlight::tags_grammar(path).is_some(),
            lines,
        };
        let mut out = Vec::with_capacity(text.len().saturating_mul(2).saturating_add(256));
        serde_json::to_writer(&mut out, &view).expect(
            "the reply types are plain structs of strings, numbers, and sequences, which always \
             serialize",
        );
        // Only cache when `(modified, len)` is unchanged after reading so concurrent edits never
        // populate the cache under a stale stamp.
        if let Some((modified, len)) = stamp
            && out.len() <= FILE_CACHE_MAX_ENTRY_BYTES
            && read_stamp() == Some((modified, len))
        {
            file_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(path.to_owned(), modified, len, Arc::from(out.as_slice()));
        }
        Ok(out)
    }

    /// The reply to `GET /api/definitions?name=x&from=path`: where `x` is defined, those
    /// nearest the file `from` first, or `{"state": "building"}` until the index is first
    /// built. Reads no file: the index is built on its own thread.
    fn definitions(&self, query: &str) -> Result<Vec<u8>> {
        let name = query_value(query, "name")
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                Error::BadRequest("expected ?name=<percent-encoded name>&from=<path>".to_owned())
            })?;
        let from = query_value(query, "from").unwrap_or_default();
        let definitions = self
            .project
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|project| project.definitions.clone())
            .ok_or(Error::NoProject)?;
        Ok(to_json(&definitions.lookup(&name, &from)))
    }

    /// The reply to `POST /api/definitions/refresh`, which the page sends when the window
    /// regains focus and when a run moves on (`refresh` in `ui/app.js`): the index looks at
    /// the files again, on its own thread. Answers `{}` at once.
    fn refresh_definitions(&self) -> Result<Vec<u8>> {
        let definitions = self
            .project
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|project| project.definitions.clone())
            .ok_or(Error::NoProject)?;
        definitions.refresh();
        Ok(b"{}".to_vec())
    }

    /// The reply to `GET /api/notes`: every note, moved to where its snippet is now. Reads
    /// the review file and never writes it. Cached by `review.json` and referenced file stamps.
    fn notes(&self) -> Result<Vec<u8>> {
        let (repo, notes_cache) = {
            let guard = self.project.read().unwrap_or_else(PoisonError::into_inner);
            let project = guard.as_ref().ok_or(Error::NoProject)?;
            (Arc::clone(&project.repo), Arc::clone(&project.notes_cache))
        };
        {
            let guard = notes_cache.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(cached) = guard.as_ref()
                && cached.matches(&repo)
            {
                return Ok(cached.json.to_vec());
            }
        }
        let pre_review_stamp = file_stamp(&repo.review_file());
        let review = notes::current(&repo)?;
        let json = to_json(&review);
        let entry = NotesCache::capture(&repo, &review, Arc::from(json.as_slice()));
        // Only populate the cache when `review.json` did not change during `notes::current` so a
        // concurrent mutation cannot pin stale JSON under the new stamp.
        if entry.review_stamp == pre_review_stamp {
            *notes_cache.lock().unwrap_or_else(PoisonError::into_inner) = Some(entry);
        } else {
            *notes_cache.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
        Ok(json)
    }

    /// The reply to `GET /api/analysis`: the diagrams, each code reference placed where its
    /// code is now, with the files not on the map (`analysis::view`), or `null` when there is
    /// no analysis. With `?at=<commit>`, the diagrams alone, placed where the code is in that
    /// commit (`analysis::at`), for a run's change map. Reads the analysis file and never
    /// writes it.
    fn analysis(&self, query: &str) -> Result<Vec<u8>> {
        let repo = self.repo()?;
        if let Some(commit) = query_value(query, "at") {
            return Ok(to_json(&analysis::at(&repo, &commit)?));
        }
        let analysis = analysis::current(&repo)?.map(|analysis| analysis::view(&repo, analysis));
        Ok(to_json(&analysis))
    }

    /// The reply to `POST /api/analysis` `{"kind": ..., "of": ..., "question": ...}`: starts an
    /// analysis run of the settings' Analyzer (`Runner::analyze`), and answers `{"run": "R5"}`.
    /// The page follows that run by its id: it is not the Run tab's latest.
    fn start_analysis(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: AnalyzeRequest = parse_body(body, Error::BadRequest)?;
        self.analyze(AnalysisJob::New(request))
    }

    /// The reply to `POST /api/analysis/update`, with `{"diagram": "D1"}` or no body for every
    /// diagram: starts an update of the outdated nodes and the files not on the map, as
    /// [`Site::start_analysis`] starts an analysis. An update of every diagram with nothing
    /// to send, while the checkout moved to another history, starts no run, moves `checked`,
    /// and answers `{"run": null}`.
    fn update_analysis(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: UpdateRequest = if body.is_empty() {
            UpdateRequest::default()
        } else {
            parse_body(body, Error::BadRequest)?
        };
        self.analyze(AnalysisJob::Update(request))
    }

    fn analyze(&self, job: AnalysisJob) -> Result<Vec<u8>> {
        let settings = self.current_settings();
        let agent = settings.analyzer()?;
        let run = self.runner()?.analyze(agent, job, settings.team_memory())?;
        Ok(to_json(&serde_json::json!({ "run": run })))
    }

    /// The reply to `POST /api/analysis/check`, which the page sends on each refresh: compares
    /// the analysis's `checked` with `HEAD` (`analysis::check_head`), and starts the forced
    /// update of the settings' Analyzer when the move needs one (`Runner::analyze_checked`).
    /// Answers what happened ([`CheckView`]); a start that is refused is an answer, not an
    /// error, since the page checks with the map closed too. The body is not read.
    fn check_analysis(&self) -> Result<Vec<u8>> {
        let repo = self.repo()?;
        let runner = self.runner()?;
        let view = match analysis::check_head(&repo)? {
            Verdict::Same => CheckView::Same,
            Verdict::Untouched => CheckView::Untouched,
            Verdict::Diverged => CheckView::Diverged,
            Verdict::Update {
                expected,
                head,
                planned,
            } => {
                let settings = self.current_settings();
                match settings.analyzer() {
                    Err(error) => CheckView::Blocked {
                        reason: error_chain(&error),
                    },
                    Ok(agent) => match runner.analyze_checked(
                        agent,
                        settings.team_memory(),
                        expected.as_ref(),
                        &head,
                        planned,
                    )? {
                        StartedUpdate::Started { run, commit } => {
                            CheckView::Updated { run, commit }
                        }
                        StartedUpdate::AlreadyChecked => CheckView::Same,
                        StartedUpdate::Blocked(reason) => CheckView::Blocked { reason },
                    },
                }
            }
        };
        Ok(to_json(&view))
    }

    /// The reply to `POST /api/analysis/start-over`: moves the analysis file to the Trash, one
    /// this build cannot read included, and answers `{"trashed": "<where it went>"}`, or
    /// `{"trashed": null}` when there was no file. Refused while an analysis of this window is
    /// under way: its merge would write a new file as this one goes.
    fn start_analysis_over(&self) -> Result<Vec<u8>> {
        // Snapshot repo and runner from the same project, then drop the read lock before disk I/O.
        let (repo, runner) = {
            let guard = self.project.read().unwrap_or_else(PoisonError::into_inner);
            let project = guard.as_ref().ok_or(Error::NoProject)?;
            (Arc::clone(&project.repo), Arc::clone(&project.runner))
        };
        if runner.analyzing() {
            return Err(Error::AnalysisRefused(
                "an analysis of this window is under way: wait for it, or stop it".to_owned(),
            ));
        }
        let trashed = analysis::start_over(&repo)?;
        Ok(to_json(&serde_json::json!({ "trashed": trashed })))
    }

    /// The reply to `DELETE /api/analysis?diagram=D3`: deletes the diagram, with the diagrams
    /// below its nodes, and returns the analysis left as `GET /api/analysis` does: placed, with
    /// the files not on the map.
    fn delete_diagram(&self, query: &str) -> Result<Vec<u8>> {
        let id = query_value(query, "diagram")
            .ok_or_else(|| Error::BadRequest("expected ?diagram=<diagram id>".to_owned()))?;
        let repo = self.repo()?;
        let left = analysis::delete(&repo, &id)?;
        Ok(to_json(&analysis::view(&repo, left)))
    }

    /// The reply to `POST /api/notes`: adds the note in the body, saves, and returns all
    /// notes.
    fn add_note(&self, body: &[u8]) -> Result<Vec<u8>> {
        let new: NewNote = parse_body(body, Error::InvalidNote)?;
        let review = notes::add(&*self.repo()?, new)?;
        Ok(to_json(&review))
    }

    /// The reply to `PUT /api/folders` `{"open": ["src"]}`: saves the folders open in the file
    /// list, and answers `{}`.
    fn save_folders(&self, body: &[u8]) -> Result<Vec<u8>> {
        let open: Folders = parse_body(body, Error::BadRequest)?;
        folders::save(&self.repo()?.folders_file(), open)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `PATCH /api/notes?id=N1` `{"body": "..."}`: replaces the note's text, saves,
    /// and returns all notes.
    fn edit_note(&self, id: &str, body: &[u8]) -> Result<Vec<u8>> {
        let edit: EditNote = parse_body(body, Error::InvalidNote)?;
        let review = notes::edit(&*self.repo()?, id, &edit.body)?;
        Ok(to_json(&review))
    }

    /// The reply to `DELETE /api/notes?id=N1`: removes the note, saves, and returns the
    /// other notes.
    fn delete_note(&self, id: &str) -> Result<Vec<u8>> {
        // Snapshot repo and runner from the same project, then drop the read lock before disk/git I/O.
        let (repo, runner) = {
            let guard = self.project.read().unwrap_or_else(PoisonError::into_inner);
            let project = guard.as_ref().ok_or(Error::NoProject)?;
            (Arc::clone(&project.repo), Arc::clone(&project.runner))
        };
        let review = notes::delete(&repo, id)?;
        // A run goes with its last note (docs/design.md, "Runs"). The note is deleted by now:
        // a log that cannot be removed is reported, and tried again at the next delete.
        runner.delete_runs_without_notes()?;
        Ok(to_json(&review))
    }

    /// The reply to `POST /api/notes/resolve?id=N1` and `POST /api/notes/reopen?id=N1`:
    /// changes the note's state with `change` (around `notes::resolve` or `notes::reopen`),
    /// and returns all notes.
    fn change_note(&self, query: &str, change: fn(&Project, &str) -> Result<Review>) -> Reply {
        let Some(id) = query_value(query, "id") else {
            return bad_request("expected ?id=<note id>");
        };
        let project = self
            .project
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        json_or_error(
            project
                .as_ref()
                .ok_or(Error::NoProject)
                .and_then(|project| change(project, &id))
                .map(|review| to_json(&review)),
        )
    }

    /// The replies to `/api/notes`: GET the notes, POST a new one, PATCH `?id=N1` its text, or
    /// DELETE `?id=N1`.
    fn respond_notes(&self, method: &Method, query: &str, body: &[u8]) -> Reply {
        let id = query_value(query, "id");
        match (method, id) {
            (&Method::GET, _) => json_or_error(self.notes()),
            (&Method::POST, _) => json_or_error(self.add_note(body)),
            (&Method::DELETE, Some(id)) => json_or_error(self.delete_note(&id)),
            (&Method::PATCH, Some(id)) => json_or_error(self.edit_note(&id, body)),
            (&Method::DELETE | &Method::PATCH, None) => bad_request("expected ?id=<note id>"),
            _ => not_allowed(
                "GET the notes, POST a new one, PATCH ?id=N1 its text, or DELETE ?id=N1",
            ),
        }
    }

    /// The replies to `/api/definitions` and `/api/definitions/refresh`.
    fn respond_definitions(&self, path: &str, method: &Method, query: &str) -> Reply {
        match path {
            "/api/definitions" => match *method {
                Method::GET => json_or_error(self.definitions(query)),
                _ => not_allowed("GET ?name=<name>&from=<path>"),
            },
            "/api/definitions/refresh" => match *method {
                Method::POST => json_or_error(self.refresh_definitions()),
                _ => not_allowed("POST with no body"),
            },
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    /// The replies to `/api/analysis` and the routes under it. [`Site::respond`] sends every
    /// path that starts with `/api/analysis` here, so a path that is none of these (such as
    /// `/api/analysisx`) is not found here. Takes the whole request so that dispatch stays one
    /// line: `respond` is at clippy's length limit.
    fn respond_analysis(&self, request: &Request<Vec<u8>>) -> Reply {
        let (method, body) = (request.method(), request.body());
        let query = request.uri().query().unwrap_or("");
        match request.uri().path() {
            "/api/analysis" => match *method {
                Method::GET => json_or_error(self.analysis(query)),
                Method::POST => json_or_error(self.start_analysis(body)),
                Method::DELETE => json_or_error(self.delete_diagram(query)),
                _ => not_allowed(
                    "GET, GET ?at=<commit>, POST {\"kind\": ...}, or DELETE ?diagram=<id>",
                ),
            },
            "/api/analysis/update" => match *method {
                Method::POST => json_or_error(self.update_analysis(body)),
                _ => not_allowed("POST {\"diagram\": ...}, or with no body for every diagram"),
            },
            "/api/analysis/start-over" => match *method {
                Method::POST => json_or_error(self.start_analysis_over()),
                _ => not_allowed("POST with no body"),
            },
            "/api/analysis/check" => match *method {
                Method::POST => json_or_error(self.check_analysis()),
                _ => not_allowed("POST with no body"),
            },
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    /// The replies to the routes under `/api/agents/`, as [`Site::respond`] gives them: a new
    /// agent scan, and the models agents offer.
    fn respond_agents(&self, path: &str, method: &Method, body: &[u8]) -> Reply {
        match path {
            "/api/agents/rescan" => match *method {
                Method::POST => {
                    self.agents.rescan();
                    // An agent found again may be another version, with other models.
                    self.models.rescan();
                    // hippius-mem is looked for on the same path. A set up under way checks
                    // it again when it ends.
                    match self.memory.check() {
                        Ok(()) | Err(Error::MemorySetupRunning) => {
                            reply(StatusCode::OK, "application/json", self.settings())
                        }
                        Err(error) => json_or_error(Err(error)),
                    }
                }
                _ => not_allowed("POST with no body"),
            },
            "/api/agents/models" => match *method {
                Method::POST => json_or_error(self.agent_models(body)),
                _ => not_allowed("POST {\"agents\": [...], \"customAgents\": [...]}"),
            },
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    /// The replies to `/api/memory` and the routes under it, as [`Site::respond`] gives them: a
    /// memory, a recall, Refresh, a draft and a remember, a new check of hippius-mem and a trial
    /// vault's set up. Each answers at once with what is known, and the work runs in the
    /// background (`memory.rs`, and the draft run, `run.rs`).
    /// Takes the whole request so that dispatch stays one line: `respond` is at clippy's
    /// length limit.
    fn respond_memory(&self, request: &Request<Vec<u8>>) -> Reply {
        let method = request.method();
        let query = request.uri().query().unwrap_or("");
        match request.uri().path() {
            "/api/memory" => match *method {
                Method::GET => json_or_error(self.memory_detail(query)),
                _ => not_allowed("GET ?id=mem_..."),
            },
            "/api/memory/recall" => match *method {
                Method::GET => json_or_error(self.memory_recall(query)),
                _ => not_allowed("GET ?path=<path>&node=<node label>, ?run=R1, or ?text=<query>"),
            },
            "/api/memory/draft" => match *method {
                Method::POST => json_or_error(self.memory_draft(request.body())),
                _ => not_allowed("POST {\"note\": \"N1\"}"),
            },
            "/api/memory/remember" => match *method {
                Method::POST => json_or_error(self.memory_remember(request.body())),
                _ => not_allowed(
                    "POST {\"note\": \"N1\", \"noteType\": ..., \"summary\": ..., \"body\": ..., \
                     \"scope\": \"repo\" or \"global\"}",
                ),
            },
            "/api/memory/refresh" => match *method {
                Method::POST => json_or_error(self.memory_refresh()),
                _ => not_allowed("POST with no body"),
            },
            "/api/memory/check" => match *method {
                Method::POST => json_or_error(self.memory.check().map(|()| self.settings())),
                _ => not_allowed("POST with no body"),
            },
            "/api/memory/setup" => match *method {
                Method::POST => json_or_error(self.memory.setup().map(|()| self.settings())),
                _ => not_allowed("POST with no body"),
            },
            "/api/memory/upgrade" => match *method {
                Method::POST => json_or_error(self.memory_upgrade_s3(request.body())),
                _ => not_allowed(
                    "POST {\"bucket\": ..., \"accessKeyId\": ..., \"secretAccessKey\": ...}",
                ),
            },
            "/api/memory/join" => match *method {
                Method::POST => json_or_error(self.memory_join_bundle(request.body())),
                _ => not_allowed("POST {\"bundle\": ...}"),
            },
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    /// The recall for the open file (`?path=`, with the selected node's label as `?node=`), for
    /// the thread that ends with a run (`?run=`): its prompt and changed paths, or for a text
    /// (`?text=`), a draft's summary, which the Remember dialog checks for duplicates.
    fn memory_recall(&self, query: &str) -> Result<Vec<u8>> {
        if let Some(text) = query_value(query, "text") {
            if text.trim().is_empty() {
                return Err(Error::BadRequest(
                    "expected ?text=<a query that is not blank>".to_owned(),
                ));
            }
            self.repo()?;
            return Ok(to_json(&self.memory.recall_run(text)?));
        }
        if let Some(run) = query_value(query, "run") {
            // The runner holds no lock once this returns: memory is never waited on with a
            // run's lock held.
            let text = self.runner()?.recall_query(&run)?;
            return Ok(to_json(&self.memory.recall_run(text)?));
        }
        let path = query_value(query, "path").ok_or_else(|| {
            Error::BadRequest(
                "expected ?path=<percent-encoded path>, ?run=<run id> or ?text=<query>".to_owned(),
            )
        })?;
        self.repo()?.check_path(&path)?;
        let node = query_value(query, "node").filter(|label| !label.is_empty());
        Ok(to_json(&self.memory.recall_file(&path, node.as_deref())?))
    }

    /// Memory `?id=` whole, with its history.
    fn memory_detail(&self, query: &str) -> Result<Vec<u8>> {
        self.repo()?;
        let id = query_value(query, "id")
            .ok_or_else(|| Error::BadRequest("expected ?id=mem_...".to_owned()))?;
        Ok(to_json(&self.memory.detail(MemoryId::parse(&id)?)?))
    }

    fn memory_refresh(&self) -> Result<Vec<u8>> {
        self.repo()?;
        Ok(to_json(&self.memory.refresh()?))
    }

    /// The reply to `POST /api/memory/draft` (docs/review-assist.md, "Postil writes memory"):
    /// starts a draft run in the thread the note was sent in ([`Runner::draft`]), with the
    /// settings' choice for the agent that ran its latest turn should a fresh one start; or,
    /// when the note was never sent or its thread ended, the note's text alone, and no run.
    fn memory_draft(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: DraftRequest = parse_body(body, Error::BadRequest)?;
        let repo = self.repo()?;
        let source = notes::draft_source(&repo, &request.note)?;
        let runner = self.runner()?;
        let tip = match &source.run {
            Some(run) => runner.thread_tip(run)?,
            None => None,
        };
        let Some(of) = tip else {
            return Ok(to_json(&DraftReply::Fallback { body: source.body }));
        };
        let agent = self
            .current_settings()
            .chosen_agent(&runner.agent_of(&of)?)?;
        let run = runner.draft(&of, agent, &source.quoted)?;
        Ok(to_json(&DraftReply::Run {
            run,
            fallback: source.body,
        }))
    }

    /// The reply to `POST /api/memory/remember`: the write, checked here and then queued for
    /// the memory thread ([`Memory::remember`]), as far as it is known.
    fn memory_remember(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: RememberRequest = parse_body(body, Error::BadRequest)?;
        let repo = self.repo()?;
        let job = request.into_job(&repo)?;
        Ok(to_json(&self.memory.remember(job)?))
    }

    fn memory_upgrade_s3(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: UpgradeS3Request = parse_body(body, Error::BadRequest)?;
        self.memory.upgrade_s3(request)?;
        Ok(self.settings())
    }

    fn memory_join_bundle(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: JoinBundleBody = parse_body(body, Error::BadRequest)?;
        self.memory.join_bundle(request.bundle)?;
        Ok(self.settings())
    }

    fn respond_hub_or_lane(&self, path: &str, method: &Method, query: &str, body: &[u8]) -> Reply {
        if path == "/api/hub" || path.starts_with("/api/hub/") {
            self.respond_hub(path, method, body)
        } else {
            self.respond_lane(path, method, query, body)
        }
    }

    /// Replies to `/api/hub` and `/api/hub/*`.
    fn respond_hub(&self, path: &str, method: &Method, body: &[u8]) -> Reply {
        match path {
            "/api/hub" => match *method {
                Method::GET => reply(
                    StatusCode::OK,
                    "application/json",
                    to_json(&self.hub.view()),
                ),
                _ => not_allowed("GET"),
            },
            "/api/hub/repos" => match *method {
                Method::POST => json_or_error(self.hub_repos(body)),
                _ => not_allowed(
                    "POST {\"action\": \"add\"|\"remove\"|\"select\"|\"selectAll\", ...}",
                ),
            },
            "/api/hub/sync" => match *method {
                Method::POST => {
                    self.hub.sync_all();
                    reply(
                        StatusCode::OK,
                        "application/json",
                        to_json(&self.hub.view()),
                    )
                }
                _ => not_allowed("POST with no body"),
            },
            "/api/hub/evaluate" => match *method {
                Method::POST => json_or_error(self.hub_evaluate(body)),
                _ => not_allowed("POST {\"repo\": ..., \"kind\": ..., \"number\": ...}"),
            },
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    fn hub_repos(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: HubRepoRequest = parse_body(body, Error::BadRequest)?;
        match request.action.as_str() {
            "add" => {
                let path = request
                    .path
                    .as_deref()
                    .filter(|p| !p.trim().is_empty())
                    .ok_or_else(|| Error::BadRequest("expected path to monitor".to_owned()))?;
                self.hub.add_repo(Path::new(path))?;
            }
            "remove" => {
                let path = request
                    .path
                    .as_deref()
                    .ok_or_else(|| Error::BadRequest("expected path to remove".to_owned()))?;
                self.hub.remove_repo(path);
            }
            "select" => {
                let path = request
                    .path
                    .as_deref()
                    .ok_or_else(|| Error::BadRequest("expected path to select".to_owned()))?;
                let selected = request.selected.unwrap_or(true);
                self.hub.set_selected(path, selected);
            }
            "selectAll" => {
                let selected = request.selected.unwrap_or(true);
                self.hub.set_all_selected(selected);
            }
            other => {
                return Err(Error::BadRequest(format!(
                    "unknown hub action {other:?}: expected add, remove, select, or selectAll"
                )));
            }
        }
        Ok(to_json(&self.hub.view()))
    }

    fn hub_evaluate(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: HubEvaluateRequest = parse_body(body, Error::BadRequest)?;
        let item = self
            .hub
            .evaluate_item(&request.repo, request.kind, request.number)?;
        Ok(to_json(&item))
    }

    /// Replies to `/api/lanes/preview`, `/api/pipelines`, `/api/lane/*`, and `/api/pr/submit-review`.
    fn respond_lane(&self, path: &str, method: &Method, query: &str, body: &[u8]) -> Reply {
        match path {
            "/api/lanes/preview" => match *method {
                Method::GET => json_or_error(self.lane_preview(query)),
                _ => not_allowed("GET ?lane=easy|middle|hard&kind=issue|pullRequest"),
            },
            "/api/pipelines" => match *method {
                Method::GET => {
                    json_or_error(self.runner().map(|runner| to_json(&runner.pipelines())))
                }
                _ => not_allowed("GET"),
            },
            "/api/lane/start" => match *method {
                Method::POST => json_or_error(self.start_lane(body)),
                _ => not_allowed(
                    "POST {\"lane\": ..., \"kind\": ..., \"number\": ..., \"title\": ...}",
                ),
            },
            "/api/lane/gate/approve" => match *method {
                Method::POST => json_or_error(self.approve_lane_gate(body)),
                _ => not_allowed("POST {\"pipeline\": \"R1\", ...}"),
            },
            "/api/lane/gate/revise" => match *method {
                Method::POST => json_or_error(self.revise_lane_gate(body)),
                _ => not_allowed("POST {\"pipeline\": \"R1\", \"feedback\": ...}"),
            },
            "/api/pr/submit-review" => match *method {
                Method::POST => json_or_error(self.submit_pr_review(body)),
                _ => not_allowed("POST {\"number\": ..., \"verdict\": ..., ...}"),
            },
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    fn lane_preview(&self, query: &str) -> Result<Vec<u8>> {
        let lane = match query_value(query, "lane").as_deref() {
            Some("easy") => Lane::Easy,
            Some("middle") => Lane::Middle,
            Some("hard") => Lane::Hard,
            other => {
                return Err(Error::BadRequest(format!(
                    "lane={other:?}: expected easy, middle, or hard"
                )));
            }
        };
        let kind = match query_value(query, "kind").as_deref() {
            Some("issue") => WorkKind::Issue,
            Some("pullRequest") => WorkKind::PullRequest,
            other => {
                return Err(Error::BadRequest(format!(
                    "kind={other:?}: expected issue or pullRequest"
                )));
            }
        };
        let settings = self.current_settings();
        let stages = lane::resolve_stages(&settings, lane, kind)?;
        Ok(to_json(&serde_json::json!({
            "lane": lane,
            "kind": kind,
            "hardHumanGate": settings.lanes().hard_human_gate,
            "stages": stages,
        })))
    }

    fn start_lane(&self, body: &[u8]) -> Result<Vec<u8>> {
        let mut request: StartPipelineRequest = parse_body(body, Error::BadRequest)?;
        // Hydrate body/labels/memories from Hub cache when the caller passes only minimal fields.
        if request.body.is_empty()
            && let Ok(repo) = self.repo()
            && let Some(cached) =
                self.hub
                    .find_item(&repo.root().to_string_lossy(), request.kind, request.number)
        {
            request.body = cached.body;
            if request.url.is_empty() {
                request.url = cached.url;
            }
            if request.labels.is_empty() {
                request.labels = cached.labels;
            }
            if request.memories.is_empty() {
                request.memories = cached.memories;
            }
            if request.base_branch.is_none() {
                request.base_branch = cached.base_ref;
            }
            if request.head_branch.is_none() {
                request.head_branch = cached.head_ref;
            }
        }
        let settings = self.current_settings();
        let pipeline = self.runner()?.start_pipeline(request, &settings)?;
        Ok(to_json(&pipeline))
    }

    fn approve_lane_gate(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: ApproveGateRequest = parse_body(body, Error::BadRequest)?;
        let pipeline = self.runner()?.approve_pipeline_gate(request)?;
        Ok(to_json(&pipeline))
    }

    fn revise_lane_gate(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: ReviseGateRequest = parse_body(body, Error::BadRequest)?;
        let pipeline = self.runner()?.revise_pipeline_gate(&request)?;
        Ok(to_json(&pipeline))
    }

    fn submit_pr_review(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: SubmitPrReviewRequest = parse_body(body, Error::BadRequest)?;
        self.runner()?.submit_pr_review(&request)?;
        Ok(b"{}".to_vec())
    }

    /// The replies to `/api/thread` and the routes under it, as [`Site::respond`] gives them:
    /// a thread's record, and ending it.
    fn respond_thread(&self, path: &str, method: &Method, query: &str, body: &[u8]) -> Reply {
        match path {
            "/api/thread" => match *method {
                Method::GET => match query_value(query, "id") {
                    Some(id) => json_or_error(
                        self.runner()
                            .and_then(|runner| runner.thread_view(&id))
                            .map(|thread| to_json(&thread)),
                    ),
                    None => bad_request("expected ?id=<the thread's first run>"),
                },
                _ => not_allowed("GET ?id=R1"),
            },
            "/api/thread/apply" => match *method {
                Method::POST => json_or_error(self.end_thread(body, Ending::Apply)),
                _ => not_allowed("POST"),
            },
            "/api/thread/discard" => match *method {
                Method::POST => json_or_error(self.end_thread(body, Ending::Discard)),
                _ => not_allowed("POST"),
            },
            "/api/thread/pr" => match *method {
                Method::POST => json_or_error(self.open_pull_request(body)),
                _ => not_allowed("POST"),
            },
            "/api/thread/pr/state" => match *method {
                Method::POST => json_or_error(self.check_pull_request(body)),
                _ => not_allowed("POST"),
            },
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    /// The replies to `/api/run` and the routes under it, as [`Site::respond`] gives them.
    fn respond_run(&self, path: &str, method: &Method, query: &str, body: &[u8]) -> Reply {
        match path {
            "/api/run" => match *method {
                Method::GET => json_or_error(self.run_view(query)),
                Method::POST => json_or_error(self.send_run(body)),
                _ => not_allowed(
                    "GET the latest run (?after=<entries seen>) or one run (?id=R1), or POST a \
                     new one",
                ),
            },
            "/api/run/preview" => match *method {
                Method::GET => match query_value(query, "notes") {
                    Some(ids) => json_or_error(self.preview_run(&ids)),
                    None => bad_request("expected ?notes=N1,N2"),
                },
                _ => not_allowed("GET ?notes=N1,N2"),
            },
            "/api/run/changes" => match *method {
                Method::GET => json_or_error(self.changes(query)),
                _ => not_allowed("GET"),
            },
            "/api/run/cancel" => match *method {
                Method::POST => json_or_error(self.cancel_run(body)),
                _ => not_allowed("POST"),
            },
            "/api/run/permission" => match *method {
                Method::POST => json_or_error(self.answer_permission(body)),
                _ => not_allowed("POST"),
            },
            "/api/run/question" => match *method {
                Method::POST => json_or_error(self.answer_question(body)),
                _ => not_allowed("POST"),
            },
            "/api/run/review" => match *method {
                Method::POST => json_or_error(self.review_run(body)),
                _ => not_allowed("POST"),
            },
            "/api/run/follow" => match *method {
                Method::POST => json_or_error(self.follow_up(body)),
                _ => not_allowed("POST"),
            },
            "/api/run/ask" => match *method {
                Method::POST => json_or_error(self.ask(body)),
                _ => not_allowed("POST {\"note\": {...}}"),
            },
            "/api/run/uncommitted" => match *method {
                Method::GET => json_or_error(self.uncommitted(query)),
                _ => not_allowed("GET, with ?file=<path> for a file"),
            },
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    /// The reply to `GET /api/layout`: the pane widths and their limits.
    fn layout(&self) -> Vec<u8> {
        let layout = self
            .layout_file
            .as_deref()
            .map_or_else(Layout::default, layout::load);
        to_json(&LayoutView {
            layout,
            defaults: Layout::default(),
            limits: LayoutLimits {
                sidebar: [*layout::SIDEBAR.start(), *layout::SIDEBAR.end()],
                tray: [*layout::TRAY.start(), *layout::TRAY.end()],
                map: [*layout::MAP.start(), *layout::MAP.end()],
            },
        })
    }

    /// The reply to `PUT /api/layout`: saves the widths in the body and answers `{}`; the
    /// page already holds what it sent.
    fn save_layout(&self, body: &[u8]) -> Result<Vec<u8>> {
        let layout: RawLayout = serde_json::from_slice(body)
            .map_err(|error| Error::BadRequest(format!("unreadable layout: {error}")))?;
        let file = self.layout_file.as_deref().ok_or(Error::NoHome)?;
        // Checked here, not by serde: a serde error would carry a line and column and the
        // "unreadable layout" prefix, and the page shows this message as it is.
        let layout = Layout::try_from(layout).map_err(Error::BadRequest)?;
        layout::save(file, &layout)?;
        Ok(b"{}".to_vec())
    }

    /// The open project's runner.
    fn runner(&self) -> Result<Arc<Runner>> {
        self.project
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|project| Arc::clone(&project.runner))
            .ok_or(Error::NoProject)
    }

    /// The reply to `GET /api/runs`: every run's id, and the live ones with their threads.
    fn runs(&self) -> Result<Vec<u8>> {
        let runner = self.runner()?;
        let ids = runner.ids()?;
        let live: Vec<_> = runner
            .live()
            .into_iter()
            .map(|(id, thread)| serde_json::json!({ "id": id, "thread": thread }))
            .collect();
        Ok(to_json(&serde_json::json!({ "runs": ids, "live": live })))
    }

    /// The reply to `GET /api/run`: with `?id=R2`, that run, else the latest run, or `null`
    /// when the project has none; either with its entries from number `?after=` on, without
    /// its notes and prompt unless that is 0.
    fn run_view(&self, query: &str) -> Result<Vec<u8>> {
        let runner = self.runner()?;
        let after = after_of(query)?;
        match query_value(query, "id") {
            Some(id) => Ok(to_json(&runner.view_of(&id, after)?)),
            None => Ok(to_json(&runner.view(after)?)),
        }
    }

    /// The reply to `POST /api/run/cancel`: stops the run in the body, and answers `{}`.
    fn cancel_run(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: CancelRun = parse_body(body, Error::BadRequest)?;
        self.runner()?.cancel(&request.run)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `GET /api/run/changes?from=R1&to=R3`: what runs `from` to `to` changed
    /// (`Runner::changes`). `to` is `from` when left out.
    fn changes(&self, query: &str) -> Result<Vec<u8>> {
        let from = query_value(query, "from")
            .ok_or_else(|| Error::BadRequest("expected ?from=<run id>".to_owned()))?;
        let to = query_value(query, "to").unwrap_or_else(|| from.clone());
        Ok(to_json(&self.runner()?.changes(&from, &to)?))
    }

    /// The reply to `POST /api/run`: sends the notes in the body to the agent, and answers
    /// `{}`. The page fetches the run (`GET /api/run`), as after every change.
    fn send_run(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: SendRun = parse_body(body, Error::BadRequest)?;
        let settings = self.current_settings();
        let agent = match &request.agent {
            Some(name) => settings.chosen_agent(name)?,
            None => settings.default_agent()?,
        };
        self.runner()?
            .send(agent, &request.notes, settings.team_memory())?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/run/ask` `{"note": {...}}`: asks the question in the body, a new
    /// note as `POST /api/notes` takes one, of the settings' agent, and returns every note, the
    /// ask last (`Runner::ask`). The page fetches the run, as after a send.
    fn ask(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: AskRun = parse_body(body, Error::InvalidNote)?;
        let settings = self.current_settings();
        let agent = settings.default_agent()?;
        let review = self
            .runner()?
            .ask(agent, request.note, settings.team_memory())?;
        Ok(to_json(&review))
    }

    /// The reply to `GET /api/run/uncommitted`, with `?file=<path>` for an ask on a file: the
    /// engineer's changes since the last commit, which a new thread does not see, and whether
    /// the file is among them, as the send step's preview says for its notes.
    fn uncommitted(&self, query: &str) -> Result<Vec<u8>> {
        let files: Vec<String> = query_value(query, "file").into_iter().collect();
        // Answers no-project as the other run routes do, before reading the working tree.
        self.runner()?;
        Ok(to_json(&self.uncommitted_among(&files)?))
    }

    /// The reply to `GET /api/run/preview?notes=N1,N2`.
    fn preview_run(&self, ids: &str) -> Result<Vec<u8>> {
        let ids: Vec<String> = ids.split(',').map(str::to_owned).collect();
        let memory = self.current_settings().team_memory();
        let prompt = self.runner()?.preview(&ids, memory)?;
        let uncommitted = self.uncommitted_among(&prompt.files)?;
        Ok(to_json(&PreviewView {
            prompt: prompt.text,
            files: prompt.files,
            uncommitted,
        }))
    }

    /// The engineer's changes since the last commit, and which of `files` are among them.
    fn uncommitted_among(&self, files: &[String]) -> Result<UncommittedView> {
        // One `git status` each time the send step reads the prompt or an ask opens, on the
        // request thread: as long as `git status` in a terminal ([`Repo::uncommitted`]). One
        // that fails costs the warning, not the prompt or the question.
        let (entries, uncommitted_error) = match self.repo()?.uncommitted() {
            Ok(entries) => (Some(entries), None),
            Err(error) => (None, Some(error_chain(&error))),
        };
        // A rename's source too: a noted file renamed away is in the agent's copy as it was.
        let uncommitted_paths: Vec<&String> = entries
            .iter()
            .flatten()
            .flat_map(|(path, source)| std::iter::once(path).chain(source))
            .collect();
        // A path ending in `/` is an untracked folder: every noted file in it is one the
        // agent's copy lacks.
        let is_uncommitted = |file: &String| {
            uncommitted_paths
                .iter()
                .any(|path| *path == file || (path.ends_with('/') && file.starts_with(*path)))
        };
        let uncommitted_noted = files
            .iter()
            .filter(|file| is_uncommitted(file))
            .cloned()
            .collect();
        Ok(UncommittedView {
            uncommitted: entries.as_ref().map(Vec::len),
            uncommitted_noted,
            uncommitted_error,
        })
    }

    /// The reply to `POST /api/run/permission`.
    fn answer_permission(&self, body: &[u8]) -> Result<Vec<u8>> {
        let answer: AnswerPermission = parse_body(body, Error::BadRequest)?;
        self.runner()?
            .answer(&answer.run, answer.request, &answer.option)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/run/review`: starts the settings' reviewers on the run, in
    /// order, and answers `{}`.
    fn review_run(&self, body: &[u8]) -> Result<Vec<u8>> {
        let review: ReviewRun = parse_body(body, Error::BadRequest)?;
        // Loaded once, so every reviewer is checked against the same revision of the file.
        let settings = self.current_settings();
        let reviewers = settings.reviewer_choices()?;
        self.runner()?
            .review(&review.of, reviewers, settings.team_memory())?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/run/follow`: sends the engineer's follow-up to the agent that
    /// ran the run it follows, and answers `{}`. The settings' choice for that agent is used
    /// only when a fresh agent starts; a kept one goes on with its own (`Runner::follow_up`).
    fn follow_up(&self, body: &[u8]) -> Result<Vec<u8>> {
        let follow: FollowRun = parse_body(body, Error::BadRequest)?;
        let runner = self.runner()?;
        let settings = self.current_settings();
        let agent = settings.chosen_agent(&runner.agent_of(&follow.of)?)?;
        runner.follow_up(&follow.of, agent, &follow.text, settings.team_memory())?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/thread/apply` and `POST /api/thread/discard`: ends the thread
    /// in the body as `ending` says, and answers `{}`.
    fn end_thread(&self, body: &[u8], ending: Ending) -> Result<Vec<u8>> {
        let request: ThreadBody = parse_body(body, Error::BadRequest)?;
        self.runner()?.end_thread(&request.thread, ending)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/thread/pr`: starts opening the thread's pull request, and
    /// answers `{}`. The work goes on after the reply: `GET api/thread` shows it.
    fn open_pull_request(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: ThreadBody = parse_body(body, Error::BadRequest)?;
        self.runner()?.open_pull_request(&request.thread)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/thread/pr/state`: starts reading the state of the thread's pull
    /// request again, and answers `{}`, as [`Site::open_pull_request`] does.
    fn check_pull_request(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: ThreadBody = parse_body(body, Error::BadRequest)?;
        self.runner()?.check_pull_request(&request.thread)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/run/question`.
    fn answer_question(&self, body: &[u8]) -> Result<Vec<u8>> {
        let answer: AnswerQuestion = parse_body(body, Error::BadRequest)?;
        self.runner()?
            .reply(&answer.run, answer.request, answer.content)?;
        Ok(b"{}".to_vec())
    }

    /// Loads `settings.json` with an `(mtime, len)` cache so repeated polls skip re-parsing.
    fn load_settings_cached(&self, file: &Path) -> settings::Loaded {
        let stamp = match std::fs::metadata(file) {
            Ok(meta) => match meta.modified() {
                Ok(modified) => Some((modified, meta.len())),
                Err(_) => return settings::load(file),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return settings::load(file),
        };
        {
            let guard = self
                .settings_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(cached) = guard.as_ref()
                && cached.stamp == stamp
            {
                return cached.loaded.clone();
            }
        }
        let loaded = settings::load(file);
        *self
            .settings_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(SettingsCache {
            stamp,
            loaded: loaded.clone(),
        });
        loaded
    }

    fn invalidate_settings_cache(&self) {
        *self
            .settings_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The settings in effect: the file's, or the defaults when it cannot be used or there is
    /// no configuration directory.
    fn current_settings(&self) -> Settings {
        self.settings_file
            .as_deref()
            .map_or_else(Settings::default, |file| {
                self.load_settings_cached(file).settings
            })
    }

    /// The reply to `GET /api/settings`, and to every request that changes them: the
    /// settings in effect, why the file is not used if it is not, and the agents.
    fn settings(&self) -> Vec<u8> {
        let (loaded, file) = match &self.settings_file {
            Some(file) => (
                self.load_settings_cached(file),
                Some(file.to_string_lossy().into_owned()),
            ),
            None => (
                settings::Loaded {
                    settings: Settings::default(),
                    problem: Some(format!("settings are off: {}", Error::NoHome)),
                    revision: String::new(),
                    first_run: false,
                },
                None,
            ),
        };
        let search_path = self.agents.path();
        let agents = search_path
            .as_deref()
            .map(|path| agent_views(&loaded.settings, &self.agents, path));
        let tools = search_path
            .as_deref()
            .map(|path| tools_view(&self.agents, path));
        to_json(&SettingsView {
            settings: &loaded.settings,
            problem: loaded.problem,
            revision: loaded.revision,
            file,
            first_run: loaded.first_run,
            agents,
            memory: MemoryView::from(self.memory.status()),
            tools,
            font_sizes: [*settings::FONT_SIZES.start(), *settings::FONT_SIZES.end()],
            tab_widths: settings::TAB_WIDTHS,
        })
    }

    /// The reply to `PUT /api/settings`: checks and saves the settings in the body, if the
    /// file is still at the revision the body names.
    fn save_settings(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: SaveSettings = parse_body(body, Error::InvalidSettings)?;
        let file = self.settings_file.as_deref().ok_or(Error::NoHome)?;
        // Checked here, not by serde, for the same reason as `save_layout`.
        let settings = Settings::try_from(request.settings).map_err(Error::InvalidSettings)?;
        // Only turning it on is refused, so a hippius-mem removed later does not block every
        // other change. The state is the latest check's, never waited for: a check can take
        // seconds, and the dialog offers the switch only once it is ready.
        if settings.team_memory() == TeamMemory::On
            && self.current_settings().team_memory() == TeamMemory::Off
        {
            let state = self.memory.status().state;
            if let Some(problem) = memory::not_ready(state.as_ref()) {
                return Err(Error::MemoryNotReady(problem));
            }
        }
        settings::save(file, &settings, &request.revision)?;
        self.invalidate_settings_cache();
        Ok(self.settings())
    }

    /// The reply to `POST /api/settings/reset`: writes the defaults over the file.
    fn reset_settings(&self) -> Result<Vec<u8>> {
        settings::reset(self.settings_file.as_deref().ok_or(Error::NoHome)?)?;
        self.invalidate_settings_cache();
        Ok(self.settings())
    }

    /// The reply to `POST /api/agents/models`: what each agent in the body reports it offers,
    /// probing the ones not asked yet in the background (`models.rs`). Never waits for a
    /// probe. A built-in agent is reported loading while the agent scan runs, and failed when
    /// it is not installed, without starting it.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when the body names an agent that is neither built in nor a
    /// custom agent of the body or the settings, or a custom agent whose command breaks the
    /// settings' rule.
    fn agent_models(&self, body: &[u8]) -> Result<Vec<u8>> {
        let request: ModelsRequest = parse_body(body, Error::BadRequest)?;
        let settings = self.current_settings();
        let mut models = BTreeMap::new();
        let mut probed: Vec<(String, String)> = Vec::new();
        for name in request.agents {
            if let Some(agent) = BUILT_IN.iter().find(|agent| agent.name == name) {
                let report = match self.agents.find_program(agent.program) {
                    agents::ProgramLookup::Scanning => ModelReport::Loading,
                    agents::ProgramLookup::Missing => ModelReport::Failed {
                        error: format!("{name} is not installed"),
                    },
                    agents::ProgramLookup::Found(_) => {
                        probed.push((name, agent.command.to_owned()));
                        continue;
                    }
                };
                models.insert(name, report);
                continue;
            }
            let draft = request
                .custom_agents
                .iter()
                .find(|agent| agent.name == name)
                .map(|agent| agent.command.as_str());
            let command = draft
                .or_else(|| settings.custom_command(&name))
                .ok_or_else(|| Error::BadRequest(format!("there is no agent called {name:?}")))?
                .to_owned();
            settings::check_agent_command(&name, &command).map_err(Error::BadRequest)?;
            probed.push((name, command));
        }
        let commands: Vec<&str> = probed.iter().map(|(_, command)| command.as_str()).collect();
        let reports = self.models.query(&commands);
        models.extend(probed.into_iter().map(|(name, _)| name).zip(reports));
        let loading = models
            .values()
            .any(|report| matches!(report, ModelReport::Loading));
        Ok(to_json(&ModelsView { loading, models }))
    }

    /// The reply to `GET /api/usage` and `POST /api/usage/refresh`.
    fn usage_view(&self, query: &str, fetch: QuotaFetch) -> Result<Vec<u8>> {
        let (has_project, project_name, project_root) = {
            let guard = self.project.read().unwrap_or_else(PoisonError::into_inner);
            match guard.as_ref() {
                Some(proj) => (
                    true,
                    Some(proj.repo.name()),
                    Some(proj.repo.root().to_string_lossy().into_owned()),
                ),
                None => (false, None, None),
            }
        };

        let requested_scope = match query_value(query, "scope") {
            Some(value) => Scope::parse(&value).ok_or_else(|| {
                Error::BadRequest(format!("scope={value:?}: expected project or all"))
            })?,
            None => Scope::Project,
        };
        // With no project open, "this project" selects nothing: report every project.
        let scope = if has_project {
            requested_scope
        } else {
            Scope::All
        };
        let range = match query_value(query, "range") {
            Some(value) => Range::parse(&value).ok_or_else(|| {
                Error::BadRequest(format!("range={value:?}: expected 24h, 7d, 30d or all"))
            })?,
            None => Range::SevenDays,
        };

        let records = match self.usage_file.as_deref() {
            Some(file) => self
                .read_ledger_cached(file)
                .map_err(|source| Error::ReadUsage {
                    path: file.to_owned(),
                    source,
                })?,
            None => Arc::from([]),
        };
        let settings = self.current_settings();
        let remap_legacy_gemini = !settings.custom_agents().iter().any(|agent| {
            agent.name.eq_ignore_ascii_case("Gemini")
                || agent.name.eq_ignore_ascii_case("Antigravity")
        });
        let (cli_records, scanning_sessions) = self.sessions.query(fetch);
        let only_repo = match scope {
            Scope::Project => project_root.as_deref(),
            Scope::All => None,
        };
        let (summary, mut activities) = ledger::aggregate(
            records.iter().chain(cli_records.iter()),
            only_repo,
            range,
            now_secs(),
            remap_legacy_gemini,
        );
        let ledger_file = self
            .usage_file
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());

        let Some(search_path) = self.agents.path() else {
            return Ok(to_json(&UsageView {
                scope,
                has_project,
                project_name,
                range,
                scanning: true,
                polling: scanning_sessions,
                ledger_file,
                summary,
                agents: None,
            }));
        };

        let agents = self.usage_agents(&settings, &search_path, &mut activities, fetch);
        let polling = scanning_sessions
            || agents
                .iter()
                .any(|agent| agent.quota.status == QuotaStatus::Loading);

        Ok(to_json(&UsageView {
            scope,
            has_project,
            project_name,
            range,
            scanning: false,
            polling,
            ledger_file,
            summary,
            agents: Some(agents),
        }))
    }

    /// Reads `usage.jsonl` with an `(dev, ino, parsed_offset, mtime)` cache and safe
    /// incremental tailing so repeated usage polls and appended runs avoid re-parsing the full
    /// history without duplicating records or skipping partial lines.
    fn read_ledger_cached(&self, file: &Path) -> std::io::Result<Arc<[ledger::LedgerRecord]>> {
        use std::os::unix::fs::MetadataExt;
        let meta = match std::fs::metadata(file) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                *self
                    .ledger_cache
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                return Ok(Arc::from([]));
            }
            Err(error) => return Err(error),
        };
        let modified = meta.modified().ok();
        let len = meta.len();
        let dev = meta.dev();
        let ino = meta.ino();
        let tail_base = {
            let guard = self
                .ledger_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(cached) = guard.as_ref()
                && cached.dev == dev
                && cached.ino == ino
            {
                if cached.parsed_offset == len && cached.modified == modified {
                    return Ok(Arc::clone(&cached.records));
                }
                if len >= cached.parsed_offset {
                    Some((cached.parsed_offset, Arc::clone(&cached.records)))
                } else {
                    None
                }
            } else {
                None
            }
        };
        let (records, parsed_offset, chunk_modified, chunk_dev, chunk_ino) =
            if let Some((offset, prev)) = tail_base
                && let Some(chunk) = ledger::read_chunk(file, offset, Some((dev, ino)))?
            {
                let mut combined = Vec::with_capacity(prev.len() + chunk.records.len());
                combined.extend_from_slice(&prev);
                combined.extend(chunk.records);
                (
                    Arc::from(combined),
                    chunk.parsed_offset,
                    chunk.modified,
                    chunk.dev,
                    chunk.ino,
                )
            } else {
                let chunk = match ledger::read_chunk(file, 0, None) {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => unreachable!("read_chunk with expected_id=None never returns None"),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        *self
                            .ledger_cache
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = None;
                        return Ok(Arc::from([]));
                    }
                    Err(error) => return Err(error),
                };
                (
                    Arc::from(chunk.records),
                    chunk.parsed_offset,
                    chunk.modified,
                    chunk.dev,
                    chunk.ino,
                )
            };
        *self
            .ledger_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(LedgerCache {
            modified: chunk_modified,
            parsed_offset,
            dev: chunk_dev,
            ino: chunk_ino,
            records: Arc::clone(&records),
        });
        Ok(records)
    }

    fn usage_agents(
        &self,
        settings: &Settings,
        search_path: &OsStr,
        activities: &mut BTreeMap<String, AgentActivity>,
        fetch: QuotaFetch,
    ) -> Vec<UsageAgentView> {
        let find = |program: &str| {
            find_cached_program(&self.agents, program, search_path)
                .map(|path| path.to_string_lossy().into_owned())
        };
        let mut entries: Vec<(String, bool, Option<String>)> = Vec::new();
        for agent in BUILT_IN {
            let found = find(agent.program);
            if found.is_some() || activities.contains_key(agent.name) {
                entries.push((agent.name.to_owned(), true, found));
            }
        }
        for agent in settings.custom_agents() {
            let found = agents::program_of(&agent.command).and_then(find);
            if found.is_some() || activities.contains_key(&agent.name) {
                entries.push((agent.name.clone(), false, found));
            }
        }
        // Any historical agent removed from settings that still has activity in this window.
        for name in activities.keys() {
            if !entries.iter().any(|(existing, _, _)| existing == name) {
                entries.push((name.clone(), false, None));
            }
        }

        // Only an installed built-in agent has a provider quota source to probe.
        let probe_names: Vec<&str> = entries
            .iter()
            .filter(|(_, built_in, found)| *built_in && found.is_some())
            .map(|(name, _, _)| name.as_str())
            .collect();
        let mut quotas = self.quotas.query(&probe_names, search_path, fetch);

        entries
            .into_iter()
            .map(|(name, built_in, found)| {
                let enabled = settings.is_enabled(&name);
                let quota = match quotas.remove(&name) {
                    Some(quota) => quota,
                    None if built_in => QuotaReport::unavailable("Not installed on PATH"),
                    None => QuotaReport::unavailable("Custom agent — no provider quota source"),
                };
                let activity = activities.remove(&name).unwrap_or_default();
                UsageAgentView {
                    name,
                    built_in,
                    found,
                    enabled,
                    quota,
                    activity,
                }
            })
            .collect()
    }
}

impl std::fmt::Debug for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `notify` is left out: it is a closure, which has no `Debug`.
        f.debug_struct("Site")
            .field("project", &self.project)
            .field("highlighter", &self.highlighter)
            .field("bench", &self.bench)
            .field("recent_file", &self.recent_file)
            .field("settings_file", &self.settings_file)
            .field("layout_file", &self.layout_file)
            .field("usage_file", &self.usage_file)
            .field("agents", &self.agents)
            .field("quotas", &self.quotas)
            .field("sessions", &self.sessions)
            .field("models", &self.models)
            .field("memory", &self.memory)
            .finish_non_exhaustive()
    }
}

// The request thread drops the site on `Job::Close`, while the event loop still runs. The
// event loop exits on `AppEvent::SiteClosed`, or at `QUIT_WAIT` without it. A runner whose
// agent is still starting is also held by its start thread, so dropping the project alone
// would not stop that agent: the site stops it here, so a quit that closes the site in time
// leaves no agent behind.
impl Drop for Site {
    fn drop(&mut self) {
        if let Some(project) = self
            .project
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            // The index first, as in `open`: telling it to stop waits for nothing.
            project.definitions.close();
            project.runner.close();
        }
        // A model probe's agent would otherwise run on until its timeout.
        self.models.close();
        // The memory server, a process of its own, would otherwise outlive the window.
        self.memory.close();
    }
}

/// Looks up `program` in `agents`'s per-scan cache, falling back to the snapshotted
/// `search_path` if a concurrent `rescan()` started mid-request.
fn find_cached_program(
    agents: &agents::Scanner,
    program: &str,
    search_path: &OsStr,
) -> Option<PathBuf> {
    match agents.find_program(program) {
        agents::ProgramLookup::Found(path) => Some(path),
        agents::ProgramLookup::Missing => None,
        agents::ProgramLookup::Scanning => agents::find_program(program, search_path),
    }
}

/// The built-in agents, then the custom ones, each with where its program was found on
/// `agents`'s cached search path.
fn agent_views<'a>(
    settings: &'a Settings,
    agents: &agents::Scanner,
    search_path: &OsStr,
) -> Vec<AgentView<'a>> {
    let found = |program: Option<&str>| {
        program
            .and_then(|program| find_cached_program(agents, program, search_path))
            .map(|path| path.to_string_lossy().into_owned())
    };
    let built_in = BUILT_IN.iter().map(|agent| AgentView {
        name: agent.name,
        command: agent.command,
        built_in: true,
        found: found(Some(agent.program)),
    });
    let custom = settings.custom_agents().iter().map(|agent| AgentView {
        name: &agent.name,
        command: &agent.command,
        built_in: false,
        found: found(agents::program_of(&agent.command)),
    });
    built_in.chain(custom).collect()
}

/// Where external tools Postil relies on live on `agents`'s cached search path: `git` for
/// repositories, `npx` for the Claude and Codex adapters, and `gh` for optional draft pull
/// requests.
fn tools_view(agents: &agents::Scanner, search_path: &OsStr) -> ToolsView {
    let find = |program| {
        find_cached_program(agents, program, search_path)
            .map(|path| path.to_string_lossy().into_owned())
    };
    ToolsView {
        git: find("git"),
        npx: find("npx"),
        gh: find("gh"),
    }
}

/// The `after` of `GET /api/run?after=N`: the number of entries the page has; 0 without it.
fn after_of(query: &str) -> Result<usize> {
    query_value(query, "after").map_or(Ok(0), |after| {
        after.parse().map_err(|_| {
            Error::BadRequest(format!("after={after:?}: expected a number of entries"))
        })
    })
}

fn is_hub_or_lane_path(path: &str) -> bool {
    path == "/api/hub"
        || path.starts_with("/api/hub/")
        || path == "/api/pipelines"
        || path.starts_with("/api/lanes/")
        || path.starts_with("/api/lane/")
        || path == "/api/pr/submit-review"
}

fn to_json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect(
        "the reply types are plain structs of strings, numbers, and sequences, which always \
         serialize",
    )
}

/// The JSON request body `body` as a `T`; when it does not parse, `error` of "unreadable
/// request" and serde's reason, which names the line and column.
fn parse_body<T: DeserializeOwned>(body: &[u8], error: fn(String) -> Error) -> Result<T> {
    serde_json::from_slice(body).map_err(|problem| error(format!("unreadable request: {problem}")))
}

fn json_or_error(body: Result<Vec<u8>>) -> Reply {
    match body {
        Ok(body) => reply(StatusCode::OK, "application/json", body),
        Err(error) => {
            // Every variant is listed so that a new one is a decision, not a silent 500.
            let status = match error {
                Error::OutsideRepository { .. }
                | Error::InvalidNote(_)
                | Error::BadRequest(_)
                | Error::NotAFolder { .. }
                | Error::TooBroad { .. }
                | Error::InvalidSettings(_) => StatusCode::BAD_REQUEST,
                Error::NoteTextChanged { .. }
                | Error::NoProject
                | Error::ReviewLocked { .. }
                | Error::AnalysisLocked { .. }
                | Error::AnalysisRefused(_)
                | Error::SettingsLocked(_)
                | Error::SettingsChanged
                | Error::MemoryNotReady(_)
                | Error::MemorySetupRunning
                | Error::MemoryBusy
                | Error::TurnRunning
                | Error::RunElsewhere
                | Error::TooManyRuns { .. }
                | Error::TooManyWorktrees { .. }
                | Error::PullRequestRefused(_)
                | Error::RunInOpenProject
                | Error::ThreadChanged { .. }
                | Error::NotAWorktree { .. }
                | Error::ApplyConflict { .. } => StatusCode::CONFLICT,
                Error::NoSuchNote { .. }
                | Error::NoSuchRequest
                | Error::NoSuchRun { .. }
                | Error::NoSuchDiagram { .. } => StatusCode::NOT_FOUND,
                Error::DiskFull { .. } => StatusCode::INSUFFICIENT_STORAGE,
                // The agent, the server behind this one, failed.
                Error::AgentNotFound { .. }
                | Error::StartAgent { .. }
                | Error::AgentSilent { .. }
                | Error::AgentExited { .. }
                | Error::AgentRefused { .. }
                | Error::AgentProtocol(_)
                | Error::ModelNotOffered { .. }
                | Error::ModelNotChoosable { .. }
                | Error::NoGh
                | Error::GitHub { .. } => StatusCode::BAD_GATEWAY,
                // Well-formed requests for what cannot be done. The page tells a missing
                // repository apart: it offers to make the folder one.
                Error::NotARepository { .. }
                | Error::FileTooLarge { .. }
                | Error::NoCommit
                | Error::ColonInPath { .. } => StatusCode::UNPROCESSABLE_ENTITY,
                Error::SpawnGit(_)
                | Error::Git { .. }
                | Error::Read { .. }
                | Error::Highlight { .. }
                | Error::Window(_)
                | Error::Webview(_)
                | Error::SpawnWorker(_)
                | Error::StartMemory(_)
                | Error::BenchInterrupted
                | Error::ReadReview { .. }
                | Error::ParseReview { .. }
                | Error::InvalidReview { .. }
                | Error::WriteReview { .. }
                | Error::ReadAnalysis { .. }
                | Error::ParseAnalysis { .. }
                | Error::InvalidAnalysis { .. }
                | Error::WriteAnalysis { .. }
                | Error::TrashAnalysis { .. }
                | Error::WriteRecent { .. }
                | Error::WriteSettings { .. }
                | Error::WriteRun { .. }
                | Error::CopyIndex { .. }
                | Error::WriteFolders { .. }
                | Error::WriteLayout { .. }
                | Error::ReadUsage { .. }
                | Error::ReadRun { .. }
                | Error::NoHome
                | Error::WriteWorktree { .. }
                | Error::ReadThreads { .. }
                | Error::WriteThreads { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            };
            reply(status, "text/plain", error_chain(&error).into_bytes())
        }
    }
}

/// The page's own file at `path` (`PAGE`), if it is one.
pub(crate) fn page_asset(path: &str) -> Option<Reply> {
    if path.starts_with("/api/") {
        return None;
    }
    PAGE.iter()
        .find(|(served, ..)| *served == path)
        .map(|&(_, kind, body)| {
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, kind)
                .header(wry::http::header::CACHE_CONTROL, "no-cache")
                .body(Cow::Borrowed(body))
                .expect("a status code and two static headers always make a valid response")
        })
}

pub(crate) fn reply(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Cow<'static, [u8]>>,
) -> Reply {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .body(body.into())
        .expect("a status code and one static header always make a valid response")
}

/// 405, with what the route takes.
fn not_allowed(takes: &'static str) -> Reply {
    reply(
        StatusCode::METHOD_NOT_ALLOWED,
        "text/plain",
        takes.as_bytes(),
    )
}

/// 400, saying what the request lacks.
fn bad_request(problem: &'static str) -> Reply {
    reply(StatusCode::BAD_REQUEST, "text/plain", problem.as_bytes())
}

/// Finds `key` in a URL query string and percent-decodes its value.
///
/// Returns `None` when the key is missing, an escape is malformed, or the decoded bytes are
/// not UTF-8. `+` is kept as is: the page encodes with `encodeURIComponent`, which writes a
/// space as `%20`.
fn query_value(query: &str, key: &str) -> Option<String> {
    let encoded = query
        .split('&')
        .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))?;

    let mut decoded = Vec::with_capacity(encoded.len());
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = hex_digit(bytes.next()?)?;
            let low = hex_digit(bytes.next()?)?;
            decoded.push(high << 4 | low);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests;
