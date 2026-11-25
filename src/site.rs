//! The API the page calls: [`Site`], which answers each request on the `postil://` scheme
//! with the page's own files or JSON, and the request and reply types of its routes.
//!
//! `shell.rs` owns the window and hands every request to the one thread that owns the
//! `Site` (`spawn_site_worker`); this module never touches the window.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wry::http::header::CONTENT_TYPE;
use wry::http::{Method, Request, Response, StatusCode};

use crate::agents::{self, BUILT_IN, Scanner};
use crate::error::{Error, Result, error_chain};
use crate::files::config_dir;
use crate::folders::{self, Folders};
use crate::highlight::{Highlighter, Token};
use crate::layout::{self, Layout, RawLayout};
use crate::notes::{self, NewNote, Review};
use crate::recent;
use crate::repo::Repo;
use crate::run::{Ending, Notify, Runner};
use crate::settings::{self, RawSettings, Settings};

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
        "/settings-dialog.js",
        "text/javascript",
        include_bytes!("../ui/settings-dialog.js"),
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
        "/notes-tray.js",
        "text/javascript",
        include_bytes!("../ui/notes-tray.js"),
    ),
    (
        "/welcome.js",
        "text/javascript",
        include_bytes!("../ui/welcome.js"),
    ),
    (
        "/icons.svg",
        "image/svg+xml",
        include_bytes!("../ui/icons.svg"),
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
    let home = std::env::var_os("HOME")
        // An empty HOME would put the worktrees in the current folder.
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".postil"));
    let site = Site::new(config.as_deref(), home.as_deref(), bench, agents, notify);
    if let Some(repo) = startup_repo(dir, &settings, site.recent_file.as_deref())? {
        site.open(repo, LiveRun::Stop)?;
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
/// One thread owns it and answers the requests one at a time (`spawn_site_worker`), so the
/// open project needs no lock. It is a `RefCell` because requests take `&self`; no borrow of
/// it is held across a call back into `Site`, so none panics.
pub(crate) struct Site {
    /// The open project, if any.
    project: RefCell<Option<Project>>,
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
    /// Where the pane widths are kept (`layout.rs`); `None`, for the same reason as
    /// `recent_file`, keeps them in each window only, until it closes.
    layout_file: Option<PathBuf>,
    /// Postil's home folder, `~/.postil`, under which threads' worktrees live (`thread.rs`);
    /// `None` when the home folder is unknown, which refuses every send.
    home: Option<PathBuf>,
    /// Where agents' programs are looked for, found in the background.
    agents: Scanner,
    /// Tells the page a run changed (`run.rs`).
    notify: Notify,
}

/// The open project.
#[derive(Debug, Clone)]
struct Project {
    repo: Arc<Repo>,
    /// The project's runs and the agent kept between them.
    runner: Arc<Runner>,
    /// Why the recent projects could not be updated when this project opened, shown under
    /// the branch name. Kept with the project, so every page load of it shows the line.
    warning: Option<String>,
}

/// The reply to `/api/repo` when a project is open; `null` when none is.
#[derive(Debug, Serialize)]
struct RepoInfo<'a> {
    name: String,
    root: String,
    branch: String,
    files: Vec<String>,
    bench: Option<&'a str>,
    warning: Option<String>,
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
    /// Every agent, built-in ones first; `None` while the agent scan runs.
    agents: Option<Vec<AgentView<'a>>>,
    /// The smallest and largest code font size.
    font_sizes: [u8; 2],
    tab_widths: [u8; 3],
}

/// The body of `PUT /api/settings`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveSettings {
    /// The revision of the settings file the change started from.
    revision: String,
    settings: RawSettings,
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
    /// How many changes the engineer's working tree has since the last commit, as `git status`
    /// lists them, which the thread, started from that commit, does not see (docs/design.md,
    /// "Threads"); `None` when the status could not be read, which the page says.
    uncommitted: Option<usize>,
    /// The linked files among them: their notes' lines may not match the agent's copy.
    uncommitted_noted: Vec<String>,
    /// Why `git status` failed, when `uncommitted` is `None`.
    uncommitted_error: Option<String>,
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

