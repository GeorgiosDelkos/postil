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
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Theme, Window, WindowBuilder};
use wry::http::header::CONTENT_TYPE;
use wry::http::{Method, Request, Response, StatusCode, Uri};
use wry::{DragDropEvent, PageLoadEvent, WebView, WebViewBuilder};

use crate::agents::{self, BUILT_IN, Scanner};
use crate::dialog;
use crate::error::{Error, Result, error_chain};
use crate::files::config_dir;
use crate::folders::{self, Folders};
use crate::highlight::{Highlighter, Token};
use crate::layout::{self, Layout};
#[cfg(target_os = "macos")]
use crate::menu::Command;
use crate::notes::{self, NewNote, Review};
use crate::recent;
use crate::repo::Repo;
use crate::run::{AgentChoice, Notify, Runner};
use crate::settings::{self, Settings};

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
        "/question.js",
        "text/javascript",
        include_bytes!("../ui/question.js"),
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
    /// The page confirmed quitting (Quit or Close Window from the menu, or the close button): no
    /// draft note would be lost.
    Quit,
    /// A folder (or file) was dropped on the window.
    Dropped(PathBuf),
    /// The page saved a theme setting, to apply to the window.
    SetTheme(settings::Theme),
    /// A menu command was chosen; the page carries it out.
    #[cfg(target_os = "macos")]
    Menu(crate::menu::Command),
    /// A run changed (`run.rs`); the page fetches what it has not seen.
    RunChanged,
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
            "theme:system" => Some(Self::SetTheme(settings::Theme::System)),
            "theme:light" => Some(Self::SetTheme(settings::Theme::Light)),
            "theme:dark" => Some(Self::SetTheme(settings::Theme::Dark)),
            _ => body
                .strip_prefix("bench:")
                .map(|report| Self::BenchDone(report.to_owned())),
        }
    }
}

/// The site the page talks to, with the start-up project open (see [`startup_repo`]), and
/// the settings the window starts with.
fn start_site(dir: Option<&Path>, bench: Option<&str>, notify: Notify) -> Result<(Site, Settings)> {
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
    let site = Site::new(config.as_deref(), bench, agents, notify);
    if let Some(repo) = startup_repo(dir, &settings, site.recent_file.as_deref())? {
        site.open(repo);
    }
    // A benchmark of a file that cannot be read would time an empty pane and report it as a
    // result. Fail here instead, before the window opens.
    if let Some(file) = bench {
        site.repo()?.read(file)?;
    }
    Ok((site, settings))
}

fn serve(dir: Option<&Path>, bench: Option<&str>) -> Result<Option<BenchReport>> {
    let started = Instant::now();
    let mut event_loop: EventLoop<AppEvent> = EventLoopBuilder::with_user_event().build();
    let run_proxy = Mutex::new(event_loop.create_proxy());
    // Set from a nudge until the page is told: an agent streams its reply in many small
    // parts, and the page fetches everything new at once, so one call covers them all.
    let nudged = Arc::new(AtomicBool::new(false));
    let unnudge = Arc::clone(&nudged);
    let notify: Notify = Arc::new(move || {
        // A nudge is skipped only when the swap reads `true`: a call is pending, and the
        // event loop clears the flag before it calls the page, whose fetch then sees what was
        // recorded before this swap. SeqCst puts the swap and that store in one order, so a
        // swap that read `true` came before the store, and the fetch after it.
        if !nudged.swap(true, Ordering::SeqCst) {
            // Fails only once the event loop is gone, when no page is left to tell.
            drop(lock(&run_proxy).send_event(AppEvent::RunChanged));
        }
    });
    let (site, settings) = start_site(dir, bench, notify)?;

    let window = build_window(&event_loop, settings.theme)?;

    // The menu bar lives as long as this function, which is as long as the app.
    #[cfg(target_os = "macos")]
    let _menu = {
        let menu_proxy = event_loop.create_proxy();
        muda::MenuEvent::set_event_handler(Some(move |event: muda::MenuEvent| {
            if let Some(command) = crate::menu::Command::from_id(event.id.as_ref()) {
                // Fails only once the event loop is gone.
                drop(menu_proxy.send_event(AppEvent::Menu(command)));
            }
        }));
        crate::menu::install()
    };

    let committed = Rc::new(Cell::new(false));
    let webview = build_webview(&event_loop, &window, site, Rc::clone(&committed))?;

    let page = Page {
        webview: &webview,
        committed: &committed,
        quit: event_loop.create_proxy(),
    };
    let mut launch_to_paint = None;
    let mut report = None;
    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // The close button does what Close Window does, so a draft note stops it too.
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => ask_page(&page, "close-window", Unanswered::Quit),
            Event::UserEvent(AppEvent::Quit) => *control_flow = ControlFlow::Exit,
            // The system switched between light and dark while the window follows it.
            Event::WindowEvent {
                event: WindowEvent::ThemeChanged(theme),
                ..
            } => drop(webview.set_background_color(canvas_color(theme))),
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
            Event::UserEvent(AppEvent::SetTheme(theme)) => {
                window.set_theme(window_theme(theme));
                // Behind the page, seen where it has not painted yet, such as during a resize.
                // Fails only when the webview is being torn down.
                drop(webview.set_background_color(canvas_color(window.theme())));
            }
            #[cfg(target_os = "macos")]
            Event::UserEvent(AppEvent::Menu(command)) => {
                // The app must stay closable whatever state the page is in. The cost: an
                // exception in the page's draft guard quits and loses the draft, accepted over
                // a window that cannot be closed.
                let unanswered = match command {
                    Command::CloseWindow | Command::Quit => Unanswered::Quit,
                    Command::OpenFolder
                    | Command::SwitchProject
                    | Command::GoToFile
                    | Command::Settings
                    | Command::FocusMode => Unanswered::Ignore,
                };
                ask_page(&page, command.id(), unanswered);
            }
            Event::UserEvent(AppEvent::RunChanged) => {
                // Cleared before the page is told, so a change during its fetch nudges again.
                unnudge.store(false, Ordering::SeqCst);
                // A page that has not loaded yet fetches the run when it starts. Fails only
                // when the webview is being torn down.
                drop(webview.evaluate_script("window.postil?.runChanged?.()"));
            }
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

/// Which drags over the window are the app's. A drag that carries files is: the webview
/// would navigate to a dropped file. Any other drag (text into the note composer) is left to
/// the webview, so the handler answers `true` only while files are over the window.
#[derive(Debug, Default)]
struct FileDrag {
    /// Whether the drag now over the window carries files.
    carries_files: bool,
}

/// What the drag handler does with one event.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
enum DragOutcome {
    /// Leave the event to the webview.
    Pass,
    /// Keep the event from the webview.
    Take,
    /// Keep the drop from the webview and open this path: the first folder dropped, else the
    /// first file, which the page reports as not a folder.
    Open(PathBuf),
}

impl FileDrag {
    fn handle(&mut self, event: DragDropEvent) -> DragOutcome {
        let take_if = |files| {
            if files {
                DragOutcome::Take
            } else {
                DragOutcome::Pass
            }
        };
        match event {
            DragDropEvent::Enter { paths, .. } => {
                self.carries_files = !paths.is_empty();
                take_if(self.carries_files)
            }
            DragDropEvent::Over { .. } => take_if(self.carries_files),
            DragDropEvent::Drop { paths, .. } => {
                self.carries_files = false;
                // A project is one folder: the first folder dropped opens, or, when only files
                // were dropped, the first file, which the page then reports as not a folder.
                paths
                    .iter()
                    .find(|path| path.is_dir())
                    .or_else(|| paths.first())
                    .map_or(DragOutcome::Pass, |path| DragOutcome::Open(path.clone()))
            }
            DragDropEvent::Leave => take_if(std::mem::take(&mut self.carries_files)),
            _ => DragOutcome::Pass,
        }
    }
}

/// Hands a chosen or dropped folder to the page, which opens it through `/api/open`: the
/// page decides what to show for a folder that is not a repository.
fn offer_folder(webview: &WebView, folder: &Path) {
    let path = serde_json::to_string(&folder.to_string_lossy())
        .expect("a string always serializes to JSON");
    // Fails only when the webview is being torn down, when there is no page to tell.
    drop(webview.evaluate_script(&format!("window.postil?.openFolder({path})")));
}

/// The webview that shows the page in `window`, served by `site`. It sets `committed` once
/// the page's first navigation commits (see [`Page`]).
fn build_webview(
    event_loop: &EventLoop<AppEvent>,
    window: &Window,
    site: Site,
    committed: Rc<Cell<bool>>,
) -> Result<WebView> {
    let page_proxy = event_loop.create_proxy();
    let drop_proxy = event_loop.create_proxy();
    let file_drag = RefCell::new(FileDrag::default());
    WebViewBuilder::new()
        .with_background_color(canvas_color(window.theme()))
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
        .with_drag_drop_handler(move |event| {
            match file_drag.borrow_mut().handle(event) {
                DragOutcome::Pass => false,
                DragOutcome::Take => true,
                DragOutcome::Open(path) => {
                    // As above: fails only once the event loop is gone.
                    drop(drop_proxy.send_event(AppEvent::Dropped(path)));
                    true
                }
            }
        })
        // The page never navigates. Should it ever be sent elsewhere, the other origin must
        // not reach the `postil://` handlers, which read and write the repository.
        .with_navigation_handler(|url| url.starts_with("postil://"))
        // On macOS, wry fires `Started` from `didCommitNavigation`, before it runs the scripts
        // it queued and stops queueing (wry 0.57.3, `wkwebview/navigation.rs`,
        // `did_commit_navigation`); its docs say only "started loading", and other platforms'
        // backends are unchecked. `ask_page` relies on that order, and no test can check it
        // without a window: recheck it when bumping wry or adding a platform.
        .with_on_page_load_handler(move |event, _| {
            if matches!(event, PageLoadEvent::Started) {
                committed.set(true);
            }
        })
        .with_url("postil://localhost/")
        .build(window)
        .map_err(Error::Webview)
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
    if !settings.reopen_last_project {
        return Ok(None);
    }
    let recent = recent_file.map_or_else(Vec::new, recent::list);
    let last = recent.first();
    Ok(last.and_then(|root| Repo::open(Path::new(root)).ok()))
}

/// What to do when the page does not carry out a command it was asked to.
#[derive(Debug, Clone, Copy)]
enum Unanswered {
    /// Quit anyway: for Quit and Close Window, so the app can always be closed.
    Quit,
    /// Nothing: the command was only a convenience.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(dead_code, reason = "only the macOS menu bar sends such commands")
    )]
    Ignore,
}

/// The page, for [`ask_page`].
struct Page<'a> {
    webview: &'a WebView,
    /// Whether the page's first navigation has committed. Until then wry's macOS backend
    /// queues a script and drops its callback (wry 0.57.3, `wkwebview::eval`), so an answer
    /// could never come.
    committed: &'a Cell<bool>,
    /// Sends `AppEvent::Quit` when the page cannot answer.
    quit: EventLoopProxy<AppEvent>,
}

/// Asks the page to carry out the menu command `id` (`window.postil.menu` in ui/app.js), so
/// its guards apply, such as a draft note stopping Quit; the page posts `quit` itself when
/// nothing stops it.
///
/// The script ends in `true`, which WebKit hands back only when it ran to the end. An
/// exception, a page whose app.js did not load (no `window.postil`), or a web content process
/// that is gone hands back an empty string instead, and `unanswered` decides. Before the
/// page's first navigation commits there is no answer to wait for, and no draft note to
/// protect either (app.js has not run), so `unanswered` decides at once.
fn ask_page(page: &Page<'_>, id: &'static str, unanswered: Unanswered) {
    let proxy = page.quit.clone();
    let carried_out = move |done: bool| {
        if !done && matches!(unanswered, Unanswered::Quit) {
            // Fails only once the event loop is gone, when the app is quitting anyway.
            drop(proxy.send_event(AppEvent::Quit));
        }
    };
    if !page.committed.get() {
        carried_out(false);
        return;
    }
    // `id` is one of the fixed ASCII command names, safe inside quotes.
    let script = format!("window.postil.menu(\"{id}\"); true");
    let answered = move |answer: String| carried_out(answer == "true");
    // Fails only when the webview is being torn down, when there is no page to ask.
    drop(
        page.webview
            .evaluate_script_with_callback(&script, answered),
    );
}

/// The colour painted behind the page in `theme`: the page's `--canvas`.
fn canvas_color(theme: Theme) -> (u8, u8, u8, u8) {
    match theme {
        Theme::Dark => DARK_CANVAS,
        _ => LIGHT_CANVAS,
    }
}

/// The window appearance for a theme setting; `None` follows the system.
fn window_theme(theme: settings::Theme) -> Option<Theme> {
    match theme {
        settings::Theme::System => None,
        settings::Theme::Light => Some(Theme::Light),
        settings::Theme::Dark => Some(Theme::Dark),
    }
}

/// Builds the window. Its appearance also sets the page's: WebKit answers the page's
/// `prefers-color-scheme` from the window, so `ui/app.css` needs no theme of its own.
fn build_window(event_loop: &EventLoop<AppEvent>, theme: settings::Theme) -> Result<Window> {
    let builder = WindowBuilder::new()
        .with_theme(window_theme(theme))
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
    /// directory is unknown, which turns the list off. `config_dir` fails only with
    /// `Error::NoHome`, which is the reason `open` gives for a `None` here.
    recent_file: Option<PathBuf>,
    /// Where the settings are kept (`settings.rs`); `None`, for the same reason as
    /// `recent_file`, turns them off: the defaults apply and saving fails.
    settings_file: Option<PathBuf>,
    /// Where the pane widths are kept (`layout.rs`); `None`, for the same reason as
    /// `recent_file`, keeps them in each window only, until it closes.
    layout_file: Option<PathBuf>,
    /// Where agents' programs are looked for, found in the background.
    agents: Scanner,
    /// Tells the page a run changed (`run.rs`).
    notify: Notify,
}

/// The open project.
#[derive(Clone)]
struct Project {
    repo: Arc<Repo>,
    /// The project's runs and the agent kept between them.
    runner: Arc<Runner>,
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

/// The reply to `/api/settings`.
#[derive(Serialize)]
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveSettings {
    /// The revision of the settings file the change started from.
    revision: String,
    settings: Settings,
}

/// An agent as the settings dialog lists it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentView<'a> {
    name: &'a str,
    command: &'a str,
    built_in: bool,
    /// Where the agent's program is; `None` when it is not installed.
    found: Option<String>,
}

/// The reply to `GET /api/layout`.
#[derive(Serialize)]
struct LayoutView {
    #[serde(flatten)]
    layout: Layout,
    /// The widths a double-click on a divider resets to.
    defaults: Layout,
    limits: LayoutLimits,
}

/// The smallest and largest width of each resizable pane, in CSS px.
#[derive(Serialize)]
struct LayoutLimits {
    sidebar: [u32; 2],
    tray: [u32; 2],
}

/// The body of `POST /api/run`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendRun {
    /// The agent's name; the settings' agent when absent.
    agent: Option<String>,
    /// The ids of the notes to send, in order.
    notes: Vec<String>,
}

/// The body of `POST /api/run/permission`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerPermission {
    /// The request's number, as the run's log gives it (the agent's, counting on across the
    /// turns of a kept agent).
    request: usize,
    /// The id of the option chosen, one the agent offered.
    option: String,
}

/// The body of `POST /api/run/review`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRun {
    /// The run to review, `R3`.
    of: String,
}

/// The body of `PATCH /api/notes?id=N1`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditNote {
    /// The note's new text.
    body: String,
}

/// The body of `POST /api/run/follow`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowRun {
    /// The run to follow up, the latest of its thread: `R3`.
    of: String,
    /// What the engineer asks.
    text: String,
}

