//! The window, the webview, and the URL scheme the page talks to.
//!
//! The page is served from `postil://localhost/`. It is compiled into the binary, and it
//! gets its data by `fetch` on the same scheme, which [`Site::respond`] answers in this
//! process. No socket is opened.
//!
//! One window shows one project at a time. Without one, the page shows the welcome screen
//! (open a folder, the recent projects, or a folder dropped on the window); opening another
//! project replaces the current one and the page reloads.

use std::borrow::Cow;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Theme, Window, WindowBuilder};
use wry::http::header::CONTENT_TYPE;
use wry::http::{Method, Request, Response, StatusCode};
use wry::{DragDropEvent, WebView, WebViewBuilder};

use crate::dialog;
use crate::error::{Error, Result};
use crate::files::config_dir;
use crate::highlight::{Highlighter, Token};
use crate::notes::{self, NewNote};
use crate::recent;
use crate::repo::Repo;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");
const FUZZY_JS: &str = include_str!("../ui/fuzzy.js");
const TREE_JS: &str = include_str!("../ui/tree.js");
const ICONS_JS: &str = include_str!("../ui/icons.js");
const ICONS_SVG: &str = include_str!("../ui/icons.svg");
const GEIST: &[u8] = include_bytes!("../ui/fonts/Geist-Variable.woff2");
const GEIST_MONO: &[u8] = include_bytes!("../ui/fonts/GeistMono-Variable.woff2");

/// The `--canvas` colors of `ui/app.css`, painted behind the page so the window does not
/// flash white before the first frame.
const DARK_CANVAS: (u8, u8, u8, u8) = (0x15, 0x17, 0x1b, 0xff);
const LIGHT_CANVAS: (u8, u8, u8, u8) = (0xf4, 0xf4, 0xf1, 0xff);

/// What the benchmark measured.
#[derive(Debug)]
pub struct BenchReport {
    /// From the start of [`bench`] to the page reporting that the file is painted.
    pub launch_to_paint: Duration,
    /// The page's own measurements, as the JSON object `ui/app.js` builds in `runBench`.
    pub page: String,
}

/// Opens the window and returns when it closes. With `dir`, the window opens on the
/// repository that contains it; without, on the welcome screen.
///
/// # Errors
///
/// Fails when `dir` is not inside a git working tree, when the home directory is unknown,
/// or when the window or the webview cannot be created.
pub fn run(dir: Option<&Path>) -> Result<()> {
    serve(dir, None).map(|_| ())
}

/// Opens the window on the repository that contains `dir`, shows `file`, scrolls through
/// it, and returns the measurements.
///
/// `file` is relative to the repository root.
///
/// # Errors
///
/// As [`run`], and [`Error::BenchInterrupted`] when the window closes first.
pub fn bench(dir: &Path, file: &str) -> Result<BenchReport> {
    serve(Some(dir), Some(file))?.ok_or(Error::BenchInterrupted)
}

/// What reaches the event loop: the page's messages (through `window.ipc.postMessage`) and
/// folders dropped on the window.
#[derive(Debug)]
enum AppEvent {
    /// The pointer went down on the window's drag strip.
    DragWindow,
    /// The page asked for the folder dialog (Open Folder).
    ChooseFolder,
    /// The engineer pressed Cmd-Q or Cmd-W. The window has no menu bar, which is where macOS
    /// would otherwise find Quit, so the page forwards the keys.
    Quit,
    /// A folder (or file) was dropped on the window.
    Dropped(PathBuf),
    /// The benchmark's file is on screen.
    BenchPainted,
    /// The benchmark finished; the payload is its JSON report.
    BenchDone(String),
}

impl AppEvent {
    /// Reads a message as `ui/app.js` posts it.
    fn parse(body: &str) -> Option<Self> {
        match body {
            "drag" => Some(Self::DragWindow),
            "choose-folder" => Some(Self::ChooseFolder),
            "quit" => Some(Self::Quit),
            "bench-painted" => Some(Self::BenchPainted),
            _ => body
                .strip_prefix("bench:")
                .map(|report| Self::BenchDone(report.to_owned())),
        }
    }
}