/// The reply to `/api/file`.
#[derive(Debug, Serialize)]
struct FileView<'a> {
    path: &'a str,
    lines: Vec<Vec<Token<'a>>>,
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
        Self {
            project: RefCell::new(None),
            highlighter: Highlighter::new(),
            bench: bench.map(str::to_owned),
            recent_file: config.map(|dir| dir.join("recent.json")),
            settings_file: config.map(|dir| dir.join(SETTINGS_FILE)),
            layout_file: config.map(|dir| dir.join("layout.json")),
            home: home.map(Path::to_owned),
            agents,
            notify,
        }
    }

    /// The open project's repository.
    fn repo(&self) -> Result<Arc<Repo>> {
        self.project
            .borrow()
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
        let mut project = self.project.borrow_mut();
        let open = project.as_ref();
        let same = open.is_some_and(|open| open.repo.root() == repo.root());
        if !same && matches!(live, LiveRun::Refuse) && open.is_some_and(|open| open.runner.busy()) {
            return Err(Error::RunInOpenProject);
        }
        let warning = match (&self.recent_file, &self.bench) {
            (_, Some(_)) => None,
            (None, None) => Some(format!("recent projects are off: {}", Error::NoHome)),
            (Some(file), None) => recent::record(file, repo.root())
                .err()
                .map(|error| error_chain(&error)),
        };
        if let Some(open) = project.as_mut().filter(|_| same) {
            open.warning = warning;
            return Ok(());
        }
        let repo = Arc::new(repo);
        let runner = Runner::new(
            Arc::clone(&repo),
            self.agents.clone(),
            Arc::clone(&self.notify),
            self.home.clone(),
        );
        let old = project.replace(Project {
            repo,
            runner,
            warning,
        });
        drop(project);
        // Stops the agent of the project this one replaces, after the borrow ends: that waits
        // for its process.
        if let Some(old) = old {
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
            "/api/notes" => self.respond_notes(method, query, body),
            "/api/notes/resolve" => match *method {
                Method::POST => self.change_note(query, notes::resolve),
                _ => not_allowed("POST ?id=<note id> with no body"),
            },
            "/api/notes/reopen" => match *method {
                Method::POST => self.change_note(query, notes::reopen),
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
            "/api/agents/rescan" => match *method {
                Method::POST => {
                    self.agents.rescan();
                    reply(StatusCode::OK, "application/json", self.settings())
                }
                _ => not_allowed("POST with no body"),
            },
            "/api/layout" => match *method {
                Method::GET => reply(StatusCode::OK, "application/json", self.layout()),
                Method::PUT => json_or_error(self.save_layout(body)),
                _ => not_allowed("GET the pane widths or PUT new ones"),
            },
            "/api/runs" => match *method {
                Method::GET => json_or_error(self.runner().and_then(|runner| {
                    let ids = runner.ids()?;
                    let live: Vec<_> = runner
                        .live()
                        .into_iter()
                        .map(|(id, thread)| serde_json::json!({ "id": id, "thread": thread }))
                        .collect();
                    Ok(to_json(&serde_json::json!({ "runs": ids, "live": live })))
                })),
                _ => not_allowed("GET"),
            },
            path if path == "/api/run" || path.starts_with("/api/run/") => {
                self.respond_run(path, method, query, body)
            }
            path if path == "/api/thread" || path.starts_with("/api/thread/") => {
                self.respond_thread(path, method, query, body)
            }
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    /// The open project, or `null`: the page then shows the welcome screen.
    fn repo_info(&self) -> Result<Vec<u8>> {
        let Some(Project { repo, warning, .. }) = self.project.borrow().clone() else {
            return Ok(b"null".to_vec());
        };
        let info = RepoInfo {
            name: repo.name(),
            root: repo.root().to_string_lossy().into_owned(),
            branch: repo.branch()?,
            files: repo.files()?,
            bench: self.bench.as_deref(),
            warning,
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
        let text = self.repo()?.read(path)?;
        let lines = self.highlighter.lines(path, &text)?;
        Ok(to_json(&FileView { path, lines }))
    }

    /// The reply to `GET /api/notes`: every note, moved to where its snippet is now. Reads
    /// the review file and never writes it.
    fn notes(&self) -> Result<Vec<u8>> {
        let review = notes::current(&*self.repo()?)?;
        Ok(to_json(&review))
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
        let review = notes::delete(&*self.repo()?, id)?;
        // A run goes with its last note (docs/design.md, "Runs"). The note is deleted by now:
        // a log that cannot be removed is reported, and tried again at the next delete.
        self.runner()?.delete_runs_without_notes()?;
        Ok(to_json(&review))
    }

    /// The reply to `POST /api/notes/resolve?id=N1` and `POST /api/notes/reopen?id=N1`:
    /// changes the note's state with `change` (`notes::resolve` or `notes::reopen`), and
    /// returns all notes.
    fn change_note(&self, query: &str, change: fn(&Repo, &str) -> Result<Review>) -> Reply {
        let Some(id) = query_value(query, "id") else {
            return bad_request("expected ?id=<note id>");
        };
        json_or_error(
            self.repo()
                .and_then(|repo| change(&repo, &id))
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
            .borrow()
            .as_ref()
            .map(|project| Arc::clone(&project.runner))
            .ok_or(Error::NoProject)
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
        self.runner()?.send(agent, &request.notes)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `GET /api/run/preview?notes=N1,N2`.
    fn preview_run(&self, ids: &str) -> Result<Vec<u8>> {
        let ids: Vec<String> = ids.split(',').map(str::to_owned).collect();
        let prompt = self.runner()?.preview(&ids)?;
        // One `git status` each time the send step reads the prompt, on the request thread:
        // as long as `git status` in a terminal ([`Repo::uncommitted`]). One that fails costs
        // the warning, not the prompt.
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
        let uncommitted_noted = prompt
            .files
            .iter()
            .filter(|file| is_uncommitted(file))
            .cloned()
            .collect();
        let uncommitted = entries.as_ref().map(Vec::len);
        Ok(to_json(&PreviewView {
            prompt: prompt.text,
            files: prompt.files,
            uncommitted,
            uncommitted_noted,
            uncommitted_error,
        }))
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
        let reviewers = settings
            .reviewers()
            .iter()
            .map(|name| settings.chosen_agent(name))
            .collect::<Result<Vec<_>>>()?;
        self.runner()?.review(&review.of, reviewers)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/run/follow`: sends the engineer's follow-up to the agent that
    /// ran the run it follows, and answers `{}`.
    fn follow_up(&self, body: &[u8]) -> Result<Vec<u8>> {
        let follow: FollowRun = parse_body(body, Error::BadRequest)?;
        let runner = self.runner()?;
        let agent = self
            .current_settings()
            .chosen_agent(&runner.agent_of(&follow.of)?)?;
        runner.follow_up(&follow.of, agent, &follow.text)?;
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

    /// The settings in effect: the file's, or the defaults when it cannot be used or there is
    /// no configuration directory. Reads the file each time.
    fn current_settings(&self) -> Settings {
        self.settings_file
            .as_deref()
            .map_or_else(Settings::default, |file| settings::load(file).settings)
    }

    /// The reply to `GET /api/settings`, and to every request that changes them: the
    /// settings in effect, why the file is not used if it is not, and the agents.
    fn settings(&self) -> Vec<u8> {
        let (loaded, file) = match &self.settings_file {
            Some(file) => (
                settings::load(file),
                Some(file.to_string_lossy().into_owned()),
            ),
            None => (
                settings::Loaded {
                    settings: Settings::default(),
                    problem: Some(format!("settings are off: {}", Error::NoHome)),
                    revision: String::new(),
                },
                None,
            ),
        };
        let agents = self
            .agents
            .path()
            .map(|path| agent_views(&loaded.settings, &path));
        to_json(&SettingsView {
            settings: &loaded.settings,
            problem: loaded.problem,
            revision: loaded.revision,
            file,
            agents,
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
        settings::save(file, &settings, &request.revision)?;
        Ok(self.settings())
    }

    /// The reply to `POST /api/settings/reset`: writes the defaults over the file.
    fn reset_settings(&self) -> Result<Vec<u8>> {
        settings::reset(self.settings_file.as_deref().ok_or(Error::NoHome)?)?;
        Ok(self.settings())
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
            .field("agents", &self.agents)
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
        if let Some(project) = self.project.borrow_mut().take() {
            project.runner.close();
        }
    }
}

/// The built-in agents, then the custom ones, each with where its program was found on
/// `search_path`.
fn agent_views<'a>(settings: &'a Settings, search_path: &OsStr) -> Vec<AgentView<'a>> {
    let found = |program: Option<&str>| {
        program
            .and_then(|program| agents::find_program(program, search_path))
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

/// The `after` of `GET /api/run?after=N`: the number of entries the page has; 0 without it.
fn after_of(query: &str) -> Result<usize> {
    query_value(query, "after").map_or(Ok(0), |after| {
        after.parse().map_err(|_| {
            Error::BadRequest(format!("after={after:?}: expected a number of entries"))
        })
    })
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
                | Error::SettingsLocked(_)
                | Error::SettingsChanged
                | Error::TurnRunning
                | Error::RunElsewhere
                | Error::TooManyRuns { .. }
                | Error::TooManyWorktrees { .. }
                | Error::PullRequestRefused(_)
                | Error::RunInOpenProject
                | Error::ThreadChanged { .. }
                | Error::NotAWorktree { .. }
                | Error::ApplyConflict { .. } => StatusCode::CONFLICT,
                Error::NoSuchNote { .. } | Error::NoSuchRequest | Error::NoSuchRun { .. } => {
                    StatusCode::NOT_FOUND
                }
                Error::DiskFull { .. } => StatusCode::INSUFFICIENT_STORAGE,
                // The agent, the server behind this one, failed.
                Error::AgentNotFound { .. }
                | Error::StartAgent { .. }
                | Error::AgentSilent { .. }
                | Error::AgentExited { .. }
                | Error::AgentRefused { .. }
                | Error::AgentProtocol(_)
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
                | Error::BenchInterrupted
                | Error::ReadReview { .. }
                | Error::ParseReview { .. }
                | Error::InvalidReview { .. }
                | Error::WriteReview { .. }
                | Error::WriteRecent { .. }
                | Error::WriteSettings { .. }
                | Error::WriteRun { .. }
                | Error::CopyIndex { .. }
                | Error::WriteFolders { .. }
                | Error::WriteLayout { .. }
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
    PAGE.iter()
        .find(|(served, ..)| *served == path)
        .map(|&(_, kind, body)| reply(StatusCode::OK, kind, body))
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