/// The body of `POST /api/run/question`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerQuestion {
    /// The question's number, as the run's log gives it.
    request: usize,
    /// The form's values by field, or `null` to skip the question.
    content: Option<serde_json::Map<String, serde_json::Value>>,
}

/// The reply to `/api/run/preview`.
#[derive(Serialize)]
struct PreviewView {
    /// The prompt's text, exactly as the agent will receive it.
    prompt: String,
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

// The webview owns the site and drops it when the window closes. A runner whose agent is
// still starting is also held by its start thread, so dropping the project alone would not
// stop that agent: the site stops it here, and quitting leaves no agent behind.
impl Drop for Site {
    fn drop(&mut self) {
        if let Some(project) = lock(&self.project).take() {
            project.runner.close();
        }
    }
}

impl Site {
    /// A site with no project open, keeping its files in `config` (`None` turns the recent
    /// list and the settings off).
    fn new(config: Option<&Path>, bench: Option<&str>, agents: Scanner, notify: Notify) -> Self {
        Self {
            project: Mutex::new(None),
            highlighter: Highlighter::new(),
            bench: bench.map(str::to_owned),
            recent_file: config.map(|dir| dir.join("recent.json")),
            settings_file: config.map(|dir| dir.join(SETTINGS_FILE)),
            layout_file: config.map(|dir| dir.join("layout.json")),
            agents,
            notify,
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
        let repo = Arc::new(repo);
        let runner = Runner::new(
            Arc::clone(&repo),
            self.agents.clone(),
            Arc::clone(&self.notify),
        );
        let old = lock(&self.project).replace(Project {
            repo,
            runner,
            warning,
        });
        // Stops the agent of the project this one replaces.
        if let Some(old) = old {
            old.runner.close();
        }
    }

    fn respond(&self, request: &Request<Vec<u8>>) -> Reply {
        let uri = request.uri();
        let post = *request.method() == Method::POST;
        if let Some(&(_, kind, body)) = PAGE.iter().find(|(path, ..)| *path == uri.path()) {
            return reply(StatusCode::OK, kind, body);
        }
        match uri.path() {
            "/api/repo" => json_or_error(self.repo_info()),
            "/api/recent" => json_or_error(Ok(self.recent())),
            "/api/open" if post => json_or_error(self.open_folder(request.body(), Repo::open)),
            "/api/init" if post => json_or_error(self.open_folder(request.body(), Repo::init)),
            "/api/notes" => self.respond_notes(request),
            "/api/notes/resolve" if post => self.change_note(uri, notes::resolve),
            "/api/notes/reopen" if post => self.change_note(uri, notes::reopen),
            "/api/folders" => match *request.method() {
                Method::GET => json_or_error(
                    self.repo()
                        .map(|repo| to_json(&folders::load(&repo.folders_file()))),
                ),
                Method::PUT => json_or_error(self.save_folders(request.body())),
                _ => reply(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "text/plain",
                    &b"GET the open folders or PUT them"[..],
                ),
            },
            "/api/settings" => match *request.method() {
                Method::GET => reply(StatusCode::OK, "application/json", self.settings()),
                Method::PUT => json_or_error(self.save_settings(request.body())),
                _ => reply(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "text/plain",
                    &b"GET the settings or PUT new ones"[..],
                ),
            },
            "/api/runs" => match *request.method() {
                Method::GET => json_or_error(
                    self.runner()
                        .and_then(|runner| runner.ids())
                        .map(|ids| to_json(&serde_json::json!({ "runs": ids }))),
                ),
                _ => reply(StatusCode::METHOD_NOT_ALLOWED, "text/plain", &b"GET"[..]),
            },
            // `respond_run` answers "not found" for a path under it that it does not serve.
            path if path == "/api/run" || path.starts_with("/api/run/") => {
                self.respond_run(request)
            }
            "/api/layout" => match *request.method() {
                Method::GET => reply(StatusCode::OK, "application/json", self.layout()),
                Method::PUT => json_or_error(self.save_layout(request.body())),
                _ => reply(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "text/plain",
                    &b"GET the pane widths or PUT new ones"[..],
                ),
            },
            "/api/settings/reset" if post => json_or_error(self.reset_settings()),
            "/api/agents/rescan" if post => {
                self.agents.rescan();
                reply(StatusCode::OK, "application/json", self.settings())
            }
            "/api/notes/resolve" | "/api/notes/reopen" => reply(
                StatusCode::METHOD_NOT_ALLOWED,
                "text/plain",
                &b"POST ?id=<note id> with no body"[..],
            ),
            "/api/settings/reset" | "/api/agents/rescan" => reply(
                StatusCode::METHOD_NOT_ALLOWED,
                "text/plain",
                &b"POST with no body"[..],
            ),
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
        let Some(Project { repo, warning, .. }) = lock(&self.project).clone() else {
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

    /// The reply to `PUT /api/folders` `{"open": ["src"]}`: saves the folders open in the file
    /// list, and answers `{}`.
    fn save_folders(&self, body: &[u8]) -> Result<Vec<u8>> {
        let open: Folders = serde_json::from_slice(body)
            .map_err(|error| Error::BadRequest(format!("unreadable request: {error}")))?;
        folders::save(&self.repo()?.folders_file(), open)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `PATCH /api/notes?id=N1` `{"body": "..."}`: replaces the note's text, saves,
    /// and returns all notes.
    fn edit_note(&self, id: &str, body: &[u8]) -> Result<Vec<u8>> {
        let edit: EditNote = serde_json::from_slice(body)
            .map_err(|error| Error::InvalidNote(format!("unreadable request: {error}")))?;
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
    fn change_note(&self, uri: &Uri, change: fn(&Repo, &str) -> Result<Review>) -> Reply {
        let Some(id) = query_value(uri.query().unwrap_or(""), "id") else {
            return reply(
                StatusCode::BAD_REQUEST,
                "text/plain",
                &b"expected ?id=<note id>"[..],
            );
        };
        json_or_error(
            self.repo()
                .and_then(|repo| change(&repo, &id))
                .map(|review| to_json(&review)),
        )
    }

    /// The replies to `/api/notes`: GET the notes, POST a new one, PATCH `?id=N1` its text, or
    /// DELETE `?id=N1`.
    fn respond_notes(&self, request: &Request<Vec<u8>>) -> Reply {
        let id = query_value(request.uri().query().unwrap_or(""), "id");
        match (request.method(), id) {
            (&Method::GET, _) => json_or_error(self.notes()),
            (&Method::POST, _) => json_or_error(self.add_note(request.body())),
            (&Method::DELETE, Some(id)) => json_or_error(self.delete_note(&id)),
            (&Method::PATCH, Some(id)) => json_or_error(self.edit_note(&id, request.body())),
            (&Method::DELETE | &Method::PATCH, None) => reply(
                StatusCode::BAD_REQUEST,
                "text/plain",
                &b"expected ?id=<note id>"[..],
            ),
            _ => reply(
                StatusCode::METHOD_NOT_ALLOWED,
                "text/plain",
                &b"GET the notes, POST a new one, PATCH ?id=N1 its text, or DELETE ?id=N1"[..],
            ),
        }
    }

    /// The replies to `/api/run` and the routes under it.
    fn respond_run(&self, request: &Request<Vec<u8>>) -> Reply {
        let uri = request.uri();
        let post = *request.method() == Method::POST;
        match uri.path() {
            "/api/run" => match *request.method() {
                Method::GET => json_or_error(self.run_view(uri.query().unwrap_or(""))),
                Method::POST => json_or_error(self.send_run(request.body())),
                _ => reply(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "text/plain",
                    &b"GET the latest run (?after=<entries seen>) or one run (?id=R1), \
                       or POST a new one"[..],
                ),
            },
            "/api/run/preview" if *request.method() != Method::GET => reply(
                StatusCode::METHOD_NOT_ALLOWED,
                "text/plain",
                &b"GET ?notes=N1,N2"[..],
            ),
            "/api/run/preview" => match query_value(uri.query().unwrap_or(""), "notes") {
                Some(ids) => json_or_error(self.preview_run(&ids)),
                None => reply(
                    StatusCode::BAD_REQUEST,
                    "text/plain",
                    &b"expected ?notes=N1,N2"[..],
                ),
            },
            "/api/run/changes" if *request.method() == Method::GET => {
                json_or_error(self.changes(uri.query().unwrap_or("")))
            }
            "/api/run/cancel" if post => json_or_error(
                self.runner()
                    .and_then(|runner| runner.cancel())
                    .map(|()| b"{}".to_vec()),
            ),
            "/api/run/permission" if post => json_or_error(self.answer_permission(request.body())),
            "/api/run/question" if post => json_or_error(self.answer_question(request.body())),
            "/api/run/review" if post => json_or_error(self.review_run(request.body())),
            "/api/run/follow" if post => json_or_error(self.follow_up(request.body())),
            "/api/run/cancel"
            | "/api/run/permission"
            | "/api/run/question"
            | "/api/run/review"
            | "/api/run/follow" => {
                reply(StatusCode::METHOD_NOT_ALLOWED, "text/plain", &b"POST"[..])
            }
            "/api/run/changes" => reply(StatusCode::METHOD_NOT_ALLOWED, "text/plain", &b"GET"[..]),
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
        let layout: Layout = serde_json::from_slice(body)
            .map_err(|error| Error::BadRequest(format!("unreadable layout: {error}")))?;
        let file = self.layout_file.as_deref().ok_or(Error::NoHome)?;
        layout::save(file, &layout)?;
        Ok(b"{}".to_vec())
    }

    /// The open project's runner.
    fn runner(&self) -> Result<Arc<Runner>> {
        lock(&self.project)
            .as_ref()
            .map(|project| Arc::clone(&project.runner))
            .ok_or(Error::NoProject)
    }

    /// The reply to `GET /api/run`: with `?id=R2`, that run whole; else the latest run with its
    /// entries from number `?after=` on, or `null` when the project has none.
    fn run_view(&self, query: &str) -> Result<Vec<u8>> {
        let runner = self.runner()?;
        match query_value(query, "id") {
            Some(id) => Ok(to_json(&runner.view_of(&id)?)),
            None => Ok(to_json(&runner.view(after_of(query)?)?)),
        }
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
        let request: SendRun = serde_json::from_slice(body)
            .map_err(|error| Error::BadRequest(format!("unreadable request: {error}")))?;
        let agent = self.agent_choice(request.agent)?;
        self.runner()?.send(agent, &request.notes)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `GET /api/run/preview?notes=N1,N2`.
    fn preview_run(&self, ids: &str) -> Result<Vec<u8>> {
        let ids: Vec<String> = ids.split(',').map(str::to_owned).collect();
        let prompt = self.runner()?.preview(&ids)?;
        Ok(to_json(&PreviewView { prompt }))
    }

    /// The reply to `POST /api/run/permission`.
    fn answer_permission(&self, body: &[u8]) -> Result<Vec<u8>> {
        let answer: AnswerPermission = serde_json::from_slice(body)
            .map_err(|error| Error::BadRequest(format!("unreadable request: {error}")))?;
        self.runner()?.answer(answer.request, &answer.option)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/run/review`: starts the settings' reviewers on the run, in
    /// order, and answers `{}`.
    fn review_run(&self, body: &[u8]) -> Result<Vec<u8>> {
        let review: ReviewRun = serde_json::from_slice(body)
            .map_err(|error| Error::BadRequest(format!("unreadable request: {error}")))?;
        let names = self
            .settings_file
            .as_deref()
            .map_or_else(Settings::default, |file| settings::load(file).settings)
            .reviewers;
        let reviewers = names
            .into_iter()
            .map(|name| self.agent_choice(Some(name)))
            .collect::<Result<Vec<_>>>()?;
        self.runner()?.review(&review.of, reviewers)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/run/follow`: sends the engineer's follow-up to the agent that
    /// ran the run it follows, and answers `{}`.
    fn follow_up(&self, body: &[u8]) -> Result<Vec<u8>> {
        let follow: FollowRun = serde_json::from_slice(body)
            .map_err(|error| Error::BadRequest(format!("unreadable request: {error}")))?;
        let runner = self.runner()?;
        let agent = self.agent_choice(Some(runner.agent_of(&follow.of)?))?;
        runner.follow_up(&follow.of, agent, &follow.text)?;
        Ok(b"{}".to_vec())
    }

    /// The reply to `POST /api/run/question`.
    fn answer_question(&self, body: &[u8]) -> Result<Vec<u8>> {
        let answer: AnswerQuestion = serde_json::from_slice(body)
            .map_err(|error| Error::BadRequest(format!("unreadable request: {error}")))?;
        self.runner()?.reply(answer.request, answer.content)?;
        Ok(b"{}".to_vec())
    }

    /// The agent called `name`, or the settings' agent, with its command. It must be turned
    /// on in the settings.
    fn agent_choice(&self, name: Option<String>) -> Result<AgentChoice> {
        let settings = self
            .settings_file
            .as_deref()
            .map_or_else(Settings::default, |file| settings::load(file).settings);
        let name = name.or(settings.agent).ok_or_else(|| {
            Error::BadRequest("choose the agent notes are sent to in Settings".to_owned())
        })?;
        if !settings.enabled_agents.contains(&name) {
            return Err(Error::BadRequest(format!(
                "turn {name} on in Settings before sending to it"
            )));
        }
        let built_in = BUILT_IN
            .iter()
            .find(|agent| agent.name == name)
            .map(|agent| agent.command.to_owned());
        let custom = || {
            settings
                .custom_agents
                .into_iter()
                .find(|agent| agent.name == name)
                .map(|agent| agent.command)
        };
        let command = built_in
            .or_else(custom)
            .ok_or_else(|| Error::BadRequest(format!("there is no agent called {name}")))?;
        Ok(AgentChoice { name, command })
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
        let request: SaveSettings = serde_json::from_slice(body)
            .map_err(|error| Error::InvalidSettings(format!("unreadable request: {error}")))?;
        let file = self.settings_file.as_deref().ok_or(Error::NoHome)?;
        settings::save(file, &request.settings, &request.revision)?;
        Ok(self.settings())
    }

    /// The reply to `POST /api/settings/reset`: writes the defaults over the file.
    fn reset_settings(&self) -> Result<Vec<u8>> {
        settings::reset(self.settings_file.as_deref().ok_or(Error::NoHome)?)?;
        Ok(self.settings())
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
    let custom = settings.custom_agents.iter().map(|agent| AgentView {
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
                | Error::SettingsLocked(_)
                | Error::SettingsChanged
                | Error::TurnRunning
                | Error::RunElsewhere => StatusCode::CONFLICT,
                Error::NoSuchNote { .. } | Error::NoSuchRequest | Error::NoSuchRun { .. } => {
                    StatusCode::NOT_FOUND
                }
                // The agent, the server behind this one, failed.
                Error::AgentNotFound { .. }
                | Error::StartAgent { .. }
                | Error::AgentSilent { .. }
                | Error::AgentExited { .. }
                | Error::AgentRefused { .. }
                | Error::AgentProtocol(_) => StatusCode::BAD_GATEWAY,
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
                | Error::WriteSettings { .. }
                | Error::WriteRun { .. }
                | Error::CopyIndex { .. }
                | Error::WriteFolders { .. }
                | Error::WriteLayout { .. }
                | Error::ReadRun { .. }
                | Error::NoHome => StatusCode::INTERNAL_SERVER_ERROR,
            };
            reply(status, "text/plain", error_chain(&error).into_bytes())
        }
    }
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    // The page's files, as the tests that check its links and colours read them.
    const INDEX_HTML: &str = include_str!("../ui/index.html");
    const APP_JS: &str = include_str!("../ui/app.js");
    const APP_CSS: &str = include_str!("../ui/app.css");

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
        assert_eq!(stored["version"], 3);
        assert_eq!(stored["notes"], review["notes"]);
        assert_eq!(note["state"], serde_json::json!({ "kind": "open" }));

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
        // A version 1 file has no states: its notes read as open, and the write that added
        // N6 made it version 3.
        let stored: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(stored["version"], 3);
        assert_eq!(
            stored["notes"][0]["state"],
            serde_json::json!({ "kind": "open" })
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

    fn patch(site: &Site, uri: &str, body: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .method(Method::PATCH)
            .uri(uri)
            .body(body.as_bytes().to_vec())
            .unwrap();
        let reply = site.respond(&request);
        let body = String::from_utf8(reply.body().to_vec()).unwrap();
        (reply.status(), body)
    }

    #[test]
    fn a_note_s_text_is_edited_in_place_and_a_blank_or_unknown_one_is_refused() {
        let repo = TempRepo::new("notes-edit");
        repo.write("a.txt", "x\ny\n");
        let site = repo.site();
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");
        let file = site.repo().unwrap().review_file();
        assert_eq!(
            post(&site, &format!("{NOTES}/resolve?id=N2"), "").0,
            StatusCode::OK
        );

        let (status, body) = patch(
            &site,
            &format!("{NOTES}?id=N2"),
            r#"{"body": "  second  "}"#,
        );
        assert_eq!(status, StatusCode::OK, "{body}");
        let review: serde_json::Value = serde_json::from_str(&body).unwrap();
        let bodies: Vec<&str> = review["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["body"].as_str().unwrap())
            .collect();
        assert_eq!(bodies, ["one", "second"], "trimmed, and only N2");
        assert_eq!(
            review["notes"][1]["state"],
            serde_json::json!({ "kind": "resolved" }),
            "an edit changes the text alone"
        );
        assert_eq!(
            anchors(&review)[1],
            anchor("N2", 2, 2, "found"),
            "its lines stay"
        );
        assert!(fs::read_to_string(&file).unwrap().contains("\"second\""));

        let before = fs::read_to_string(&file).unwrap();
        for (uri, body, status, why) in [
            (
                format!("{NOTES}?id=N1"),
                r#"{"body": "   "}"#,
                StatusCode::BAD_REQUEST,
                "no text",
            ),
            (
                format!("{NOTES}?id=N9"),
                r#"{"body": "x"}"#,
                StatusCode::NOT_FOUND,
                "no note N9",
            ),
            (
                format!("{NOTES}?id=N1"),
                r#"{"text": "x"}"#,
                StatusCode::BAD_REQUEST,
                "unreadable",
            ),
            (
                NOTES.to_owned(),
                r#"{"body": "x"}"#,
                StatusCode::BAD_REQUEST,
                "expected ?id=",
            ),
        ] {
            let (got, reply) = patch(&site, &uri, body);
            assert_eq!(got, status, "{uri} {body}: {reply}");
            assert!(reply.contains(why), "{uri} {body}: {reply}");
        }
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            before,
            "nothing refused was written"
        );
    }

    #[test]
    fn a_deleted_note_is_gone_and_ids_above_the_highest_left_are_reused() {
        let repo = TempRepo::new("notes-delete");
        repo.write("a.txt", "x\ny\nz\n");
        let site = repo.site();
        for line in 1..=3 {
            post_note(&site, "a.txt", line, line, "note");
        }

        // Deleting N2 leaves N1 and N3 with their ids.
        let (status, body) = delete(&site, &format!("{NOTES}?id=N2"));
        assert_eq!(status, StatusCode::OK, "{body}");
        let review: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            anchors(&review),
            [anchor("N1", 1, 1, "found"), anchor("N3", 3, 3, "found")]
        );
        let stored = fs::read_to_string(site.repo().unwrap().review_file()).unwrap();
        assert!(!stored.contains("\"N2\""), "{stored}");

        // Once N3 is gone too, the highest left is N1: N2 is handed out again.
        delete(&site, &format!("{NOTES}?id=N3"));
        let review = post_note(&site, "a.txt", 2, 2, "again");
        assert_eq!(
            anchors(&review),
            [anchor("N1", 1, 1, "found"), anchor("N2", 2, 2, "found")]
        );
    }

    #[test]
    fn a_note_is_resolved_with_its_run_and_reopened_to_where_it_was() {
        let repo = TempRepo::new("notes-resolve");
        repo.write("a.txt", "a\nb\n");
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        let change = |action: &str, id: &str| {
            let (status, body) = post(&site, &format!("{NOTES}/{action}?id={id}"), "");
            assert_eq!(status, StatusCode::OK, "{action} {id}: {body}");
            let review: serde_json::Value = serde_json::from_str(&body).unwrap();
            review["notes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|note| note["state"].clone())
                .collect::<Vec<_>>()
        };
        let open = serde_json::json!({ "kind": "open" });
        let resolved_after_r1 = serde_json::json!({ "kind": "resolved", "run": "R1" });
        let resolved = serde_json::json!({ "kind": "resolved" });

        assert_eq!(
            change("resolve", "N1"),
            [resolved_after_r1.clone(), open.clone()]
        );
        assert_eq!(
            change("resolve", "N1"),
            [resolved_after_r1.clone(), open.clone()],
            "resolving twice changes nothing"
        );
        assert_eq!(
            change("resolve", "N2"),
            [resolved_after_r1, resolved.clone()]
        );

        // Only Reopen takes a note out of Resolved: a send is refused, and a run that takes
        // its turn after the engineer resolved the note leaves it resolved, with that run.
        let (status, body) = send(&site, &["N2"]);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("N2 is resolved: reopen it"), "{body}");
        notes::mark_sent(&site.repo().unwrap(), &["N1".to_owned()], "R9").unwrap();
        assert_eq!(
            states(&site),
            [
                serde_json::json!({ "kind": "resolved", "run": "R9" }),
                resolved.clone()
            ]
        );

        let sent = serde_json::json!({ "kind": "sent", "run": "R9" });
        assert_eq!(change("reopen", "N1"), [sent.clone(), resolved.clone()]);
        assert_eq!(
            change("reopen", "N1"),
            [sent.clone(), resolved],
            "reopening a sent note changes nothing"
        );
        assert_eq!(change("reopen", "N2"), [sent.clone(), open.clone()]);
        assert_eq!(
            change("reopen", "N2"),
            [sent, open],
            "reopening an open note changes nothing"
        );
        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(site.repo().unwrap().review_file()).unwrap()).unwrap();
        assert_eq!(stored["version"], 3);
    }

    #[test]
    fn changing_a_missing_or_malformed_id_says_so_and_changes_nothing() {
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
        for (uri, status) in [
            (format!("{NOTES}/resolve?id=N9"), StatusCode::NOT_FOUND),
            (format!("{NOTES}/reopen?id=nine"), StatusCode::BAD_REQUEST),
            (format!("{NOTES}/resolve"), StatusCode::BAD_REQUEST),
        ] {
            assert_eq!(post(&site, &uri, "").0, status, "{uri}");
        }
        for action in ["resolve", "reopen"] {
            assert_eq!(
                get(&site, &format!("{NOTES}/{action}?id=N1")).0,
                StatusCode::METHOD_NOT_ALLOWED,
                "only a POST changes a note: {action}"
            );
        }

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
            (
                r#"{"version": 4, "notes": []}"#,
                "version 4, but this build of Postil reads versions 1 to 3".to_owned(),
            ),
            (r#"{"version": 0, "notes": []}"#, "version 0".to_owned()),
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
            (
                &*format!(
                    r#"{{"version": 2, "notes": [{}]}}"#,
                    note.replace(
                        r#""body":"b""#,
                        r#""body":"b","state":{"kind":"open","run":"R1"}"#
                    )
                ),
                "unknown field `run`".to_owned(),
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

        for route in ["open", "init"] {
            let (status, _) = get(&site, &format!("postil://localhost/api/{route}"));
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{route}");
        }
    }

    #[test]
    fn a_folder_with_a_broken_git_link_gets_git_s_error_and_no_offer_to_init() {
        let repo = TempRepo::new("broken-gitlink");
        let site = repo.empty_site();
        let folder = plain_folder("broken-gitlink");
        // What a linked worktree looks like after its git directory was removed.
        fs::write(folder.join(".git"), "gitdir: /no/such/git/dir\n").unwrap();

        let (status, body) = post(&site, "postil://localhost/api/open", &open_request(&folder));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(body.starts_with("git rev-parse"), "{body}");
        let (status, _) = post(&site, "postil://localhost/api/init", &open_request(&folder));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(folder.join(".git").is_file(), "the link is left as it was");
        drop(fs::remove_dir_all(&folder));
    }

    fn drag(paths: &[&Path]) -> Vec<PathBuf> {
        paths.iter().map(|path| path.to_path_buf()).collect()
    }

    #[test]
    fn only_drags_that_carry_files_are_taken_from_the_webview() {
        let folder = plain_folder("drag");
        let file = folder.join("a.txt");
        fs::write(&file, "x").unwrap();
        let position = (0, 0);
        let mut drags = FileDrag::default();
        let over = DragDropEvent::Over { position };
        let enter = |paths| DragDropEvent::Enter { paths, position };
        let dropped = |paths| DragDropEvent::Drop { paths, position };

        // Text dragged into the composer: no paths, left to the webview throughout.
        assert_eq!(drags.handle(enter(Vec::new())), DragOutcome::Pass);
        assert_eq!(drags.handle(over.clone()), DragOutcome::Pass);

        // Files: taken from entering to leaving, and not after.
        assert_eq!(drags.handle(enter(drag(&[&file]))), DragOutcome::Take);
        assert_eq!(drags.handle(over.clone()), DragOutcome::Take);
        assert_eq!(drags.handle(DragDropEvent::Leave), DragOutcome::Take);
        assert_eq!(drags.handle(over.clone()), DragOutcome::Pass);

        // A drop opens the first folder among the items, else the first item, and ends the
        // drag: a later event is the webview's again.
        drags.handle(enter(drag(&[&file, &folder])));
        assert_eq!(
            drags.handle(dropped(drag(&[&file, &folder]))),
            DragOutcome::Open(folder.clone())
        );
        assert_eq!(drags.handle(over), DragOutcome::Pass);
        assert_eq!(
            drags.handle(dropped(drag(&[&file]))),
            DragOutcome::Open(file.clone())
        );
        assert_eq!(drags.handle(dropped(Vec::new())), DragOutcome::Pass);
        drop(fs::remove_dir_all(&folder));
    }

    #[test]
    fn a_recent_list_that_cannot_be_written_warns_and_the_project_still_opens() {
        let repo = TempRepo::new("recent-unwritable");
        // The list's path is a directory, so no file can be renamed onto it.
        fs::create_dir_all(repo.config().join("recent.json/inside")).unwrap();
        let site = repo.empty_site();

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
        let site = Site::new(None, None, no_agents(), quiet());

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
        let recent = repo.config().join("recent.json");
        let site = Site::new(Some(&repo.config()), Some("a.rs"), no_agents(), quiet());

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

        let site = Site::new(Some(&repo.config()), Some("src/a.rs"), no_agents(), quiet());
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

    // ---- Settings ------------------------------------------------------------------------

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

    #[test]
    fn open_folders_are_kept_per_working_tree_and_read_back() {
        let repo = TempRepo::new("folders");
        repo.write("src/a.rs", "fn a() {}\n");
        let site = repo.site();
        let folders = "postil://localhost/api/folders";
        assert_eq!(get_json(&site, folders), serde_json::json!({ "open": [] }));

        let (status, body) = put(&site, folders, r#"{"open": ["src", "src/gone"]}"#);
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            get_json(&repo.site(), folders),
            serde_json::json!({ "open": ["src", "src/gone"] }),
            "a new process reads them"
        );
        assert!(repo.root.join(".git/postil/folders.json").exists());

        let (status, body) = put(&site, folders, r#"{"open": "src"}"#);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("unreadable request"), "{body}");
        assert_eq!(post(&site, folders, "").0, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            get(&repo.empty_site(), folders).0,
            StatusCode::CONFLICT,
            "no project"
        );
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

    /// The settings view once the agent scan has finished, waiting at most five seconds.
    fn scanned_settings(site: &Site) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let view = get_json(site, SETTINGS);
            if !view["agents"].is_null() {
                return view;
            }
            assert!(Instant::now() < deadline, "the agent scan did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Settings that pass every rule and differ from the defaults in every field.
    fn custom_settings() -> serde_json::Value {
        serde_json::json!({
            "theme": "dark",
            "codeFontSize": 15,
            "tabWidth": 2,
            "reopenLastProject": true,
            "customAgents": [{ "name": "Local", "command": "local-agent --acp" }],
            "enabledAgents": ["Grok", "Local"],
            "agent": "Local",
            "reviewers": ["Grok", "Grok"],
        })
    }

    #[test]
    fn settings_start_as_the_defaults_and_saved_ones_survive_a_restart() {
        let repo = TempRepo::new("settings-saved");
        let file = repo.config().join("settings.json");
        let view = get_json(&repo.empty_site(), SETTINGS);
        assert_eq!(view["settings"]["theme"], "system");
        assert_eq!(view["settings"]["codeFontSize"], 13);
        assert_eq!(view["settings"]["tabWidth"], 4);
        assert_eq!(view["problem"], serde_json::Value::Null);
        assert_eq!(view["file"], file.to_str().unwrap());
        assert_eq!(view["fontSizes"], serde_json::json!([11, 20]));
        assert_eq!(view["tabWidths"], serde_json::json!([2, 4, 8]));
        assert!(!file.exists(), "reading the defaults writes nothing");

        let (status, body) = save(&repo.empty_site(), &custom_settings().to_string());
        assert_eq!(status, StatusCode::OK, "{body}");
        let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            saved["settings"],
            custom_settings(),
            "the reply is the new settings"
        );

        let restarted = get_json(&repo.empty_site(), SETTINGS);
        assert_eq!(restarted["settings"], custom_settings());
        let stored: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(stored["version"], 1);
        assert_eq!(stored["settings"], custom_settings());
    }

    #[test]
    fn settings_that_break_a_rule_are_refused_and_nothing_is_written() {
        let repo = TempRepo::new("settings-refused");
        let site = repo.empty_site();
        let file = repo.config().join("settings.json");
        let with = |field: &str, value: serde_json::Value| {
            let mut settings = custom_settings();
            settings[field] = value;
            settings.to_string()
        };
        let agent =
            |name: &str, command: &str| serde_json::json!([{ "name": name, "command": command }]);
        let cases = [
            (
                with("codeFontSize", 10.into()),
                "code font size must be 11 to 20",
            ),
            (
                with("codeFontSize", 21.into()),
                "code font size must be 11 to 20",
            ),
            (with("tabWidth", 3.into()), "tab width must be 2, 4 or 8"),
            (with("theme", "sepia".into()), "unknown variant `sepia`"),
            (
                with("customAgents", agent("claude", "x")),
                "already an agent called \"claude\"",
            ),
            (
                with("customAgents", agent(" Local", "x")),
                "cannot be empty or start or end",
            ),
            (
                with("customAgents", agent("", "x")),
                "cannot be empty or start or end",
            ),
            (
                with("customAgents", agent(&"a".repeat(41), "x")),
                "at most 40 characters",
            ),
            (
                with("customAgents", agent("Local", "  ")),
                "needs a command",
            ),
            (
                with("customAgents", agent("Local", "run \"a b\"")),
                "cannot hold quotes",
            ),
            (
                with("enabledAgents", serde_json::json!(["Nobody"])),
                "no agent called \"Nobody\"",
            ),
            (
                with("enabledAgents", serde_json::json!(["Grok", "Grok"])),
                "enabled twice",
            ),
            (
                with("agent", "Claude".into()),
                "notes cannot go to \"Claude\"",
            ),
            (
                with("reviewers", serde_json::json!(["Claude"])),
                "\"Claude\" cannot review",
            ),
            (with("colour", "red".into()), "unknown field `colour`"),
            ("not json".to_owned(), "unreadable request"),
        ];
        for (body, expected) in cases {
            let (status, reply) = save(&site, &body);
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
            assert!(reply.starts_with("cannot save the settings: "), "{reply}");
            assert!(reply.contains(expected), "{body}: {reply}");
        }
        let two_locals = serde_json::json!([
            { "name": "Local", "command": "a" },
            { "name": "LOCAL", "command": "b" },
        ]);
        let (status, reply) = save(&site, &with("customAgents", two_locals));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            reply.contains("already an agent called \"LOCAL\""),
            "{reply}"
        );
        assert!(!file.exists());

        // The limits themselves are allowed: a 40 character name, sizes 11 and 20.
        let longest = "a".repeat(40);
        let mut at_limits = custom_settings();
        at_limits["customAgents"] = serde_json::json!([{ "name": longest, "command": "x" }]);
        at_limits["enabledAgents"] = serde_json::json!([longest]);
        at_limits["agent"] = longest.into();
        at_limits["reviewers"] = serde_json::json!([]);
        for size in [11, 20] {
            at_limits["codeFontSize"] = size.into();
            let (status, reply) = save(&site, &at_limits.to_string());
            assert_eq!(status, StatusCode::OK, "{reply}");
        }
    }

    #[test]
    fn a_settings_file_that_cannot_be_used_is_kept_until_it_is_reset() {
        let repo = TempRepo::new("settings-broken");
        let site = repo.empty_site();
        let file = repo.config().join("settings.json");
        fs::create_dir_all(repo.config()).unwrap();
        let unusable = [
            (
                "{ \"version\": 1, \"settings\": { \"theme\": ",
                "is not valid settings JSON",
            ),
            (
                "{\"version\": 2, \"settings\": {}}",
                "written by a newer Postil (format version 2)",
            ),
            (
                "{\"version\": 0, \"settings\": {}}",
                "unknown format version 0",
            ),
            (
                "{\"version\": 1, \"settings\": {\"tabWidth\": 5}}",
                "breaks a rule: the tab width must be 2, 4 or 8, not 5",
            ),
        ];
        for (text, expected) in unusable {
            fs::write(&file, text).unwrap();
            let view = get_json(&site, SETTINGS);
            let problem = view["problem"].as_str().unwrap();
            assert!(problem.contains(expected), "{problem}");
            assert!(problem.contains("saving is off until"), "{problem}");
            assert_eq!(view["settings"]["tabWidth"], 4, "the defaults are in use");

            let (status, reply) = save(&site, &custom_settings().to_string());
            assert_eq!(status, StatusCode::CONFLICT, "{reply}");
            assert!(reply.contains(expected), "{reply}");
            assert_eq!(fs::read_to_string(&file).unwrap(), text, "left as it was");
        }

        let (status, reply) = post(&site, "postil://localhost/api/settings/reset", "");
        assert_eq!(status, StatusCode::OK, "{reply}");
        let view: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(view["problem"], serde_json::Value::Null);
        assert_eq!(view["settings"]["tabWidth"], 4);
        let (status, reply) = save(&site, &custom_settings().to_string());
        assert_eq!(status, StatusCode::OK, "saving works again: {reply}");
    }

    #[test]
    fn a_save_from_before_another_change_is_refused_and_the_other_change_kept() {
        let repo = TempRepo::new("settings-changed");
        let file = repo.config().join("settings.json");
        let (status, reply) = save(&repo.empty_site(), &custom_settings().to_string());
        assert_eq!(status, StatusCode::OK, "{reply}");

        // A window reads the settings; then a hand edit (or another window) changes them.
        let site = repo.empty_site();
        let stale = get_json(&site, SETTINGS)["revision"].clone();
        let edited = "{\"version\": 1, \"settings\": {\"theme\": \"light\"}}";
        fs::write(&file, edited).unwrap();

        let body = format!(
            "{{\"revision\": {stale}, \"settings\": {}}}",
            custom_settings()
        );
        let (status, reply) = put(&site, SETTINGS, &body);
        assert_eq!(status, StatusCode::CONFLICT, "{reply}");
        assert!(
            reply.contains("the file changed since this window read it"),
            "{reply}"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            edited,
            "the other change is kept"
        );

        // Read again, the window saves on top of the edit.
        let fresh = get_json(&site, SETTINGS);
        assert_eq!(fresh["settings"]["theme"], "light");
        assert_ne!(fresh["revision"], stale);
        let (status, reply) = save(&site, &custom_settings().to_string());
        assert_eq!(status, StatusCode::OK, "{reply}");
    }

    #[test]
    fn a_settings_file_written_by_hand_may_name_only_what_it_changes() {
        let repo = TempRepo::new("settings-partial");
        fs::create_dir_all(repo.config()).unwrap();
        fs::write(
            repo.config().join("settings.json"),
            "{\"version\": 1, \"settings\": {\"theme\": \"light\"}}",
        )
        .unwrap();
        let view = get_json(&repo.empty_site(), SETTINGS);
        assert_eq!(view["problem"], serde_json::Value::Null);
        assert_eq!(view["settings"]["theme"], "light");
        assert_eq!(view["settings"]["codeFontSize"], 13);
        assert_eq!(view["settings"]["enabledAgents"], serde_json::json!([]));
    }

    #[test]
    fn without_a_configuration_directory_settings_are_the_defaults_and_cannot_be_saved() {
        let site = Site::new(None, None, no_agents(), quiet());
        let view = get_json(&site, SETTINGS);
        assert_eq!(
            view["problem"],
            "settings are off: cannot find the home directory (is HOME set?)"
        );
        assert_eq!(view["file"], serde_json::Value::Null);
        let (status, reply) = save(&site, &custom_settings().to_string());
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply, "cannot find the home directory (is HOME set?)");
    }

    #[test]
    fn agents_are_found_on_the_search_path_and_custom_ones_by_their_command() {
        let repo = TempRepo::new("settings-agents");
        let bin = repo.root.join(".git/test-bin");
        fs::create_dir_all(&bin).unwrap();
        for program in ["grok", "local-agent"] {
            let path = bin.join(program);
            fs::write(&path, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The first scan finds `bin`; every later one finds nothing, so a rescan shows.
        let scans = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&scans);
        let first_path = bin.clone().into_os_string();
        let site = Site::new(
            Some(&repo.config()),
            None,
            Scanner::new(move || {
                let mut scans = lock(&counter);
                *scans += 1;
                if *scans == 1 {
                    first_path.clone()
                } else {
                    OsString::new()
                }
            }),
            quiet(),
        );
        let (status, body) = save(&site, &custom_settings().to_string());
        assert_eq!(status, StatusCode::OK, "{body}");

        let agents = scanned_settings(&site)["agents"].clone();
        let names: Vec<&str> = agents
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| agent["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["Claude", "Codex", "Gemini", "Grok", "OpenCode", "Local"]
        );
        let agent = |name: &str| {
            agents
                .as_array()
                .unwrap()
                .iter()
                .find(|agent| agent["name"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(agent("Grok")["found"], bin.join("grok").to_str().unwrap());
        assert_eq!(agent("Grok")["command"], "grok agent stdio");
        assert_eq!(agent("Grok")["builtIn"], true);
        assert_eq!(agent("Claude")["found"], serde_json::Value::Null);
        assert_eq!(
            agent("Local")["found"],
            bin.join("local-agent").to_str().unwrap()
        );
        assert_eq!(agent("Local")["builtIn"], false);

        // A rescan starts over, and its result replaces the first one.
        let (status, body) = post(&site, "postil://localhost/api/agents/rescan", "");
        assert_eq!(status, StatusCode::OK, "{body}");
        let rescanned = scanned_settings(&site)["agents"].clone();
        let found: Vec<&serde_json::Value> = rescanned
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| &agent["found"])
            .collect();
        assert!(found.iter().all(|found| found.is_null()), "{rescanned}");
        assert_eq!(*lock(&scans), 2);

        // Reset and rescan change state, so they take POST only.
        for route in ["api/settings/reset", "api/agents/rescan"] {
            let (status, _) = get(&site, &format!("postil://localhost/{route}"));
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{route}");
        }
        assert_eq!(*lock(&scans), 2, "a refused rescan does not scan");
        assert_eq!(
            get_json(&site, SETTINGS)["settings"],
            custom_settings(),
            "nor reset"
        );
    }

    #[test]
    fn settings_that_cannot_be_read_or_written_say_so() {
        let repo = TempRepo::new("settings-io");
        // A directory where the settings file should be cannot be read as one.
        fs::create_dir_all(repo.config().join("settings.json")).unwrap();
        let site = repo.empty_site();
        let problem = get_json(&site, SETTINGS)["problem"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            problem.contains("settings.json cannot be read"),
            "{problem}"
        );
        let (status, reply) = save(&site, &custom_settings().to_string());
        assert_eq!(status, StatusCode::CONFLICT, "{reply}");
        assert!(
            reply.ends_with("(fix the file, or reset the settings)"),
            "{reply}"
        );

        // A read-only configuration directory: the file is missing, so saving may go ahead,
        // and then the write fails.
        let blocked = TempRepo::new("settings-unwritable");
        fs::create_dir_all(blocked.config()).unwrap();
        fs::set_permissions(blocked.config(), fs::Permissions::from_mode(0o555)).unwrap();
        let (status, reply) = save(&blocked.empty_site(), &custom_settings().to_string());
        // Writable again, so the repository's drop can remove it.
        fs::set_permissions(blocked.config(), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{reply}");
        assert!(reply.contains("the settings were not saved"), "{reply}");
    }

    #[test]
    fn every_page_module_is_served() {
        // The page imports its modules by name; one left out of PAGE stops the page.
        let site = Site::new(None, None, no_agents(), quiet());
        let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
        for entry in fs::read_dir(&ui).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let path = Path::new(&name);
            let script = path.extension().is_some_and(|extension| extension == "js");
            let test = path
                .file_stem()
                .is_some_and(|stem| stem.to_string_lossy().ends_with(".test"));
            if !script || test {
                continue;
            }
            let (status, body) = get(&site, &format!("postil://localhost/{name}"));
            assert_eq!(status, StatusCode::OK, "{name}");
            assert_eq!(body, fs::read_to_string(ui.join(&name)).unwrap(), "{name}");
        }
    }

    #[test]
    fn the_window_paints_the_page_s_canvas_colour_in_each_theme() {
        // app.css declares --canvas once for the dark palette (first) and once for the light
        // one; the window paints the same colour behind the page so nothing flashes.
        let hex: Vec<(u8, u8, u8, u8)> = APP_CSS
            .lines()
            .filter_map(|line| line.trim().strip_prefix("--canvas: #"))
            .map(|value| {
                let channel = |at: usize| u8::from_str_radix(&value[at..at + 2], 16).unwrap();
                (channel(0), channel(2), channel(4), 0xff)
            })
            .collect();
        assert_eq!(hex, [canvas_color(Theme::Dark), canvas_color(Theme::Light)]);
    }

    #[test]
    fn the_page_names_each_theme_as_the_settings_file_does() {
        // ui/app.js posts `theme:${settings.theme}`, the theme as api/settings sends it.
        for theme in [
            settings::Theme::System,
            settings::Theme::Light,
            settings::Theme::Dark,
        ] {
            let name = serde_json::to_value(theme).unwrap();
            let message = format!("theme:{}", name.as_str().unwrap());
            let Some(AppEvent::SetTheme(parsed)) = AppEvent::parse(&message) else {
                panic!("{message} is not a theme message");
            };
            assert_eq!(serde_json::to_value(parsed).unwrap(), name);
        }
    }

    #[test]
    fn the_last_project_reopens_only_when_the_settings_ask_and_it_still_opens() {
        let repo = TempRepo::new("settings-reopen");
        let other = TempRepo::new("settings-reopen-other");
        let recent = repo.config().join("recent.json");
        recent::record(&recent, Path::new(&site_root(&other.root))).unwrap();
        let reopen = Settings {
            reopen_last_project: true,
            ..Settings::default()
        };
        let root = |found: Option<Repo>| found.map(|repo| repo.root().to_owned());

        let opened = startup_repo(None, &reopen, Some(&recent)).unwrap();
        assert_eq!(root(opened), Some(PathBuf::from(site_root(&other.root))));
        let off = startup_repo(None, &Settings::default(), Some(&recent)).unwrap();
        assert_eq!(root(off), None, "off by default");
        let given = startup_repo(Some(&repo.root), &reopen, Some(&recent)).unwrap();
        assert_eq!(
            root(given),
            Some(PathBuf::from(site_root(&repo.root))),
            "a folder on the command line wins"
        );
        assert_eq!(root(startup_repo(None, &reopen, None).unwrap()), None);

        let other_root = other.root.clone();
        drop(other);
        assert!(!other_root.exists());
        let gone = startup_repo(None, &reopen, Some(&recent)).unwrap();
        assert_eq!(
            root(gone),
            None,
            "a project that is gone shows the welcome screen"
        );
    }

    // Runs (run.rs), against the fake ACP agent in tests/fake-agent.mjs.

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
    fn run_site(repo: &TempRepo, scenario: &str) -> (Site, mpsc::Receiver<()>) {
        let (sender, nudges) = mpsc::channel();
        // The receiver is gone once the test finished.
        let notify: Notify = Arc::new(move || sender.send(()).unwrap_or_default());
        let path = std::env::var_os("PATH").expect("PATH is set");
        let site = Site::new(
            Some(&repo.config()),
            None,
            Scanner::new(move || path.clone()),
            notify,
        );
        site.open(Repo::open(&repo.root).unwrap());
        set_agents(&site, &[("Fake", &fake_agent(scenario))], "Fake");
        (site, nudges)
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
    fn wait_for(site: &Site, nudges: &mpsc::Receiver<()>, status: &str) -> serde_json::Value {
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

    fn send(site: &Site, notes: &[&str]) -> (StatusCode, String) {
        post(
            site,
            RUN,
            &serde_json::json!({ "notes": notes }).to_string(),
        )
    }

    /// The kinds of a run's entries, each message with its text after a colon. Snapshots are
    /// left out, as in [`entries`].
    fn kinds(run: &serde_json::Value) -> Vec<String> {
        run["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["kind"] != "snapshot")
            .map(|entry| match entry["kind"].as_str().unwrap() {
                "message" => format!("message:{}", entry["text"].as_str().unwrap()),
                kind => kind.to_owned(),
            })
            .collect()
    }

    /// A run's entries without its snapshots: the tests of snapshots read them on their own,
    /// and the others count entries by place.
    fn entries(run: &serde_json::Value) -> Vec<serde_json::Value> {
        run["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["kind"] != "snapshot")
            .cloned()
            .collect()
    }

    /// The points of a run's snapshots, in order: `["before", "after"]` for a run that ended.
    fn snapshot_points(run: &serde_json::Value) -> Vec<serde_json::Value> {
        run["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["kind"] == "snapshot")
            .map(|entry| entry["point"].clone())
            .collect()
    }

    fn states(site: &Site) -> Vec<serde_json::Value> {
        get_json(site, NOTES)["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["state"].clone())
            .collect()
    }

    fn file_uri_of(repo: &TempRepo, path: &str) -> String {
        let root = Repo::open(&repo.root).unwrap().root().to_owned();
        format!("file://{}/{path}", root.display())
    }

    /// The run's first entry of `kind`.
    fn first_of_kind(run: &serde_json::Value, kind: &str) -> serde_json::Value {
        run["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kind"] == kind)
            .unwrap()
            .clone()
    }

    /// The message ids of the run's message entries.
    fn message_ids(run: &serde_json::Value) -> Vec<&str> {
        run["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["kind"] == "message")
            .map(|entry| entry["message"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn sent_notes_reach_the_agent_and_every_step_of_the_run_is_kept_on_disk() {
        let repo = TempRepo::new("run-turn");
        repo.write("src/a.rs", "fn a() {\n    one();\n}\n");
        repo.write("b.txt", "b\n");
        let (site, nudges) = run_site(&repo, "turn");
        post_note(&site, "src/a.rs", 2, 2, "rename one");
        post_note(&site, "b.txt", 1, 1, "drop b");
        post_note(&site, "src/a.rs", 1, 3, "not this one");

        assert_eq!(
            get(&site, RUN),
            (StatusCode::OK, "null".to_owned()),
            "no run yet"
        );
        let (status, body) = send(&site, &["N2", "N1"]);
        assert_eq!(status, StatusCode::OK, "{body}");

        // Postil answers the permission request itself, so the run goes to its end.
        let ended = wait_for(&site, &nudges, "ended");
        let prompt = ended["prompt"].as_str().unwrap();
        assert!(
            prompt.find("[N2] b.txt:1") < prompt.find("[N1] src/a.rs:2"),
            "the notes in the order chosen: {prompt}"
        );
        assert_eq!(
            kinds(&ended)[..3],
            [
                "started".to_owned(),
                format!(
                    "message:got {prompt} | {} | {}",
                    file_uri_of(&repo, "b.txt"),
                    file_uri_of(&repo, "src/a.rs")
                ),
                "message:prompt 1".to_owned(),
            ],
            "the prompt as previewed, then one link per file"
        );
        assert_eq!(ended["notes"], serde_json::json!(["N2", "N1"]));
        assert_eq!(ended["agent"], "Fake");
        assert_eq!(
            states(&site),
            [
                serde_json::json!({ "kind": "sent", "run": "R1" }),
                serde_json::json!({ "kind": "sent", "run": "R1" }),
                serde_json::json!({ "kind": "open" }),
            ],
            "only the notes sent are marked, with their run"
        );

        let permission = first_of_kind(&ended, "permission");
        assert_eq!(permission["toolCall"], "t1");
        assert_eq!(
            first_of_kind(&ended, "answer")["option"],
            "once",
            "allowed once, of the options allow once, allow always and reject"
        );
        assert_eq!(
            first_of_kind(&ended, "tool")["places"],
            serde_json::json!([{ "path": "src/a.rs", "line": 3 }]),
            "a tool's files are relative to the repository"
        );
        let kinds = kinds(&ended);
        assert_eq!(
            kinds[3..],
            [
                "message:fs answered -32601",
                "thought",
                "tool",
                "tool",
                "permission",
                "answer",
                "message:chose once",
                "toolUpdate",
                "plan",
                "ended",
            ]
        );
        let ids = message_ids(&ended);
        assert_eq!(
            ids,
            ["m1", "m2", "m3", "m4"],
            "each part keeps its message's id"
        );
        let update = &entries(&ended)[kinds.iter().position(|kind| kind == "toolUpdate").unwrap()];
        assert_eq!(
            update["output"],
            serde_json::json!([
                { "type": "diff", "path": "src/a.rs", "oldText": "a", "newText": "b" }
            ]),
            "paths inside the repository are shown relative to it"
        );
        let last = ended["entries"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["reason"], "end_turn");

        // The page asks only for what it has not seen.
        let total = ended["total"].as_u64().unwrap();
        let tail = get_json(&site, &format!("{RUN}?after={}", total - 1));
        assert_eq!(tail["entries"], serde_json::json!([last]));
        assert_eq!(tail["total"], total);

        drop(site);
        assert_read_back(&repo, &ended);
    }

    /// The log on disk holds `run`, as a new site (a restart) reads it back.
    fn assert_read_back(repo: &TempRepo, run: &serde_json::Value) {
        let read = get_json(&repo.site(), RUN);
        assert_eq!(read["status"], "ended");
        assert_eq!(read["entries"], run["entries"]);
        assert_eq!(read["prompt"], run["prompt"]);
        assert_eq!(read["problem"], serde_json::Value::Null);
    }

    #[test]
    fn the_prompt_preview_is_the_text_the_agent_receives() {
        let repo = TempRepo::new("run-preview");
        repo.write("src/a.rs", "fn a() {\n    one();\n}\n");
        let (site, _nudges) = run_site(&repo, "plain");
        post_note(&site, "src/a.rs", 1, 2, "  explain this  \n");
        post_note(&site, "src/a.rs", 3, 3, "and this");
        repo.write("src/a.rs", "fn a() {\n    one();\n// moved\n");

        let preview = get_json(&site, &format!("{RUN}/preview?notes=N2,N1"));
        assert_eq!(
            preview["prompt"],
            "These are review notes an engineer wrote while reading this repository.\n\
             Address every note. Change nothing the notes do not ask for.\n\
             When you finish, list each note id with one line on what you did.\n\
             \n\
             [N2] src/a.rs:3\n\
             (These lines are no longer in the file; they were:)\n\
             > }\n\
             and this\n\
             \n\
             [N1] src/a.rs:1-2\n\
             > fn a() {\n\
             >     one();\n\
             explain this\n"
        );
    }

    #[test]
    fn the_agent_is_kept_between_runs_and_replaced_when_another_is_chosen() {
        let repo = TempRepo::new("run-keep");
        repo.write("a.txt", "a\nb\n");
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");

        send(&site, &["N1"]);
        let first = wait_for(&site, &nudges, "ended");
        assert!(
            kinds(&first).contains(&"message:prompt 1".to_owned()),
            "{first}"
        );
        let (status, body) = send(&site, &["N2"]);
        assert_eq!(status, StatusCode::OK, "{body}");
        let second = wait_for(&site, &nudges, "ended");
        assert_eq!(second["id"], "R2");
        assert!(
            kinds(&second).contains(&"message:prompt 2".to_owned()),
            "the same agent process: {second}"
        );

        // Another command is another agent, started fresh.
        set_agents(&site, &[("Other", &fake_agent("plain2"))], "Other");
        send(&site, &["N1"]);
        let third = wait_for(&site, &nudges, "ended");
        assert_eq!(third["agent"], "Other");
        assert!(
            kinds(&third).contains(&"message:prompt 1".to_owned()),
            "{third}"
        );
    }

    #[test]
    fn any_run_can_be_read_by_its_id_in_this_window_and_from_disk_in_another() {
        let repo = TempRepo::new("run-by-id");
        repo.write("a.txt", "a\nb\n");
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        send(&site, &["N2"]);
        let second = wait_for(&site, &nudges, "ended");

        let first = get_json(&site, &format!("{RUN}?id=R1"));
        assert_eq!(first["id"], "R1");
        assert_eq!(first["notes"], serde_json::json!(["N1"]));
        assert!(
            kinds(&first).contains(&"message:prompt 1".to_owned()),
            "{first}"
        );
        assert_eq!(
            get_json(&site, &format!("{RUN}?id=R2")),
            second,
            "this window's run, whole"
        );

        // Another window has run nothing: it reads the logs.
        let (other, _) = run_site(&repo, "plain");
        assert_eq!(get_json(&other, &format!("{RUN}?id=R1")), first);

        let (status, body) = get(&site, &format!("{RUN}?id=R3"));
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(body.contains("no such run log"), "{body}");
        for id in ["R01", "R+1", "..%2FR1", ""] {
            let (status, body) = get(&site, &format!("{RUN}?id={id}"));
            assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {body}");
            assert!(body.contains("expected R and a number"), "{id}: {body}");
        }

        // A file named like a log but not by its own id is no run: listing it as R1 would name
        // another log.
        fs::write(repo.root.join(".git/postil/runs/R01.jsonl"), "").unwrap();
        assert_eq!(
            get_json(&site, RUNS)["runs"],
            serde_json::json!(["R2", "R1"])
        );
    }

    #[test]
    fn a_run_is_deleted_with_its_last_note() {
        let repo = TempRepo::new("run-delete");
        repo.write("a.txt", "a\nb\nc\n");
        let (site, nudges) = run_site(&repo, "plain");
        for (line, body) in [(1, "one"), (2, "two"), (3, "three")] {
            post_note(&site, "a.txt", line, line, body);
        }
        send(&site, &["N1", "N2"]);
        wait_for(&site, &nudges, "ended");
        send(&site, &["N3"]);
        wait_for(&site, &nudges, "ended");
        let log = |id: &str| repo.root.join(format!(".git/postil/runs/{id}.jsonl"));

        let (status, body) = delete(&site, &format!("{NOTES}?id=N1"));
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(log("R1").exists(), "R1 still has N2");
        let runs = || get_json(&site, RUNS)["runs"].clone();
        assert_eq!(runs(), serde_json::json!(["R2", "R1"]), "newest first");

        delete(&site, &format!("{NOTES}?id=N3"));
        assert!(!log("R2").exists(), "R2's only note is gone");
        assert_eq!(
            get_json(&site, RUN)["id"],
            "R1",
            "the latest run left is shown"
        );
        assert_eq!(get(&site, &format!("{RUN}?id=R2")).0, StatusCode::NOT_FOUND);
        assert_eq!(runs(), serde_json::json!(["R1"]));

        // R2's number is not given again: a page that read R2 would take the new run for it.
        send(&site, &["N2"]);
        assert_eq!(wait_for(&site, &nudges, "ended")["id"], "R3");
        assert_eq!(runs(), serde_json::json!(["R3", "R1"]));

        // A log that cannot be read is left alone: its notes cannot be checked.
        fs::write(log("R8"), "not a run log\n").unwrap();
        delete(&site, &format!("{NOTES}?id=N2"));
        assert!(!log("R1").exists());
        assert!(log("R8").exists(), "an unreadable log is kept");
        assert_eq!(
            get_json(&site, RUN),
            serde_json::Value::Null,
            "no run is left"
        );
        assert_eq!(runs(), serde_json::json!(["R8"]));
    }

    #[test]
    fn a_live_run_is_kept_when_its_note_is_deleted_and_goes_at_the_next_delete() {
        let repo = TempRepo::new("run-delete-live");
        repo.write("a.txt", "a\nb\n");
        let (site, nudges) = run_site(&repo, "hang");
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "running");
        let log = repo.root.join(".git/postil/runs/R1.jsonl");

        delete(&site, &format!("{NOTES}?id=N1"));
        assert!(log.exists(), "the live run keeps its log");
        assert_eq!(get_json(&site, RUN)["status"], "running");

        // Stopped twice, the second time forced, the run ends; the next delete takes it.
        let cancel = format!("{RUN}/cancel");
        post(&site, &cancel, "");
        site.runner().unwrap().backdate_stop();
        post(&site, &cancel, "");
        wait_for(&site, &nudges, "failed");
        delete(&site, &format!("{NOTES}?id=N2"));
        assert!(!log.exists());
    }

    #[test]
    fn a_question_waits_for_the_engineer_and_reaches_the_agent_answered_or_skipped() {
        let repo = TempRepo::new("run-ask");
        repo.write("a.txt", "a\nb\n");
        let (site, nudges) = run_site(&repo, "ask");
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");
        let answer =
            |body: serde_json::Value| post(&site, &format!("{RUN}/question"), &body.to_string());

        send(&site, &["N1"]);
        let waiting = wait_for(&site, &nudges, "waiting");
        assert!(
            kinds(&waiting).contains(&r#"message:form {"form":{}}"#.to_owned()),
            "Postil says it shows form questions: {waiting}"
        );
        let question = first_of_kind(&waiting, "question");
        assert_eq!(
            question["message"],
            "Should divide(i32::MIN, -1) also return None?"
        );
        assert_eq!(
            question["fields"]["properties"]["question_0"]["oneOf"][0]["const"], "Yes",
            "the form as the agent sent it"
        );
        let request = question["request"].clone();
        let (status, body) = answer(serde_json::json!({ "request": 99, "content": {} }));
        assert_eq!(status, StatusCode::NOT_FOUND, "no such question: {body}");
        let (status, _) = get(&site, &format!("{RUN}/question"));
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "an answer is posted"
        );
        let (status, body) = post(
            &site,
            &format!("{RUN}/permission"),
            &serde_json::json!({ "request": request, "option": "once" }).to_string(),
        );
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a question is no permission request: {body}"
        );

        let content = serde_json::json!({ "question_0": "Yes", "question_0_custom": "and log it" });
        let (status, body) = answer(serde_json::json!({ "request": request, "content": content }));
        assert_eq!(status, StatusCode::OK, "{body}");
        let ended = wait_for(&site, &nudges, "ended");
        let accepted = serde_json::json!({ "action": "accept", "content": content });
        assert!(
            kinds(&ended).contains(&format!("message:answered {accepted}")),
            "{ended}"
        );
        assert_eq!(first_of_kind(&ended, "reply")["content"], content);

        // The second asks by the name Claude's adapter uses; a skip is a decline.
        send(&site, &["N2"]);
        let waiting = wait_for(&site, &nudges, "waiting");
        let request = first_of_kind(&waiting, "question")["request"].clone();
        let (status, body) = answer(serde_json::json!({ "request": request, "content": null }));
        assert_eq!(status, StatusCode::OK, "{body}");
        let ended = wait_for(&site, &nudges, "ended");
        assert!(
            kinds(&ended).contains(&r#"message:answered {"action":"decline"}"#.to_owned()),
            "{ended}"
        );
        assert_eq!(
            first_of_kind(&ended, "reply")["content"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_withdrawn_question_closes_and_a_url_question_is_refused() {
        let repo = TempRepo::new("run-withdraw");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "ask-withdraw");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let ended = wait_for(&site, &nudges, "ended");
        let kinds = kinds(&ended);
        assert!(
            kinds.contains(&"message:url answered -32602".to_owned()),
            "a URL question cannot be shown as a form: {ended}"
        );
        assert!(
            kinds.contains(&"message:empty form answered -32602".to_owned()),
            "a form with no fields is refused: {ended}"
        );
        assert!(
            kinds.contains(&"message:withdrawn answered cancel".to_owned()),
            "a withdrawn request is still answered, as ACP asks: {ended}"
        );
        let question = kinds.iter().position(|kind| kind == "question").unwrap();
        assert_eq!(kinds[question + 1], "answer", "withdrawn: {ended}");
        assert_eq!(
            entries(&ended)[question + 1]["option"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_permission_request_with_no_allow_once_option_is_allowed_always() {
        let repo = TempRepo::new("run-always-only");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "always-only");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let ended = wait_for(&site, &nudges, "ended");
        assert_eq!(first_of_kind(&ended, "answer")["option"], "always");
        assert!(
            kinds(&ended).contains(&"message:chose always".to_owned()),
            "{ended}"
        );
    }

    #[test]
    fn a_question_left_open_at_the_turn_s_end_is_answered_cancel() {
        let repo = TempRepo::new("run-leave-open");
        repo.write("a.txt", "a\nb\n");
        let (site, nudges) = run_site(&repo, "leave-open");
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        // The same agent, kept: its second turn reports the reply to the first's question.
        send(&site, &["N2"]);
        let ended = wait_for(&site, &nudges, "ended");
        assert!(
            kinds(&ended).contains(&"message:left open answered cancel".to_owned()),
            "{ended}"
        );
    }

    #[test]
    fn a_permission_request_with_no_allow_option_waits_for_the_engineer() {
        let repo = TempRepo::new("run-reject-only");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "reject-only");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let waiting = wait_for(&site, &nudges, "waiting");
        let request = first_of_kind(&waiting, "permission")["request"].clone();
        let choose = |option: &str| {
            let body = serde_json::json!({ "request": request, "option": option });
            post(&site, &format!("{RUN}/permission"), &body.to_string())
        };
        let (status, body) = choose("sure");
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "not an offered option: {body}"
        );
        let (status, body) = choose("no");
        assert_eq!(status, StatusCode::OK, "{body}");
        let ended = wait_for(&site, &nudges, "ended");
        assert!(
            kinds(&ended).contains(&"message:chose no".to_owned()),
            "{ended}"
        );
    }

    #[test]
    fn reviewers_propose_read_only_then_the_last_judges_the_findings() {
        let repo = TempRepo::new("run-review");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        let review = |of: &str| {
            let body = serde_json::json!({ "of": of }).to_string();
            post(&site, &format!("{RUN}/review"), &body)
        };
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        let (status, body) = review("R1");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("no reviewers are set"), "{body}");

        let settings = serde_json::json!({
            "customAgents": [
                { "name": "Fake", "command": fake_agent("plain") },
                { "name": "Reviewer", "command": fake_agent("reviewer") },
            ],
            "enabledAgents": ["Fake", "Reviewer"],
            "agent": "Fake",
            "reviewers": ["Reviewer", "Reviewer"],
        });
        let (status, body) = save(&site, &settings.to_string());
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = review("R1");
        assert_eq!(status, StatusCode::OK, "{body}");

        // The judge, R3, starts on its own once the proposer, R2, ends.
        let judge = loop {
            let run = get_json(&site, RUN);
            if run["id"] == "R3" && run["status"] == "ended" {
                break run;
            }
            nudges
                .recv_timeout(WAIT)
                .unwrap_or_else(|_| panic!("no change while waiting for the judge: {run}"));
        };
        let proposer = get_json(&site, &format!("{RUN}?id=R2"));
        assert_eq!(
            proposer["review"],
            serde_json::json!({ "of": "R1", "reviewer": 1, "reviewers": 2 })
        );
        assert_eq!(
            proposer["notes"],
            serde_json::json!(["N1"]),
            "the reviewed run's notes"
        );
        assert!(
            kinds(&proposer).contains(&"message:edit no".to_owned()),
            "a proposer cannot edit: {proposer}"
        );
        assert!(
            kinds(&proposer).contains(&"message:read once".to_owned()),
            "a proposer can read: {proposer}"
        );
        let findings = first_of_kind(&proposer, "findings");
        assert_eq!(findings["findings"][0]["id"], "F1");
        assert_eq!(findings["findings"][0]["note"], "N1");
        assert_eq!(findings["problem"], serde_json::Value::Null);

        assert_eq!(judge["review"]["reviewer"], 2);
        assert!(
            snapshot_points(&proposer).is_empty(),
            "a proposer may not edit: {proposer}"
        );
        assert_eq!(
            snapshot_points(&judge),
            ["before", "after"],
            "the judge edits: {judge}"
        );
        assert!(
            kinds(&judge).contains(&"message:judged: saw F1".to_owned()),
            "the judge is given the findings: {judge}"
        );
        assert!(kinds(&judge).contains(&"message:F1: accepted - said more".to_owned()));
        let got = kinds(&proposer)
            .into_iter()
            .find(|kind| kind.starts_with("message:got "))
            .unwrap();
        assert!(
            got.contains("a.txt\n\nprompt 1"),
            "the reviewed reply, its messages a blank line apart: {got}"
        );
        assert!(!site.runner().unwrap().reviewing(), "the judge ends it");
        assert_eq!(
            states(&site),
            [serde_json::json!({ "kind": "sent", "run": "R1" })],
            "a review marks no note sent"
        );

        let (status, body) = review("R2");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("is itself a review"), "{body}");
        assert_eq!(
            get(&site, &format!("{RUN}/review")).0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    /// Saves Fake (`author`) as the agent notes go to and `reviewers`, each a (name,
    /// scenario) pair, as the reviewers in order.
    fn set_reviewers(site: &Site, author: &str, reviewers: &[(&str, &str)]) {
        let mut custom = vec![serde_json::json!({ "name": "Fake", "command": fake_agent(author) })];
        custom.extend(reviewers.iter().map(
            |(name, scenario)| serde_json::json!({ "name": name, "command": fake_agent(scenario) }),
        ));
        let mut enabled = vec!["Fake"];
        enabled.extend(reviewers.iter().map(|(name, _)| *name));
        let settings = serde_json::json!({
            "customAgents": custom,
            "enabledAgents": enabled,
            "agent": "Fake",
            "reviewers": reviewers.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        });
        let (status, body) = save(site, &settings.to_string());
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// Waits until run `id` is the latest and has ended, and returns it.
    fn wait_for_run(site: &Site, nudges: &mpsc::Receiver<()>, id: &str) -> serde_json::Value {
        loop {
            let run = get_json(site, RUN);
            if run["id"] == id
                && ["ended", "failed"].contains(&run["status"].as_str().unwrap_or(""))
            {
                return run;
            }
            nudges
                .recv_timeout(WAIT)
                .unwrap_or_else(|_| panic!("no change while waiting for {id}: {run}"));
        }
    }

    #[test]
    fn a_reviewer_sees_the_reviewed_run_s_edits() {
        let repo = TempRepo::new("run-review-edits");
        repo.write("src/a.rs", "fn a() {\n    one();\n}\n");
        let (site, nudges) = run_site(&repo, "turn");
        post_note(&site, "src/a.rs", 2, 2, "rename one");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        set_reviewers(&site, "turn", &[("Reviewer", "reviewer")]);
        let body = serde_json::json!({ "of": "R1" }).to_string();
        assert_eq!(
            post(&site, &format!("{RUN}/review"), &body).0,
            StatusCode::OK
        );
        let proposer = wait_for_run(&site, &nudges, "R2");
        assert!(
            kinds(&proposer).contains(&"message:saw the edit: true".to_owned()),
            "the run's diff, before and after, is in the prompt: {proposer}"
        );
        let linked = kinds(&proposer)
            .into_iter()
            .find(|kind| kind.starts_with("message:got "))
            .unwrap();
        assert!(
            linked.ends_with(&format!(" | {}", file_uri_of(&repo, "src/a.rs"))),
            "the edited file is linked once: {linked}"
        );
    }

    #[test]
    fn a_review_stops_at_a_reviewer_that_fails_finds_nothing_readable_or_edits() {
        for (scenario, problem) in [
            // Nothing readable is no finding, so the judge has nothing to judge.
            ("reviewer-silent", Some("no ```json block")),
            ("refuse-prompt", None),
            ("reviewer-edits", Some("reviewer 1 changed files")),
        ] {
            let repo = TempRepo::new(&format!("run-review-stop-{scenario}"));
            repo.write("a.txt", "a\n");
            let (site, nudges) = run_site(&repo, "plain");
            post_note(&site, "a.txt", 1, 1, "one");
            send(&site, &["N1"]);
            wait_for(&site, &nudges, "ended");
            set_reviewers(
                &site,
                "plain",
                &[("First", scenario), ("Judge", "reviewer")],
            );
            let body = serde_json::json!({ "of": "R1" }).to_string();
            assert_eq!(
                post(&site, &format!("{RUN}/review"), &body).0,
                StatusCode::OK
            );
            let first = wait_for_run(&site, &nudges, "R2");
            if let Some(problem) = problem {
                let found = first_of_kind(&first, "findings");
                assert!(
                    found["problem"].as_str().unwrap_or("").contains(problem),
                    "{scenario}: {found}"
                );
            }
            assert!(
                !site.runner().unwrap().reviewing(),
                "{scenario}: no judge starts"
            );
            assert_eq!(
                get_json(&site, RUNS)["runs"],
                serde_json::json!(["R2", "R1"]),
                "{scenario}"
            );
        }
    }

    #[test]
    fn a_single_reviewer_only_proposes_and_a_send_after_a_review_starts_a_fresh_agent() {
        let repo = TempRepo::new("run-review-single");
        repo.write("a.txt", "a\nb\n");
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        // The reviewer is the very agent notes go to: Fake.
        let settings = serde_json::json!({
            "customAgents": [{ "name": "Fake", "command": fake_agent("plain") }],
            "enabledAgents": ["Fake"],
            "agent": "Fake",
            "reviewers": ["Fake"],
        });
        assert_eq!(save(&site, &settings.to_string()).0, StatusCode::OK);
        let body = serde_json::json!({ "of": "R1" }).to_string();
        assert_eq!(
            post(&site, &format!("{RUN}/review"), &body).0,
            StatusCode::OK
        );
        let proposer = wait_for_run(&site, &nudges, "R2");
        assert_eq!(proposer["review"]["reviewers"], 1);
        assert_eq!(
            get_json(&site, RUNS)["runs"],
            serde_json::json!(["R2", "R1"]),
            "no judge"
        );
        assert!(!site.runner().unwrap().reviewing());

        // The kept agent is the reviewer's, told not to change files: a send starts afresh.
        send(&site, &["N2"]);
        let next = wait_for_run(&site, &nudges, "R3");
        assert!(
            kinds(&next).contains(&"message:prompt 1".to_owned()),
            "a new agent's first prompt: {next}"
        );
    }

    #[test]
    fn a_follow_up_goes_to_the_same_conversation_or_tells_a_fresh_agent_the_thread() {
        let repo = TempRepo::new("run-follow");
        repo.write("a.txt", "a\nb\n");
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        post_note(&site, "a.txt", 2, 2, "two");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        let follow = |of: &str, text: &str| {
            let body = serde_json::json!({ "of": of, "text": text }).to_string();
            post(&site, &format!("{RUN}/follow"), &body)
        };

        let (status, body) = follow("R1", "  and more  ");
        assert_eq!(status, StatusCode::OK, "{body}");
        let second = wait_for_run(&site, &nudges, "R2");
        assert_eq!(
            second["followUp"],
            serde_json::json!({ "of": "R1", "text": "and more" })
        );
        assert_eq!(
            second["notes"],
            serde_json::json!(["N1"]),
            "its thread's notes"
        );
        assert_eq!(
            kinds(&second)[1..3],
            ["message:got and more", "message:prompt 2"],
            "the kept agent's second prompt, the engineer's words alone: {second}"
        );
        assert_eq!(
            states(&site),
            [
                serde_json::json!({ "kind": "sent", "run": "R1" }),
                serde_json::json!({ "kind": "open" })
            ],
            "a follow-up marks no note sent"
        );

        // Another send, R3, goes to the kept agent. Its conversation goes on past R2, so a
        // follow-up of R2 starts a fresh agent and tells it the thread.
        send(&site, &["N2"]);
        let other = wait_for_run(&site, &nudges, "R3");
        assert!(
            kinds(&other).contains(&"message:prompt 3".to_owned()),
            "{other}"
        );
        let (status, body) = follow("R2", "again");
        assert_eq!(status, StatusCode::OK, "{body}");
        let fresh = wait_for_run(&site, &nudges, "R4");
        let got = kinds(&fresh)
            .into_iter()
            .find(|kind| kind.starts_with("message:got "))
            .unwrap();
        for part in [
            "its conversation is gone",
            "R1 was asked:\n\nThese are review notes",
            "R2 was asked:\n\nand more\n\nIt answered:\n\ngot and more\n\nprompt 2",
            "The engineer now asks:\n\nagain",
        ] {
            assert!(got.contains(part), "{part:?} in {got}");
        }
        assert!(
            kinds(&fresh).contains(&"message:prompt 1".to_owned()),
            "{fresh}"
        );
        assert_eq!(
            fresh["followUp"]["text"], "again",
            "the engineer's words, not the prompt"
        );
    }

    #[test]
    fn a_follow_up_is_refused_for_a_review_a_followed_run_or_no_text() {
        let repo = TempRepo::new("run-follow-refused");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        let body = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
        assert_eq!(
            post(&site, &format!("{RUN}/follow"), &body).0,
            StatusCode::OK
        );
        wait_for_run(&site, &nudges, "R2");

        // A review reviews the run a thread started from, not a follow-up.
        set_reviewers(&site, "plain", &[("Reviewer", "reviewer-silent")]);
        let review = |of: &str| {
            let body = serde_json::json!({ "of": of }).to_string();
            post(&site, &format!("{RUN}/review"), &body)
        };
        let (status, body) = review("R2");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("R2 is a follow-up of R1"), "{body}");
        assert_eq!(review("R1").0, StatusCode::OK);
        wait_for_run(&site, &nudges, "R3");

        for (body, status, why) in [
            (
                r#"{"of": "R2", "text": "  "}"#,
                StatusCode::BAD_REQUEST,
                "has no text",
            ),
            (
                r#"{"of": "R3", "text": "x"}"#,
                StatusCode::BAD_REQUEST,
                "R3 is a review",
            ),
            (
                r#"{"of": "R1", "text": "x"}"#,
                StatusCode::BAD_REQUEST,
                "R2 already follows R1: follow up R2",
            ),
            (r#"{"of": "R9", "text": "x"}"#, StatusCode::NOT_FOUND, "R9"),
            (
                r#"{"of": "R2"}"#,
                StatusCode::BAD_REQUEST,
                "unreadable request",
            ),
        ] {
            let (got, reply) = post(&site, &format!("{RUN}/follow"), body);
            assert_eq!(got, status, "{body}: {reply}");
            assert!(reply.contains(why), "{body}: {reply}");
        }
        assert_eq!(
            get(&site, &format!("{RUN}/follow")).0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            get_json(&site, RUNS)["runs"],
            serde_json::json!(["R3", "R2", "R1"]),
            "nothing refused made a run"
        );
    }

    #[test]
    fn a_follow_up_of_a_log_that_follows_itself_or_a_newer_run_is_refused() {
        let repo = TempRepo::new("run-follow-loop");
        let (site, _nudges) = run_site(&repo, "plain");
        let runs = repo.root.join(".git/postil/runs");
        fs::create_dir_all(&runs).unwrap();
        let follows = |file: &str, id: &str, of: &str| {
            let log = header(id).replace(
                r#""version":1"#,
                &format!(r#""version":3,"followUp":{{"of":"{of}","text":"x"}}"#),
            );
            fs::write(runs.join(format!("{file}.jsonl")), format!("{log}\n")).unwrap();
        };
        // R2 follows itself; R3 and R4 follow each other; R6.jsonl names itself R7, so a
        // check of the header's id would let it follow R6 again and again; R8 follows an id
        // that is not one.
        follows("R2", "R2", "R2");
        follows("R3", "R3", "R4");
        follows("R4", "R4", "R3");
        follows("R6", "R7", "R6");
        follows("R8", "R8", "R08");
        for (of, problem) in [
            ("R2", "R2: it follows R2"),
            ("R4", "R3: it follows R4"),
            ("R6", "R6: it follows R6"),
            ("R8", "R8: it follows R08"),
        ] {
            let body = serde_json::json!({ "of": of, "text": "more" }).to_string();
            let (status, reply) = post(&site, &format!("{RUN}/follow"), &body);
            assert_eq!(status, StatusCode::NOT_FOUND, "{of}: {reply}");
            assert!(reply.contains(problem), "{of}: {reply}");
            assert!(reply.contains("which is not an older run"), "{reply}");
        }
    }

    #[test]
    fn a_run_s_changes_are_the_diff_between_its_snapshots_and_the_index_is_left_alone() {
        let repo = TempRepo::new("run-changes");
        repo.write("a.txt", "a\n");
        repo.write("secret.txt", "one\n");
        repo.git(&["add", "a.txt", "secret.txt"]);
        let (site, nudges) = run_site(&repo, "writes");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let first = wait_for(&site, &nudges, "ended");
        let snapshots: Vec<&serde_json::Value> = first["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["kind"] == "snapshot")
            .collect();
        assert_eq!(snapshots.len(), 2, "one before, one after: {first}");
        assert_eq!(
            first["entries"][0]["kind"], "snapshot",
            "before the agent starts"
        );
        let changes = |query: &str| get(&site, &format!("{RUN}/changes?{query}"));

        let (status, body) = changes("from=R1");
        assert_eq!(status, StatusCode::OK, "{body}");
        let one: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(one["before"], snapshots[0]["tree"]);
        assert_eq!(one["after"], snapshots[1]["tree"]);
        let patch = one["patch"].as_str().unwrap();
        for part in [
            "+++ b/a.txt",
            "+prompt 1",
            "new file mode",
            "+++ b/new.txt",
            "+new",
        ] {
            assert!(patch.contains(part), "{part:?} in {patch}");
        }

        // The engineer takes a file out of git and ignores it: snapshots stop following it. And
        // force-adds an ignored one: snapshots follow it from then on.
        repo.git(&["rm", "--cached", "--quiet", "secret.txt"]);
        repo.write(".gitignore", "secret.txt\nforced.txt\n");
        repo.write("secret.txt", "two\n");
        repo.write("forced.txt", "kept\n");
        repo.git(&["add", "--force", "forced.txt"]);

        // A follow-up writes again: R1 to R2 is the whole thread, R2 alone its own change.
        let body = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
        assert_eq!(
            post(&site, &format!("{RUN}/follow"), &body).0,
            StatusCode::OK
        );
        wait_for_run(&site, &nudges, "R2");
        let patch_of = |query: &str| {
            let (status, body) = changes(query);
            assert_eq!(status, StatusCode::OK, "{query}: {body}");
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["patch"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let thread = patch_of("from=R1&to=R2");
        assert!(
            thread.contains("+prompt 2") && !thread.contains("prompt 1"),
            "{thread}"
        );
        let second = patch_of("from=R2");
        assert!(second.contains("-prompt 1\n+prompt 2"), "{second}");
        assert!(!second.contains("new.txt"), "unchanged by R2: {second}");
        assert!(
            !second.contains("+two"),
            "an ignored file left untracked: {second}"
        );
        assert!(
            thread.contains("+++ b/forced.txt"),
            "a force-added file: {thread}"
        );

        // Snapshots go through an index of their own: the engineer's staged files are the same.
        let staged = Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&staged.stdout),
            "a.txt\nforced.txt\n"
        );
    }

    #[test]
    fn changes_compare_with_the_files_now_without_a_snapshot_after_and_refuse_bad_logs() {
        let repo = TempRepo::new("run-changes-logs");
        repo.write("a.txt", "a\n");
        let (site, _nudges) = run_site(&repo, "writes");
        let changes = |query: &str| get(&site, &format!("{RUN}/changes?{query}"));
        let runs = repo.root.join(".git/postil/runs");
        fs::create_dir_all(&runs).unwrap();
        let tree = serde_json::json!(site.repo().unwrap().snapshot().unwrap());

        // A tracked, ignored file deleted but not staged stays in the real index only: the
        // snapshot index is not copied again for it.
        repo.write(".gitignore", "gen.lock\n");
        repo.write("gen.lock", "x\n");
        repo.git(&["add", "--force", "gen.lock"]);
        let open = site.repo().unwrap();
        let copies = open.index_copies();
        open.snapshot().unwrap();
        assert_eq!(
            open.index_copies(),
            copies + 1,
            "a force-added ignored file copies the index"
        );
        fs::remove_file(repo.root.join("gen.lock")).unwrap();
        open.snapshot().unwrap();
        open.snapshot().unwrap();
        assert_eq!(
            open.index_copies(),
            copies + 1,
            "copied again for a deleted file"
        );

        // A run with only the snapshot before it is compared with the files now.
        let log = header("R5").replace(r#""version":1"#, r#""version":4"#);
        fs::write(
            runs.join("R5.jsonl"),
            format!("{log}\n{{\"kind\":\"snapshot\",\"point\":\"before\",\"tree\":{tree}}}\n"),
        )
        .unwrap();
        repo.write("a.txt", "now\n");
        let (status, body) = changes("from=R5");
        assert_eq!(status, StatusCode::OK, "{body}");
        let now: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(now["after"], serde_json::Value::Null);
        assert!(now["patch"].as_str().unwrap().contains("+now"), "{now}");

        let old = header("R4").replace(r#""version":1"#, r#""version":3"#);
        fs::write(runs.join("R4.jsonl"), format!("{old}\n")).unwrap();
        let blank = header("R7").replace(r#""version":1"#, r#""version":4"#);
        fs::write(
            runs.join("R7.jsonl"),
            format!("{blank}\n{{\"kind\":\"snapshot\",\"point\":\"before\",\"tree\":\"\"}}\n"),
        )
        .unwrap();
        // A tree id is passed to git, so one edited into an option is refused, and so is none.
        let edited = header("R6").replace(r#""version":1"#, r#""version":4"#);
        fs::write(
            runs.join("R6.jsonl"),
            format!(
                "{edited}\n{{\"kind\":\"snapshot\",\"point\":\"before\",\"tree\":\"--output=x\"}}\n"
            ),
        )
        .unwrap();
        for (query, status, why) in [
            ("from=R4", StatusCode::BAD_REQUEST, "R4 has no snapshot"),
            (
                "from=R6",
                StatusCode::BAD_REQUEST,
                "expected a git object id",
            ),
            (
                "from=R7",
                StatusCode::BAD_REQUEST,
                "expected a git object id",
            ),
            (
                "from=R5&to=R4",
                StatusCode::BAD_REQUEST,
                "R4 is older than R5",
            ),
            ("to=R1", StatusCode::BAD_REQUEST, "expected ?from="),
            ("from=R9", StatusCode::NOT_FOUND, "R9"),
        ] {
            let (got, body) = changes(query);
            assert_eq!(got, status, "{query}: {body}");
            assert!(body.contains(why), "{query}: {body}");
        }
        let (status, _) = post(&site, &format!("{RUN}/changes?from=R1"), "");
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn a_snapshot_that_cannot_be_taken_is_the_run_s_problem_and_the_run_goes_on() {
        let repo = TempRepo::new("run-no-snapshot");
        repo.write("a.txt", "a\n");
        repo.git(&["add", "a.txt"]);
        let index = repo.root.join(".git/index");
        // The first snapshot starts from a copy of the real index, which cannot be read.
        fs::set_permissions(&index, fs::Permissions::from_mode(0o000)).unwrap();
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let ended = wait_for(&site, &nudges, "ended");
        fs::set_permissions(&index, fs::Permissions::from_mode(0o644)).unwrap();
        let problem = ended["problem"].as_str().unwrap_or_default();
        assert!(
            problem.contains("no snapshot of the files was taken"),
            "{ended}"
        );
        assert!(problem.contains(".git/index"), "{problem}");
        assert!(
            kinds(&ended).contains(&"message:prompt 1".to_owned()),
            "{ended}"
        );
        let (status, body) = get(&site, &format!("{RUN}/changes?from=R1"));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("R1 has no snapshot from before it"), "{body}");

        // Postil's own index torn: it is copied again from the real one at once.
        let ours = repo.root.join(".git/postil-snapshot.index");
        fs::write(&ours, "torn").unwrap();
        send(&site, &["N1"]);
        let healed = wait_for_run(&site, &nudges, "R2");
        assert_eq!(snapshot_points(&healed), ["before", "after"], "{healed}");

        // The real index torn: its copy cannot be used, so the snapshot fails and the copy is
        // removed rather than failing every snapshot after it.
        fs::remove_file(&ours).unwrap();
        fs::write(&index, "torn").unwrap();
        send(&site, &["N1"]);
        let torn = wait_for_run(&site, &nudges, "R3");
        let problem = torn["problem"].as_str().unwrap_or_default();
        assert!(problem.contains("no snapshot"), "{torn}");
        assert!(!ours.exists(), "the unusable copy is gone");
    }

    #[test]
    fn a_run_can_be_cancelled_and_one_runs_at_a_time() {
        let repo = TempRepo::new("run-cancel");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "cancel");
        post_note(&site, "a.txt", 1, 1, "one");
        let cancel = format!("{RUN}/cancel");
        let (status, body) = post(&site, &cancel, "");
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "nothing to cancel yet: {body}"
        );

        send(&site, &["N1"]);
        wait_for(&site, &nudges, "waiting");
        let (status, body) = send(&site, &["N1"]);
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("still working"), "{body}");
        let follow = serde_json::json!({ "of": "R1", "text": "more" }).to_string();
        let (status, body) = post(&site, &format!("{RUN}/follow"), &follow);
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a follow-up waits too: {body}"
        );

        let (status, body) = post(&site, &cancel, "");
        assert_eq!(status, StatusCode::OK, "{body}");
        let ended = wait_for(&site, &nudges, "ended");
        assert_eq!(
            kinds(&ended)[3..],
            [
                "question",
                "cancelRequested",
                "answer",
                "message:question cancel",
                "message:late permission cancelled",
                "message:late question cancel",
                "ended"
            ],
            "the open question, and the ones that crossed the cancel, are answered cancel"
        );
        assert_eq!(entries(&ended)[5]["option"], serde_json::Value::Null);
        assert_eq!(entries(&ended)[9]["reason"], "cancelled");

        let late = serde_json::json!({ "request": 0, "content": {} });
        let (status, body) = post(&site, &format!("{RUN}/question"), &late.to_string());
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a cancelled question: {body}"
        );
    }

    #[test]
    fn an_agent_that_fails_to_start_fails_the_run_and_leaves_the_notes_open() {
        let repo = TempRepo::new("run-fail");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "exit-on-start");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let failed = wait_for(&site, &nudges, "failed");
        let error = entries(&failed)[0]["error"].as_str().unwrap().to_owned();
        assert!(
            error.contains("Please sign in: run `fake login`"),
            "the agent's own words: {error}"
        );
        assert_eq!(states(&site), [serde_json::json!({ "kind": "open" })]);

        set_agents(&site, &[("Fake", &fake_agent("auth"))], "Fake");
        send(&site, &["N1"]);
        let failed = wait_for(&site, &nudges, "failed");
        assert_eq!(failed["id"], "R2");
        let error = entries(&failed)[0]["error"].as_str().unwrap().to_owned();
        assert!(error.contains("needs you to sign in"), "{error}");

        set_agents(&site, &[("Ghost", "no-such-agent-program --acp")], "Ghost");
        send(&site, &["N1"]);
        let failed = wait_for(&site, &nudges, "failed");
        let error = entries(&failed)[0]["error"].as_str().unwrap().to_owned();
        assert!(
            error.contains("cannot find the agent's program no-such-agent-program"),
            "{error}"
        );
    }

    #[test]
    fn an_agent_that_exits_mid_run_fails_it_and_is_started_again_next_time() {
        let repo = TempRepo::new("run-exit");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "exit-mid-turn");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let failed = wait_for(&site, &nudges, "failed");
        let last = failed["entries"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last["error"], "the agent exited: panicked at the model");

        send(&site, &["N1"]);
        let again = wait_for(&site, &nudges, "failed");
        assert_eq!(again["id"], "R2");
        assert!(
            kinds(&again).contains(&"message:prompt 1".to_owned()),
            "a new process: {again}"
        );
    }

    #[test]
    fn closing_the_project_stops_its_agent() {
        let repo = TempRepo::new("run-close");
        let other = TempRepo::new("run-close-other");
        repo.write("a.txt", "a\n");
        let pid_file = repo.root.join(".git/agent-child.pid");
        let command = format!("{} {}", fake_agent("spawns-child"), pid_file.display());
        let (site, nudges) = run_site(&repo, "plain");
        set_agents(&site, &[("Fake", &command)], "Fake");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        let pid = fs::read_to_string(&pid_file).unwrap();
        let alive = || {
            Command::new("/bin/kill")
                .args(["-0", &pid])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(alive(), "the agent is kept after its run");

        site.open(Repo::open(&other.root).unwrap());
        assert!(!alive(), "opening another project stopped it");
        assert_eq!(get(&site, RUN).1, "null", "the new project has no run");
    }

    fn header(id: &str) -> String {
        let fields = r#""agent":"Fake","notes":["N1"],"prompt":"p","startedAt":1"#;
        format!(r#"{{"version":1,"id":"{id}",{fields}}}"#)
    }

    #[test]
    fn a_log_without_an_end_reads_as_unfinished_and_torn_lines_spoil_only_themselves() {
        let repo = TempRepo::new("run-torn");
        let runs = repo.root.join(".git/postil/runs");
        fs::create_dir_all(&runs).unwrap();
        // A message whose text a crash cut inside a multi-byte character, then a torn line.
        let mut log = format!("{}\n{{\"kind\":\"started\"}}\n", header("R3")).into_bytes();
        log.extend_from_slice(b"{\"kind\":\"message\",\"text\":\"caf\xC3\"}\n{\"kind\":\"mess");
        fs::write(runs.join("R3.jsonl"), log).unwrap();
        fs::write(runs.join("notes.txt"), "not a run\n").unwrap();

        let site = repo.site();
        let run = get_json(&site, RUN);
        assert_eq!(run["id"], "R3", "the newest log");
        assert_eq!(run["status"], "unfinished");
        assert_eq!(
            run["entries"],
            serde_json::json!([
                { "kind": "started" },
                { "kind": "message", "text": "caf\u{FFFD}" },
            ])
        );
        let problem = run["problem"].as_str().unwrap();
        assert!(problem.contains("line 4 of"), "{problem}");

        // ext4 and XFS can extend a file with zeros after a crash.
        let mut log = format!("{}\n{{\"kind\":\"started\"}}\n", header("R3")).into_bytes();
        log.extend_from_slice(&[0; 64]);
        fs::write(runs.join("R3.jsonl"), log).unwrap();
        let run = get_json(&site, RUN);
        assert_eq!(run["entries"], serde_json::json!([{ "kind": "started" }]));
        assert!(
            run["problem"].as_str().unwrap().contains("line 3 of"),
            "{run}"
        );
    }

    #[test]
    fn an_unreadable_newest_log_is_skipped_and_named() {
        let repo = TempRepo::new("run-skip");
        let runs = repo.root.join(".git/postil/runs");
        fs::create_dir_all(&runs).unwrap();
        fs::write(runs.join("R2.jsonl"), format!("{}\n", header("R2"))).unwrap();
        let site = repo.site();

        // Empty: another window is about to write its header, so it is skipped quietly.
        fs::write(runs.join("R3.jsonl"), "").unwrap();
        let run = get_json(&site, RUN);
        assert_eq!(
            (run["id"].as_str(), run["problem"].as_str()),
            (Some("R2"), None)
        );

        for (newest, why) in [
            ("\n", "its first line is not a run header"),
            ("not a header\n", "its first line is not a run header"),
            (
                r#"{"version":5,"id":"R3","newField":1}"#,
                "it is version 5, and this build of Postil reads versions 1 to 4",
            ),
            (
                &*header("R3").replace(
                    r#""startedAt":1"#,
                    concat!(
                        r#""startedAt":1,"review":{"of":"R1","reviewer":1,"reviewers":1},"#,
                        r#""followUp":{"of":"R2","text":"x"}"#,
                    ),
                ),
                "its header is both a review and a follow-up",
            ),
        ] {
            fs::write(runs.join("R3.jsonl"), newest).unwrap();
            let run = get_json(&site, RUN);
            assert_eq!(run["id"], "R2", "{newest:?}");
            let problem = run["problem"].as_str().unwrap();
            assert!(problem.contains("R3.jsonl is not shown"), "{problem}");
            assert!(problem.contains(why), "{problem}");
        }

        fs::remove_file(runs.join("R2.jsonl")).unwrap();
        assert_eq!(get(&site, RUN).1, "null", "nothing readable is no run");

        // A runs directory that cannot be listed is an error, not "no runs".
        fs::remove_dir_all(&runs).unwrap();
        fs::write(&runs, "not a directory").unwrap();
        let (status, body) = get(&site, RUN);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(body.contains("cannot read the run log"), "{body}");
    }

    #[test]
    fn a_run_log_that_cannot_be_created_refuses_the_send() {
        let repo = TempRepo::new("run-readonly");
        repo.write("a.txt", "a\n");
        let (site, _nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        let postil = repo.root.join(".git/postil");
        fs::set_permissions(&postil, fs::Permissions::from_mode(0o555)).unwrap();
        let (status, body) = send(&site, &["N1"]);
        fs::set_permissions(&postil, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(body.contains("cannot write the run log"), "{body}");
        assert_eq!(get(&site, RUN).1, "null");
    }

    /// Whether the process whose pid is in `file` runs.
    fn alive(file: &Path) -> bool {
        let pid = fs::read_to_string(file).unwrap();
        Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// Waits until the fake agent wrote its pid to `file`.
    fn wait_for_pid(file: &Path) {
        let deadline = Instant::now() + WAIT;
        while fs::read_to_string(file).map_or(true, |pid| pid.is_empty()) {
            assert!(Instant::now() < deadline, "the agent never started");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until the process whose pid is in `file` is gone.
    fn wait_for_exit(file: &Path) {
        let deadline = Instant::now() + WAIT;
        while alive(file) {
            assert!(Instant::now() < deadline, "the agent was not stopped");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_cancel_while_the_agent_starts_stops_it_and_ends_the_run_at_once() {
        let repo = TempRepo::new("run-cancel-start");
        repo.write("a.txt", "a\n");
        let pid_file = repo.root.join(".git/silent.pid");
        let (site, nudges) = run_site(&repo, "plain");
        let silent = format!("{} {}", fake_agent("silent"), pid_file.display());
        set_agents(
            &site,
            &[("Silent", &silent), ("Fake", &fake_agent("plain"))],
            "Silent",
        );
        post_note(&site, "a.txt", 1, 1, "one");

        send(&site, &["N1"]);
        wait_for_pid(&pid_file);
        assert_eq!(get_json(&site, RUN)["status"], "starting");
        let (status, body) = post(&site, &format!("{RUN}/cancel"), "");
        assert_eq!(status, StatusCode::OK, "{body}");
        let ended = get_json(&site, RUN);
        assert_eq!(
            ended["status"], "ended",
            "at once, not after the start times out"
        );
        assert_eq!(entries(&ended)[1]["reason"], "cancelled");
        assert_eq!(states(&site), [serde_json::json!({ "kind": "open" })]);
        wait_for_exit(&pid_file);

        // Another send goes ahead; the stopped start reports nothing over it.
        let other = serde_json::json!({ "notes": ["N1"], "agent": "Fake" });
        let (status, body) = post(&site, RUN, &other.to_string());
        assert_eq!(status, StatusCode::OK, "{body}");
        let next = wait_for(&site, &nudges, "ended");
        assert_eq!(
            (next["id"].as_str(), next["agent"].as_str()),
            (Some("R2"), Some("Fake"))
        );
    }

    #[test]
    fn closing_the_project_while_its_agent_starts_stops_the_agent() {
        let repo = TempRepo::new("run-close-start");
        let other = TempRepo::new("run-close-start-other");
        repo.write("a.txt", "a\n");
        let pid_file = repo.root.join(".git/silent.pid");
        let (site, _nudges) = run_site(&repo, "plain");
        let silent = format!("{} {}", fake_agent("silent"), pid_file.display());
        set_agents(&site, &[("Silent", &silent)], "Silent");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        wait_for_pid(&pid_file);

        site.open(Repo::open(&other.root).unwrap());
        wait_for_exit(&pid_file);
    }

    #[test]
    fn quitting_while_an_agent_starts_stops_the_agent() {
        let repo = TempRepo::new("run-quit-start");
        repo.write("a.txt", "a\n");
        let pid_file = repo.root.join(".git/silent.pid");
        let (site, _nudges) = run_site(&repo, "plain");
        let silent = format!("{} {}", fake_agent("silent"), pid_file.display());
        set_agents(&site, &[("Silent", &silent)], "Silent");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        wait_for_pid(&pid_file);

        // The window closing drops the site.
        drop(site);
        wait_for_exit(&pid_file);
    }

    #[test]
    fn a_refused_prompt_leaves_the_notes_open_and_the_next_send_starts_a_fresh_agent() {
        let repo = TempRepo::new("run-refused");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "refuse-first");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let failed = wait_for(&site, &nudges, "failed");
        let last = failed["entries"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(
            last["error"],
            "the agent refused session/prompt: model overloaded (-32603)"
        );

        // The failed turn let the agent go (its session may be broken): a fresh one refuses
        // its first prompt again, where the kept one would have answered.
        send(&site, &["N1"]);
        let again = wait_for(&site, &nudges, "failed");
        assert_eq!(again["id"], "R2");
        // Checked last: the first run's notes would be marked on its reader thread after the
        // run showed failed, and by now that thread is long done.
        assert_eq!(
            states(&site),
            [serde_json::json!({ "kind": "open" })],
            "the agent never took the turn"
        );
    }

    #[test]
    fn notes_that_cannot_be_marked_sent_are_named_on_the_run() {
        let repo = TempRepo::new("run-unmarked");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");

        // The runs directory stays writable; the review file's directory does not.
        let postil = repo.root.join(".git/postil");
        fs::set_permissions(&postil, fs::Permissions::from_mode(0o555)).unwrap();
        send(&site, &["N1"]);
        let run = wait_for(&site, &nudges, "ended");
        fs::set_permissions(&postil, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(run["id"], "R2");
        let problem = run["problem"].as_str().unwrap();
        assert!(problem.contains("could not be marked sent"), "{problem}");
        assert_eq!(
            states(&site),
            [serde_json::json!({ "kind": "sent", "run": "R1" })]
        );
    }

    #[test]
    fn a_refusal_before_any_work_leaves_the_notes_open() {
        let repo = TempRepo::new("run-refusal");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "refusal");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        let ended = wait_for(&site, &nudges, "ended");
        let last = ended["entries"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["reason"], "refusal");
        assert_eq!(states(&site), [serde_json::json!({ "kind": "open" })]);
    }

    #[test]
    fn a_second_stop_ends_an_agent_that_does_not_stop() {
        let repo = TempRepo::new("run-hang");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "hang");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "running");
        let cancel = format!("{RUN}/cancel");
        assert_eq!(post(&site, &cancel, "").0, StatusCode::OK);
        let stopping = get_json(&site, RUN);
        assert_eq!(stopping["status"], "cancelling");
        assert!(stopping["forceIn"].as_u64().unwrap() > 0, "{stopping}");
        assert_eq!(post(&site, &cancel, "").0, StatusCode::OK);
        assert_eq!(
            get_json(&site, RUN)["status"],
            "cancelling",
            "a second Stop at once is the same request"
        );

        site.runner().unwrap().backdate_stop();
        assert_eq!(
            get_json(&site, RUN)["forceIn"],
            0,
            "the page is told the agent had its time"
        );
        assert_eq!(post(&site, &cancel, "").0, StatusCode::OK);
        let failed = get_json(&site, RUN);
        assert_eq!(
            failed["forceIn"],
            serde_json::Value::Null,
            "only while stopping"
        );
        assert_eq!(failed["status"], "failed");
        let last = failed["entries"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let error = last["error"].as_str().unwrap();
        assert!(error.contains("did not stop when asked"), "{error}");
        // Its exit is not reported, so the forced stop takes the snapshot after it.
        let changes = get_json(&site, &format!("{RUN}/changes?from=R1"));
        assert!(changes["after"].is_string(), "{changes}");

        // The working tree is free again, with a new agent: its first prompt.
        let (status, body) = send(&site, &["N1"]);
        assert_eq!(status, StatusCode::OK, "{body}");
        loop {
            let run = get_json(&site, RUN);
            if kinds(&run).contains(&"message:prompt 1".to_owned()) {
                assert_eq!(run["id"], "R2");
                break;
            }
            nudges
                .recv_timeout(WAIT)
                .unwrap_or_else(|_| panic!("the new agent never answered: {run}"));
        }
    }

    #[test]
    fn another_window_cannot_start_a_run_on_the_same_working_tree() {
        let repo = TempRepo::new("run-two-windows");
        repo.write("a.txt", "a\n");
        let (first, nudges) = run_site(&repo, "hang");
        post_note(&first, "a.txt", 1, 1, "one");
        send(&first, &["N1"]);
        wait_for(&first, &nudges, "running");
        // Made after the first send: both windows read the one settings file.
        let (second, _second_nudges) = run_site(&repo, "plain");

        let (status, body) = send(&second, &["N1"]);
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("another Postil window"), "{body}");

        // Once the first window's run ends, the second may run.
        let cancel = format!("{RUN}/cancel");
        post(&first, &cancel, "");
        first.runner().unwrap().backdate_stop();
        post(&first, &cancel, "");
        let (status, body) = send(&second, &["N1"]);
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[test]
    fn a_kept_agent_that_exited_is_started_again() {
        let repo = TempRepo::new("run-dead-kept");
        repo.write("a.txt", "a\n");
        let pid_file = repo.root.join(".git/agent.pid");
        let (site, nudges) = run_site(&repo, "plain");
        let command = format!("{} {}", fake_agent("exit-after-turn"), pid_file.display());
        set_agents(&site, &[("Fake", &command)], "Fake");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "ended");
        // The exited agent is reaped when the runner drops it on its exit event, so once it
        // is gone the runner holds no agent.
        wait_for_exit(&pid_file);

        send(&site, &["N1"]);
        let second = wait_for(&site, &nudges, "ended");
        assert_eq!(second["id"], "R2");
        assert!(
            kinds(&second).contains(&"message:prompt 1".to_owned()),
            "a new process: {second}"
        );
    }

    #[test]
    fn run_ids_continue_after_the_highest_log_on_disk() {
        let repo = TempRepo::new("run-ids");
        repo.write("a.txt", "a\n");
        let runs = repo.root.join(".git/postil/runs");
        fs::create_dir_all(&runs).unwrap();
        let header =
            r#"{"version":1,"id":"R9","agent":"Fake","notes":["N1"],"prompt":"p","startedAt":1}"#;
        fs::write(runs.join("R9.jsonl"), format!("{header}\n")).unwrap();
        let (site, nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        assert_eq!(wait_for(&site, &nudges, "ended")["id"], "R10");
    }

    #[test]
    fn a_damaged_run_counter_is_repaired_by_the_next_run() {
        // A crash leaves a zero-filled tail; damage from outside can leave bytes that are not
        // UTF-8. Neither parses, so both read as no number.
        for (case, damage) in [("zeros", &b"5\0\0"[..]), ("not-utf8", b"5\0\xff")] {
            let repo = TempRepo::new(&format!("run-counter-{case}"));
            repo.write("a.txt", "a\nb\n");
            let runs = repo.root.join(".git/postil/runs");
            fs::create_dir_all(&runs).unwrap();
            fs::write(runs.join("active.lock"), damage).unwrap();
            let (site, nudges) = run_site(&repo, "plain");
            post_note(&site, "a.txt", 1, 1, "one");
            post_note(&site, "a.txt", 2, 2, "two");
            send(&site, &["N1"]);
            assert_eq!(wait_for(&site, &nudges, "ended")["id"], "R1", "{case}");

            // R1 goes with N1; its number must not come back, which needs the counter readable.
            delete(&site, &format!("{NOTES}?id=N1"));
            send(&site, &["N2"]);
            assert_eq!(wait_for(&site, &nudges, "ended")["id"], "R2", "{case}");
        }
    }

    #[test]
    fn bad_run_requests_name_the_problem_and_make_no_run() {
        let repo = TempRepo::new("run-bad");
        repo.write("a.txt", "a\n");
        let (site, _nudges) = run_site(&repo, "plain");
        post_note(&site, "a.txt", 1, 1, "one");

        let refused = |body: &str, status: StatusCode, words: &str| {
            let (got, reply) = post(&site, RUN, body);
            assert_eq!(got, status, "{body}: {reply}");
            assert!(reply.contains(words), "{body}: {reply}");
        };
        refused(
            r#"{"notes": []}"#,
            StatusCode::BAD_REQUEST,
            "no notes were chosen",
        );
        refused(
            r#"{"notes": ["N1", "N1"]}"#,
            StatusCode::BAD_REQUEST,
            "chosen twice",
        );
        refused(
            r#"{"notes": ["N7"]}"#,
            StatusCode::NOT_FOUND,
            "there is no note N7",
        );
        refused(
            r#"{"notes": ["x"]}"#,
            StatusCode::BAD_REQUEST,
            "expected N and a number",
        );
        refused(
            r#"{"notes": ["N1"], "extra": 1}"#,
            StatusCode::BAD_REQUEST,
            "unreadable",
        );
        refused(
            r#"{"agent": "Nobody", "notes": ["N1"]}"#,
            StatusCode::BAD_REQUEST,
            "turn Nobody on in Settings",
        );
        let (status, body) = save(&site, r#"{"customAgents": [], "enabledAgents": []}"#);
        assert_eq!(status, StatusCode::OK, "{body}");
        refused(
            r#"{"notes": ["N1"]}"#,
            StatusCode::BAD_REQUEST,
            "choose the agent notes are sent to",
        );
        assert_eq!(get(&site, RUN).1, "null", "a refused send makes no run");

        assert_eq!(
            get(&site, &format!("{RUN}?after=x")).0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get(&site, &format!("{RUN}/preview")).0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get(&site, &format!("{RUN}/cancel")).0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            get(&site, &format!("{RUN}/permission")).0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            post(&site, &format!("{RUN}/preview?notes=N1"), "").0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        let answer = r#"{"request": 0, "option": "once"}"#;
        let (status, body) = post(&site, &format!("{RUN}/permission"), answer);
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "no run, so no request: {body}"
        );
    }

    // Pane widths (layout.rs).

    const LAYOUT: &str = "postil://localhost/api/layout";

    #[test]
    fn pane_widths_are_kept_per_mac_with_their_limits_and_defaults() {
        let repo = TempRepo::new("layout");
        let site = repo.empty_site();
        let defaults = serde_json::json!({ "sidebar": 248, "tray": 320 });
        let fresh = get_json(&site, LAYOUT);
        assert_eq!(
            fresh,
            serde_json::json!({
                "sidebar": 248, "tray": 320,
                "defaults": defaults,
                "limits": { "sidebar": [200, 480], "tray": [280, 720] },
            })
        );

        let (status, body) = put(&site, LAYOUT, r#"{"sidebar": 300, "tray": 460}"#);
        assert_eq!(status, StatusCode::OK, "{body}");
        // Another window, or the next start, reads the same widths: they are the Mac's, not
        // the project's.
        let other = TempRepo::new("layout-other");
        let again = Site::new(Some(&repo.config()), None, no_agents(), quiet());
        again.open(Repo::open(&other.root).unwrap());
        let kept = get_json(&again, LAYOUT);
        assert_eq!(
            (kept["sidebar"].clone(), kept["tray"].clone()),
            (300.into(), 460.into())
        );

        for (body, words) in [
            (r#"{"sidebar": 100, "tray": 320}"#, "outside 200 to 480"),
            (r#"{"sidebar": 248}"#, "unreadable layout"),
            (
                r#"{"sidebar": 248, "tray": 320, "x": 1}"#,
                "unreadable layout",
            ),
        ] {
            let (status, reply) = put(&site, LAYOUT, body);
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
            assert!(reply.contains(words), "{body}: {reply}");
        }
        assert_eq!(
            get_json(&site, LAYOUT)["sidebar"],
            300,
            "a refused layout changes nothing"
        );
        assert_eq!(post(&site, LAYOUT, "{}").0, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn without_a_configuration_directory_widths_are_the_defaults_and_not_kept() {
        let site = Site::new(None, None, no_agents(), quiet());
        assert_eq!(get_json(&site, LAYOUT)["tray"], 320);
        let (status, body) = put(&site, LAYOUT, r#"{"sidebar": 300, "tray": 460}"#);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(body.contains("HOME"), "{body}");
    }
}