fn serve(dir: Option<&Path>, bench: Option<&str>) -> Result<Option<BenchReport>> {
    let started = Instant::now();
    // Without a configuration directory the recent list is off, which the page says once a
    // project is open: the list is a convenience, so it never stops the app from starting.
    let site = Site::new(config_dir().ok().map(|dir| dir.join("recent.json")), bench);
    if let Some(dir) = dir {
        site.open(Repo::open(dir)?);
    }
    // A benchmark of a file that cannot be read would time an empty pane and report it as a
    // result. Fail here instead, before the window opens.
    if let Some(file) = bench {
        site.repo()?.read(file)?;
    }

    let mut event_loop: EventLoop<AppEvent> = EventLoopBuilder::with_user_event().build();
    let window = build_window(&event_loop)?;
    let canvas = match window.theme() {
        Theme::Dark => DARK_CANVAS,
        _ => LIGHT_CANVAS,
    };

    let page_proxy = event_loop.create_proxy();
    let drop_proxy = event_loop.create_proxy();
    let dragging_files = Cell::new(false);
    let webview = WebViewBuilder::new()
        .with_background_color(canvas)
        .with_custom_protocol("postil".to_owned(), move |_webview, request| {
            site.respond(&request)
        })
        .with_ipc_handler(move |message| {
            let event = AppEvent::parse(message.body());
            // The page and this handler are compiled into one binary, so a message that
            // does not parse is a bug in one of them.
            debug_assert!(event.is_some(), "unknown page message");
            if let Some(event) = event {
                // Sending fails only once the event loop is gone, when nobody is left to
                // act on the message.
                drop(page_proxy.send_event(event));
            }
        })
        // A drag that carries files is the app's: the webview would navigate to a dropped
        // file. Any other drag (text into the note composer) is left to the webview, so the
        // handler answers `true` only while files are over the window.
        .with_drag_drop_handler(move |event| match event {
            DragDropEvent::Enter { paths, .. } => {
                dragging_files.set(!paths.is_empty());
                dragging_files.get()
            }
            DragDropEvent::Over { .. } => dragging_files.get(),
            DragDropEvent::Drop { paths, .. } => {
                dragging_files.set(false);
                // A project is one folder: the first folder dropped opens, or, when only files
                // were dropped, the first file, which the page then reports as not a folder.
                let Some(path) = paths
                    .iter()
                    .find(|path| path.is_dir())
                    .or_else(|| paths.first())
                    .cloned()
                else {
                    return false;
                };
                // As above: fails only once the event loop is gone.
                drop(drop_proxy.send_event(AppEvent::Dropped(path)));
                true
            }
            DragDropEvent::Leave => dragging_files.replace(false),
            _ => false,
        })
        // The page never navigates. Should it ever be sent elsewhere, the other origin must
        // not reach the `postil://` handlers, which read and write the repository.
        .with_navigation_handler(|url| url.starts_with("postil://"))
        .with_url("postil://localhost/")
        .build(&window)
        .map_err(Error::Webview)?;

    let mut launch_to_paint = None;
    let mut report = None;
    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
            | Event::UserEvent(AppEvent::Quit) => *control_flow = ControlFlow::Exit,
            Event::UserEvent(AppEvent::DragWindow) => {
                // Fails when the pointer button is already up again: there is then nothing
                // to drag.
                drop(window.drag_window());
            }
            Event::UserEvent(AppEvent::ChooseFolder) => {
                if let Some(folder) = dialog::choose_folder() {
                    offer_folder(&webview, &folder);
                }
            }
            Event::UserEvent(AppEvent::Dropped(path)) => offer_folder(&webview, &path),
            Event::UserEvent(AppEvent::BenchPainted) => {
                launch_to_paint = Some(started.elapsed());
            }
            Event::UserEvent(AppEvent::BenchDone(page)) => {
                report = Some(BenchReport {
                    // The page posts `bench-painted` before `bench:`; if it somehow did
                    // not, the time until now is the honest upper bound.
                    launch_to_paint: launch_to_paint.unwrap_or_else(|| started.elapsed()),
                    page,
                });
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });

    // The webview draws into the window, so it goes first.
    drop(webview);
    drop(window);
    Ok(report)
}

/// Hands a chosen or dropped folder to the page, which opens it through `/api/open`: the
/// page decides what to show for a folder that is not a repository.
fn offer_folder(webview: &WebView, folder: &Path) {
    let path = serde_json::to_string(&folder.to_string_lossy())
        .expect("a string always serializes to JSON");
    // Fails only when the webview is being torn down, when there is no page to tell.
    drop(webview.evaluate_script(&format!("window.postil?.openFolder({path})")));
}

fn build_window(event_loop: &EventLoop<AppEvent>) -> Result<Window> {
    let builder = WindowBuilder::new()
        .with_title("Postil")
        .with_inner_size(LogicalSize::new(1440.0, 900.0));

    // No title bar: the page draws up to the top edge and the window buttons sit over the
    // sidebar, 14 px in and 16 px down to line up with the sidebar's 44 px top strip.
    #[cfg(target_os = "macos")]
    let builder = {
        use tao::dpi::LogicalPosition;
        use tao::platform::macos::WindowBuilderExtMacOS;
        builder
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true)
            .with_traffic_light_inset(LogicalPosition::new(14.0, 16.0))
    };

    builder.build(event_loop).map_err(Error::Window)
}

/// Everything the page can ask for.
///
/// The URL scheme handler must be `Send + Sync`, so the open project sits behind a mutex.
/// The lock is held only to replace the project or clone it out, never across a `git` call
/// or a file read, and the project is replaced whole, so the state is valid after any panic.
/// wry calls the handler on the main thread, one request at a time, so the lock is never
/// contended; it is there for the bound.
struct Site {
    /// The open project, if any.
    project: Mutex<Option<Project>>,
    highlighter: Highlighter,
    /// The file the benchmark opens, when this is a benchmark run.
    bench: Option<String>,
    /// Where the recent projects are listed (`recent.rs`); `None` when the configuration
    /// directory is unknown, which turns the list off.
    recent_file: Option<PathBuf>,
}

/// The open project.
#[derive(Clone)]
struct Project {
    repo: Arc<Repo>,
    /// Why the recent projects could not be updated when this project opened, shown under
    /// the branch name. Kept with the project, so every page load of it shows the line.
    warning: Option<String>,
}

/// The reply to `/api/repo` when a project is open; `null` when none is.
#[derive(Serialize)]
struct RepoInfo<'a> {
    name: String,
    root: String,
    branch: String,
    files: Vec<String>,
    bench: Option<&'a str>,
    warning: Option<String>,
}

/// An entry of the reply to `/api/recent`.
#[derive(Serialize)]
struct RecentProject {
    path: String,
    name: String,
    /// Whether the folder is still there; a moved or deleted one is listed, greyed out.
    exists: bool,
}

/// The body of `POST /api/open` and `POST /api/init`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderRequest {
    path: PathBuf,
}

/// The reply to `/api/file`.
#[derive(Serialize)]
struct FileView<'a> {
    path: &'a str,
    lines: Vec<Vec<Token<'a>>>,
}

type Reply = Response<Cow<'static, [u8]>>;

/// Locks one of `Site`'s mutexes. Poisoning is ignored: every value is replaced whole, so a
/// panic elsewhere cannot leave one half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Site {
    fn new(recent_file: Option<PathBuf>, bench: Option<&str>) -> Self {
        Self {
            project: Mutex::new(None),
            highlighter: Highlighter::new(),
            bench: bench.map(str::to_owned),
            recent_file,
        }
    }

    /// The open project's repository.
    fn repo(&self) -> Result<Arc<Repo>> {
        lock(&self.project)
            .as_ref()
            .map(|project| Arc::clone(&project.repo))
            .ok_or(Error::NoProject)
    }

    /// Makes `repo` the open project and puts it first among the recent projects. A
    /// benchmark does not count as opening: a scripted run must not reorder the list.
    fn open(&self, repo: Repo) {
        let warning = match (&self.recent_file, &self.bench) {
            (_, Some(_)) => None,
            (None, None) => Some(format!("recent projects are off: {}", Error::NoHome)),
            (Some(file), None) => recent::record(file, repo.root())
                .err()
                .map(|error| error_chain(&error)),
        };
        *lock(&self.project) = Some(Project {
            repo: Arc::new(repo),
            warning,
        });
    }

    fn respond(&self, request: &Request<Vec<u8>>) -> Reply {
        let uri = request.uri();
        let post = *request.method() == Method::POST;
        match uri.path() {
            "/" => reply(StatusCode::OK, "text/html", INDEX_HTML.as_bytes()),
            "/app.css" => reply(StatusCode::OK, "text/css", APP_CSS.as_bytes()),
            "/app.js" => reply(StatusCode::OK, "text/javascript", APP_JS.as_bytes()),
            "/fuzzy.js" => reply(StatusCode::OK, "text/javascript", FUZZY_JS.as_bytes()),
            "/tree.js" => reply(StatusCode::OK, "text/javascript", TREE_JS.as_bytes()),
            "/icons.js" => reply(StatusCode::OK, "text/javascript", ICONS_JS.as_bytes()),
            "/icons.svg" => reply(StatusCode::OK, "image/svg+xml", ICONS_SVG.as_bytes()),
            "/fonts/Geist-Variable.woff2" => reply(StatusCode::OK, "font/woff2", GEIST),
            "/fonts/GeistMono-Variable.woff2" => reply(StatusCode::OK, "font/woff2", GEIST_MONO),
            "/api/repo" => json_or_error(self.repo_info()),
            "/api/recent" => json_or_error(Ok(self.recent())),
            "/api/open" if post => json_or_error(self.open_folder(request.body(), Repo::open)),
            "/api/init" if post => json_or_error(self.open_folder(request.body(), Repo::init)),
            "/api/notes" => match *request.method() {
                Method::GET => json_or_error(self.notes()),
                Method::POST => json_or_error(self.add_note(request.body())),
                Method::DELETE => match query_value(uri.query().unwrap_or(""), "id") {
                    Some(id) => json_or_error(self.delete_note(&id)),
                    None => reply(
                        StatusCode::BAD_REQUEST,
                        "text/plain",
                        &b"expected ?id=<note id>"[..],
                    ),
                },
                _ => reply(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "text/plain",
                    &b"GET the notes, POST a new one, or DELETE ?id=N1"[..],
                ),
            },
            "/api/open" | "/api/init" => reply(
                StatusCode::METHOD_NOT_ALLOWED,
                "text/plain",
                &b"POST {\"path\": ...}"[..],
            ),
            "/api/file" => match query_value(uri.query().unwrap_or(""), "path") {
                Some(path) => json_or_error(self.file_view(&path)),
                None => reply(
                    StatusCode::BAD_REQUEST,
                    "text/plain",
                    &b"expected ?path=<percent-encoded path>"[..],
                ),
            },
            _ => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        }
    }

    /// The open project, or `null`: the page then shows the welcome screen.
    fn repo_info(&self) -> Result<Vec<u8>> {
        let Some(Project { repo, warning }) = lock(&self.project).clone() else {
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
        // `exists` checks each folder on the main thread. A folder on a network volume that is
        // gone can make that slow; accepted, as ten checks on local disks take microseconds.
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
        let request: FolderRequest = serde_json::from_slice(body)
            .map_err(|error| Error::BadRequest(format!("unreadable request: {error}")))?;
        self.open(open_repo(&request.path)?);
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
        let new: NewNote = serde_json::from_slice(body)
            .map_err(|error| Error::InvalidNote(format!("unreadable request: {error}")))?;
        let review = notes::add(&*self.repo()?, new)?;
        Ok(to_json(&review))
    }

    /// The reply to `DELETE /api/notes?id=N1`: removes the note, saves, and returns the
    /// other notes.
    fn delete_note(&self, id: &str) -> Result<Vec<u8>> {
        let review = notes::delete(&*self.repo()?, id)?;
        Ok(to_json(&review))
    }
}

fn to_json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect(
        "the reply types are plain structs of strings, numbers, and sequences, which always \
         serialize",
    )
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
                | Error::TooBroad { .. } => StatusCode::BAD_REQUEST,
                Error::NoteTextChanged { .. } | Error::NoProject => StatusCode::CONFLICT,
                Error::NoSuchNote { .. } => StatusCode::NOT_FOUND,
                // The page tells this one apart: it offers to make the folder a repository.
                Error::NotARepository { .. } => StatusCode::UNPROCESSABLE_ENTITY,
                Error::SpawnGit(_)
                | Error::Git { .. }
                | Error::Read { .. }
                | Error::Highlight(_)
                | Error::Window(_)
                | Error::Webview(_)
                | Error::BenchInterrupted
                | Error::ReadReview { .. }
                | Error::ParseReview { .. }
                | Error::InvalidReview { .. }
                | Error::WriteReview { .. }
                | Error::WriteRecent { .. }
                | Error::NoHome => StatusCode::INTERNAL_SERVER_ERROR,
            };
            reply(status, "text/plain", error_chain(&error).into_bytes())
        }
    }
}

/// The error and its sources on one line, for the page to show.
fn error_chain(error: &Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn reply(
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
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;

    /// A real git working tree in the temporary directory, removed on drop.
    struct TempRepo {
        root: PathBuf,
    }

    impl TempRepo {
        /// `name` must differ between tests: they run in parallel in one process.
        fn new(name: &str) -> Self {
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

        fn write(&self, file: &str, text: &str) {
            fs::write(self.root.join(file), text).unwrap();
        }

        /// A site with this repository open.
        fn site(&self) -> Site {
            let site = self.empty_site();
            site.open(Repo::open(&self.root).unwrap());
            site
        }

        /// A site with no project open, its recent list inside this repository's git
        /// directory, where no file listing sees it and the drop below removes it.
        fn empty_site(&self) -> Site {
            Site::new(Some(self.root.join(".git/test-recent.json")), None)
        }
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

    fn post_note(
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
        assert_eq!(stored["version"], 1);
        assert_eq!(stored["notes"], review["notes"]);

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
        assert!(reply.contains("the note was not saved"), "{reply}");
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

        let request = Request::builder()
            .method(Method::PUT)
            .uri(NOTES)
            .body(Vec::new())
            .unwrap();
        assert_eq!(
            site.respond(&request).status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
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

    #[test]
    fn a_deleted_note_is_gone_from_the_file_and_its_id_is_not_reused() {
        let repo = TempRepo::new("notes-delete");
        repo.write("a.txt", "x\ny\nz\n");
        let site = repo.site();
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");

        let (status, body) = delete(&site, &format!("{NOTES}?id=N2"));
        assert_eq!(status, StatusCode::OK, "{body}");
        let review: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(anchors(&review), [anchor("N1", 1, 1, "found")]);
        let stored = fs::read_to_string(site.repo().unwrap().review_file()).unwrap();
        assert!(!stored.contains("\"N2\""), "{stored}");

        let review = post_note(&site, "a.txt", 3, 3, "three");
        assert_eq!(
            anchors(&review),
            [anchor("N1", 1, 1, "found"), anchor("N2", 3, 3, "found")],
            "the highest id left is N1, so the next is N2 again"
        );
    }

    #[test]
    fn deleting_a_missing_or_malformed_id_says_so_and_changes_nothing() {
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
            (r#"{"version": 2, "notes": []}"#, "version 2".to_owned()),
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
    fn repo_lists_tracked_and_untracked_files_but_not_ignored_ones() {
        let repo = TempRepo::new("list");
        repo.write("src/tracked.rs", "fn a() {}\n");
        repo.git(&["add", "src/tracked.rs"]);
        repo.write("untracked.txt", "x\n");
        repo.write(".gitignore", "ignored.log\n");
        repo.write("ignored.log", "x\n");

        let info = get_json(&repo.site(), "postil://localhost/api/repo");

        assert_eq!(info["branch"], "trunk");
        assert_eq!(
            info["name"],
            repo.root.file_name().unwrap().to_str().unwrap()
        );
        assert_eq!(
            info["files"],
            serde_json::json!([".gitignore", "src/tracked.rs", "untracked.txt"])
        );
        assert_eq!(info["bench"], serde_json::Value::Null);
    }

    /// A plain folder in the temporary directory, outside any repository.
    fn plain_folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("postil-{}-plain-{name}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_request(path: &Path) -> String {
        serde_json::json!({ "path": path }).to_string()
    }

    #[test]
    fn without_a_project_the_page_gets_null_and_project_routes_say_to_open_one() {
        let repo = TempRepo::new("no-project");
        let site = repo.empty_site();

        let (status, body) = get(&site, "postil://localhost/api/repo");
        assert_eq!((status, body.as_str()), (StatusCode::OK, "null"));
        for uri in ["/api/notes", "/api/file?path=a.rs"] {
            let (status, body) = get(&site, &format!("postil://localhost{uri}"));
            assert_eq!(status, StatusCode::CONFLICT, "{uri}");
            assert!(body.contains("open a folder first"), "{uri}: {body}");
        }
    }

    #[test]
    fn opening_a_folder_makes_its_repository_the_project_and_lists_it_first() {
        let first = TempRepo::new("open-first");
        let second = TempRepo::new("open-second");
        second.write("src/b.rs", "fn b() {}\n");
        let site = first.empty_site();

        for repo in [&first, &second] {
            let (status, body) = post(
                &site,
                "postil://localhost/api/open",
                &open_request(&repo.root),
            );
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let info = get_json(&site, "postil://localhost/api/repo");
        assert_eq!(info["files"], serde_json::json!(["src/b.rs"]));
        assert_eq!(info["warning"], serde_json::Value::Null);

        let recent = get_json(&site, "postil://localhost/api/recent");
        let paths: Vec<&str> = recent
            .as_array()
            .unwrap()
            .iter()
            .map(|project| project["path"].as_str().unwrap())
            .collect();
        let root = |repo: &TempRepo| site_root(&repo.root);
        assert_eq!(paths, [root(&second), root(&first)]);
        assert_eq!(recent[0]["exists"], true);
        assert_eq!(
            recent[0]["name"],
            second.root.file_name().unwrap().to_str().unwrap()
        );
    }

    /// The root as git reports it, which resolves symbolic links such as /var to
    /// /private/var on macOS.
    fn site_root(dir: &Path) -> String {
        Repo::open(dir).unwrap().root().to_str().unwrap().to_owned()
    }

    #[test]
    fn a_plain_folder_is_refused_with_its_own_status_until_it_is_made_a_repository() {
        let repo = TempRepo::new("init-recent");
        let site = repo.empty_site();
        let folder = plain_folder("init");

        let (status, body) = post(&site, "postil://localhost/api/open", &open_request(&folder));
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body.contains("is not in a git repository"), "{body}");
        assert_eq!(get(&site, "postil://localhost/api/repo").1, "null");

        let (status, body) = post(&site, "postil://localhost/api/init", &open_request(&folder));
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(folder.join(".git").is_dir());
        let info = get_json(&site, "postil://localhost/api/repo");
        assert_eq!(info["root"], site_root(&folder));
        drop(fs::remove_dir_all(&folder));
    }

    #[test]
    fn bad_open_requests_name_the_problem() {
        let repo = TempRepo::new("open-bad");
        let site = repo.empty_site();

        let missing = open_request(Path::new("/no/such/folder"));
        let (status, body) = post(&site, "postil://localhost/api/open", &missing);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, "/no/such/folder is not a folder");

        let (status, body) = post(&site, "postil://localhost/api/open", "{\"dir\": \"x\"}");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("unreadable request"), "{body}");

        let (status, _) = get(&site, "postil://localhost/api/open");
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn a_recent_list_that_cannot_be_written_warns_and_the_project_still_opens() {
        let repo = TempRepo::new("recent-unwritable");
        // The list's path is a directory, so no file can be renamed onto it.
        let recent = repo.root.join(".git/recent-dir");
        fs::create_dir_all(recent.join("inside")).unwrap();
        let site = Site::new(Some(recent), None);

        let (status, body) = post(
            &site,
            "postil://localhost/api/open",
            &open_request(&repo.root),
        );
        assert_eq!(status, StatusCode::OK, "{body}");
        // Every load of the project shows it: a request from the page being replaced must
        // not be the one that takes it.
        for _ in 0..2 {
            let info = get_json(&site, "postil://localhost/api/repo");
            let warning = info["warning"].as_str().unwrap();
            assert!(
                warning.contains("cannot update the recent projects"),
                "{warning}"
            );
        }
    }

    #[test]
    fn without_a_configuration_directory_the_recent_list_is_off_and_projects_open() {
        let repo = TempRepo::new("recent-off");
        let site = Site::new(None, None);

        site.open(Repo::open(&repo.root).unwrap());
        let info = get_json(&site, "postil://localhost/api/repo");
        assert_eq!(
            info["warning"],
            "recent projects are off: cannot find the home directory (is HOME set?)"
        );
        assert_eq!(get(&site, "postil://localhost/api/recent").1, "[]");
    }

    #[test]
    fn a_benchmark_leaves_the_recent_list_alone() {
        let repo = TempRepo::new("recent-bench");
        repo.write("a.rs", "fn a() {}\n");
        let recent = repo.root.join(".git/test-recent.json");
        let site = Site::new(Some(recent.clone()), Some("a.rs"));

        site.open(Repo::open(&repo.root).unwrap());
        assert!(!recent.exists());
        assert_eq!(
            get_json(&site, "postil://localhost/api/repo")["warning"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn init_inside_a_repository_opens_it_and_makes_no_repository_within() {
        let repo = TempRepo::new("init-inside");
        repo.write("src/a.rs", "fn a() {}\n");
        let site = repo.empty_site();

        let src = repo.root.join("src");
        let (status, body) = post(&site, "postil://localhost/api/init", &open_request(&src));
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!src.join(".git").exists(), "no nested repository");
        let info = get_json(&site, "postil://localhost/api/repo");
        assert_eq!(info["root"], site_root(&repo.root));
    }

    #[test]
    fn a_recent_project_that_was_moved_is_listed_as_missing() {
        let repo = TempRepo::new("recent-moved");
        let site = repo.empty_site();
        let folder = plain_folder("moved");
        let (status, body) = post(&site, "postil://localhost/api/init", &open_request(&folder));
        assert_eq!(status, StatusCode::OK, "{body}");
        fs::remove_dir_all(&folder).unwrap();

        let recent = get_json(&site, "postil://localhost/api/recent");
        assert_eq!(recent[0]["exists"], false);
    }

    #[test]
    fn repo_is_found_from_a_subdirectory() {
        let repo = TempRepo::new("subdir");
        repo.write("src/a.rs", "fn a() {}\n");

        let site = Site::new(
            Some(repo.root.join(".git/test-recent.json")),
            Some("src/a.rs"),
        );
        site.open(Repo::open(&repo.root.join("src")).unwrap());
        let info = get_json(&site, "postil://localhost/api/repo");

        assert_eq!(info["files"], serde_json::json!(["src/a.rs"]));
        assert_eq!(info["bench"], "src/a.rs");
    }

    #[test]
    fn file_comes_back_as_lines_of_tokens() {
        let repo = TempRepo::new("file");
        repo.write("src/my file.rs", "// hi\nfn a() {}\n");

        let view = get_json(
            &repo.site(),
            "postil://localhost/api/file?path=src%2Fmy%20file.rs",
        );

        assert_eq!(view["path"], "src/my file.rs");
        assert_eq!(view["lines"].as_array().unwrap().len(), 2);
        assert_eq!(view["lines"][0], serde_json::json!([["m", "// hi"]]));
        assert_eq!(view["lines"][1][0], serde_json::json!(["k", "fn"]));
    }

    #[test]
    fn file_outside_the_repository_is_refused() {
        let repo = TempRepo::new("outside");
        let site = repo.site();

        for path in [
            "..%2Fsecret",
            "src%2F..%2F..%2Fsecret",
            "%2Fetc%2Fpasswd",
            "",
        ] {
            let (status, body) = get(&site, &format!("postil://localhost/api/file?path={path}"));
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        }
    }

    #[test]
    fn missing_file_and_missing_parameter_are_errors_with_a_message() {
        let repo = TempRepo::new("missing");
        let site = repo.site();

        let (status, body) = get(&site, "postil://localhost/api/file?path=nope.rs");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("nope.rs"), "{body}");

        let (status, _) = get(&site, "postil://localhost/api/file");
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = get(&site, "postil://localhost/nope");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn page_assets_are_served_with_their_types() {
        let repo = TempRepo::new("assets");
        let site = repo.site();
        let cases = [
            ("/", "text/html"),
            ("/app.css", "text/css"),
            ("/app.js", "text/javascript"),
            ("/fuzzy.js", "text/javascript"),
            ("/tree.js", "text/javascript"),
            ("/icons.js", "text/javascript"),
            ("/icons.svg", "image/svg+xml"),
            ("/fonts/Geist-Variable.woff2", "font/woff2"),
            ("/fonts/GeistMono-Variable.woff2", "font/woff2"),
        ];

        for (path, content_type) in cases {
            let uri = format!("postil://localhost{path}");
            let request = Request::builder().uri(uri).body(Vec::new()).unwrap();
            let reply = site.respond(&request);
            assert_eq!(reply.status(), StatusCode::OK, "{path}");
            assert_eq!(reply.headers()[CONTENT_TYPE], content_type, "{path}");
            assert!(!reply.body().is_empty(), "{path}");
        }
    }

    #[test]
    fn page_only_links_to_assets_that_are_served() {
        // The page is checked by eye, but a renamed asset would only show as a blank window.
        // The served paths themselves are covered by `page_assets_are_served_with_their_types`.
        for link in ["app.css", "app.js"] {
            assert!(INDEX_HTML.contains(&format!("\"{link}\"")), "{link}");
        }
        assert!(APP_JS.contains("\"./fuzzy.js\""), "app.js imports fuzzy.js");
        assert!(APP_JS.contains("\"./tree.js\""), "app.js imports tree.js");
        assert!(APP_JS.contains("\"./icons.js\""), "app.js imports icons.js");
        assert!(
            APP_JS.contains("\"icons.svg\""),
            "app.js loads the icon sprite"
        );
        for font in [
            "fonts/Geist-Variable.woff2",
            "fonts/GeistMono-Variable.woff2",
        ] {
            assert!(APP_CSS.contains(font), "{font}");
        }
    }

    #[test]
    fn page_messages_parse_as_the_page_posts_them() {
        assert!(matches!(
            AppEvent::parse("drag"),
            Some(AppEvent::DragWindow)
        ));
        assert!(matches!(
            AppEvent::parse("choose-folder"),
            Some(AppEvent::ChooseFolder)
        ));
        assert!(matches!(AppEvent::parse("quit"), Some(AppEvent::Quit)));
        assert!(matches!(
            AppEvent::parse("bench-painted"),
            Some(AppEvent::BenchPainted)
        ));
        assert!(matches!(
            AppEvent::parse("bench:{\"a\":1}"),
            Some(AppEvent::BenchDone(report)) if report == "{\"a\":1}"
        ));
        assert!(AppEvent::parse("nonsense").is_none());
    }

    #[test]
    fn query_value_decodes_what_encode_uri_component_writes() {
        // encodeURIComponent("src/my file+é.rs")
        let query = "path=src%2Fmy%20file%2B%C3%A9.rs";
        assert_eq!(query_value(query, "path").unwrap(), "src/my file+é.rs");
        // Percent-encoding allows lowercase hex too, though the page never writes it.
        assert_eq!(query_value("path=%c3%a9%2f", "path").unwrap(), "é/");
    }

    #[test]
    fn query_value_finds_the_key_among_others() {
        assert_eq!(query_value("a=1&path=x&b=2", "path").unwrap(), "x");
        assert_eq!(query_value("pathology=1&path=x", "path").unwrap(), "x");
        assert_eq!(query_value("path=", "path").unwrap(), "");
    }

    #[test]
    fn query_value_rejects_missing_keys_and_bad_escapes() {
        assert_eq!(query_value("", "path"), None);
        assert_eq!(query_value("other=x", "path"), None);
        assert_eq!(query_value("path=%", "path"), None, "escape cut short");
        assert_eq!(query_value("path=%4", "path"), None, "escape cut short");
        assert_eq!(query_value("path=%zz", "path"), None, "not hex");
        assert_eq!(query_value("path=%ff", "path"), None, "not UTF-8");
    }
}
