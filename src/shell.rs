//! The window, the webview, and the URL scheme the page talks to.
//!
//! The page is served from `postil://localhost/`. It is compiled into the binary, and it
//! gets its data by `fetch` on the same scheme, which [`Site::respond`] (`site.rs`) answers
//! in this process, on the request thread this module starts ([`spawn_site_worker`]). No
//! socket is opened.
//!
//! One window shows one project at a time. Without one, the page shows the welcome screen
//! (open a folder, the recent projects, or a folder dropped on the window); opening another
//! project replaces the current one and the page reloads.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SendError, Sender};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use tao::dpi::LogicalPosition;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
#[cfg(target_os = "macos")]
use tao::platform::macos::WindowBuilderExtMacOS;
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Theme, Window, WindowBuilder};
use wry::http::{Request, StatusCode};
use wry::{DragDropEvent, PageLoadEvent, WebView, WebViewBuilder};

use crate::dialog;
use crate::dictation::{self, Dictation, StartOptions};
use crate::error::{Error, Result};
#[cfg(target_os = "macos")]
use crate::menu::Command;
use crate::run::Notify;
use crate::settings;
use crate::site::{Reply, Site, page_asset, reply, start_site};

/// The `--canvas` colors of `ui/app.css`, painted behind the page so the window does not
/// flash white before the first frame.
const DARK_CANVAS: (u8, u8, u8, u8) = (0x15, 0x17, 0x1b, 0xff);
const LIGHT_CANVAS: (u8, u8, u8, u8) = (0xf4, 0xf4, 0xf1, 0xff);

/// What the benchmark measured.
#[derive(Debug)]
pub struct BenchReport {
    /// From the start of [`bench()`] to the page reporting that the file is painted.
    pub launch_to_paint: Duration,
    /// The page's own measurements, as the JSON object `ui/bench.js` builds in `runBench`.
    pub page: String,
}

impl BenchReport {
    /// Why the run missed a budget the page checks, in words, or `None` when it kept every one:
    /// today the map's 200-node layout (`diagram` in `ui/bench.js`). A page report without that
    /// part, or one that cannot be read, misses it: the bench would otherwise pass unmeasured.
    pub fn over_budget(&self) -> Option<String> {
        let page: serde_json::Value = match serde_json::from_str(&self.page) {
            Ok(page) => page,
            Err(error) => return Some(format!("the page's report is not JSON: {error}")),
        };
        let diagram = &page["diagram"];
        match diagram["withinBudget"].as_bool() {
            Some(true) => None,
            Some(false) => Some(format!(
                "the 200-node map layout took {} ms, over its {} ms budget",
                diagram["layoutMs"], diagram["budgetMs"]
            )),
            None => Some("the page's report has no map layout time".to_owned()),
        }
    }
}

/// Opens the window and returns when it closes. With `dir`, the window opens on the
/// repository that contains it; without, on the welcome screen.
///
/// # Errors
///
/// Fails when `dir` is not inside a git working tree, or when the window, the webview or the
/// thread that answers the page cannot be created.
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

/// What reaches the event loop: the page's messages (through `window.ipc.postMessage`),
/// folders dropped on the window, menu commands, and a nudge when a run changed.
#[derive(Debug)]
enum AppEvent {
    /// The pointer went down on the window's drag strip.
    DragWindow,
    /// The page opened a project and wants the window to fill the screen.
    Maximize,
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
    /// The request thread has closed the site, so the app may exit ([`Job::Close`]).
    SiteClosed,
    /// The page asked to start native dictation with optional vocabulary hints.
    DictationStart(StartOptions),
    /// The page asked to stop dictation gracefully (delivering any pending final result).
    DictationStop,
    /// The page or window asked to cancel dictation immediately.
    DictationCancel,
    /// Native dictation produced an event for `window.postil.dictation`.
    DictationEvent(dictation::Event),
    /// Internal dictation callback marshaled back to the main event loop.
    DictationInternal(dictation::Internal),
}

impl AppEvent {
    /// Reads a message as the page (`ui/`) posts it.
    fn parse(body: &str) -> Option<Self> {
        match body {
            "drag" => Some(Self::DragWindow),
            "maximize" => Some(Self::Maximize),
            "choose-folder" => Some(Self::ChooseFolder),
            "quit" => Some(Self::Quit),
            "bench-painted" => Some(Self::BenchPainted),
            "theme:system" => Some(Self::SetTheme(settings::Theme::System)),
            "theme:light" => Some(Self::SetTheme(settings::Theme::Light)),
            "theme:dark" => Some(Self::SetTheme(settings::Theme::Dark)),
            "dictation:start" => Some(Self::DictationStart(StartOptions::default())),
            "dictation:stop" => Some(Self::DictationStop),
            "dictation:cancel" => Some(Self::DictationCancel),
            _ => {
                if let Some(json) = body.strip_prefix("dictation:start:") {
                    StartOptions::parse_json(json).map(Self::DictationStart)
                } else {
                    body.strip_prefix("bench:")
                        .map(|report| Self::BenchDone(report.to_owned()))
                }
            }
        }
    }
}

fn serve(dir: Option<&Path>, bench: Option<&str>) -> Result<Option<BenchReport>> {
    let started = Instant::now();
    let mut event_loop: EventLoop<AppEvent> = EventLoopBuilder::with_user_event().build();
    let run_proxy = event_loop.create_proxy();
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
            drop(run_proxy.send_event(AppEvent::RunChanged));
        }
    });
    let (site, settings) = start_site(dir, bench, notify)?;
    // A failure before the event loop runs returns without waiting for the request thread to
    // drop the site: no page has started a run yet, so there is no agent to stop.
    let closed = event_loop.create_proxy();
    let jobs = spawn_site_worker(site, move || {
        // Fails only once the event loop is gone, when nobody waits for it.
        drop(closed.send_event(AppEvent::SiteClosed));
    })?;

    let window = build_window(&event_loop, settings.theme(), bench.is_none())?;

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
    let webview = build_webview(&event_loop, &window, jobs.clone(), Rc::clone(&committed))?;
    let dict_event_proxy = event_loop.create_proxy();
    let dict_internal_proxy = event_loop.create_proxy();
    let dictation = Dictation::new(dictation::Callbacks {
        on_event: Arc::new(move |event| {
            // Fails only once the event loop is gone.
            drop(dict_event_proxy.send_event(AppEvent::DictationEvent(event)));
        }),
        on_internal: Arc::new(move |internal| {
            // Fails only once the event loop is gone.
            drop(dict_internal_proxy.send_event(AppEvent::DictationInternal(internal)));
        }),
    });

    let mut app = App {
        page: Page {
            webview: &webview,
            committed: &committed,
            quit: event_loop.create_proxy(),
        },
        window: &window,
        jobs: &jobs,
        unnudge: &unnudge,
        dictation,
        started,
        quit_by: None,
        launch_to_paint: None,
        report: None,
    };
    event_loop.run_return(|event, _, control_flow| on_event(&mut app, event, control_flow));
    app.dictation.cancel();
    let report = app.report;

    // The webview draws into the window, so it goes first. The request thread is not joined:
    // it has closed the site, or the quit deadline passed (`QUIT_WAIT`) with the site still
    // open, and a request it may be on would wait for this thread's run loop, which has
    // stopped. The process ending stops it.
    drop(webview);
    drop(window);
    Ok(report)
}

/// What the event loop's handler ([`on_event`]) reads and changes while the window is open.
#[derive(Debug)]
struct App<'a> {
    page: Page<'a>,
    window: &'a Window,
    jobs: &'a Sender<Job>,
    /// Set by the notifier when a run changed, until the page is told (see `serve`).
    unnudge: &'a AtomicBool,
    /// On-device dictation controller, owned strictly on the main thread.
    dictation: Dictation,
    /// When `serve` started, for the benchmark's launch-to-paint time.
    started: Instant,
    /// Set by the first quit: the event loop exits by then even if the site never closes.
    quit_by: Option<Instant>,
    launch_to_paint: Option<Duration>,
    /// The benchmark's report, once the page sent it.
    report: Option<BenchReport>,
}

/// Handles one event of the event loop.
fn on_event(app: &mut App<'_>, event: Event<'_, AppEvent>, control_flow: &mut ControlFlow) {
    *control_flow = waiting(app.quit_by);
    match event {
        // The close button does what Close Window does, so a draft note stops it too.
        Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } => ask_page(&app.page, "close-window", Unanswered::Quit),
        Event::UserEvent(AppEvent::Quit) => {
            app.dictation.cancel();
            // Gone from the screen at once: closing the site can take a while (a slow git
            // call queued ahead of it, the snapshot after a live run).
            app.window.set_visible(false);
            close_site(app.jobs, &mut app.quit_by, control_flow);
        }
        Event::WindowEvent {
            event: WindowEvent::Focused(false),
            ..
        } => app.dictation.on_focus_lost(),
        Event::UserEvent(AppEvent::SiteClosed) => *control_flow = ControlFlow::Exit,
        // The system switched between light and dark while the window follows it.
        Event::WindowEvent {
            event: WindowEvent::ThemeChanged(theme),
            ..
        } => drop(app.page.webview.set_background_color(canvas_color(theme))),
        Event::UserEvent(AppEvent::DragWindow) => {
            // Fails when the pointer button is already up again: there is then nothing
            // to drag.
            drop(app.window.drag_window());
        }
        Event::UserEvent(AppEvent::Maximize) => app.window.set_maximized(true),
        Event::UserEvent(AppEvent::ChooseFolder) => {
            if let Some(folder) = dialog::choose_folder() {
                offer_folder(app.page.webview, &folder);
            }
        }
        Event::UserEvent(AppEvent::Dropped(path)) => offer_folder(app.page.webview, &path),
        Event::UserEvent(AppEvent::SetTheme(theme)) => {
            app.window.set_theme(window_theme(theme));
            // Behind the page, seen where it has not painted yet, such as during a resize.
            // Fails only when the webview is being torn down.
            drop(
                app.page
                    .webview
                    .set_background_color(canvas_color(app.window.theme())),
            );
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
                | Command::Usage
                | Command::SetupWizard
                | Command::FocusMode
                | Command::Map
                | Command::MapMarkers => Unanswered::Ignore,
            };
            ask_page(&app.page, command.id(), unanswered);
        }
        Event::UserEvent(AppEvent::RunChanged) => {
            // Cleared before the page is told, so a change during its fetch nudges again.
            app.unnudge.store(false, Ordering::SeqCst);
            // A page that has not loaded yet fetches the run when it starts. Fails only
            // when the webview is being torn down.
            drop(
                app.page
                    .webview
                    .evaluate_script("window.postil?.runChanged?.()"),
            );
        }
        Event::UserEvent(AppEvent::DictationStart(options)) => app.dictation.start(options),
        Event::UserEvent(AppEvent::DictationStop) => app.dictation.stop(),
        Event::UserEvent(AppEvent::DictationCancel) => app.dictation.cancel(),
        Event::UserEvent(AppEvent::DictationInternal(internal)) => {
            app.dictation.handle_internal(internal);
        }
        Event::UserEvent(AppEvent::DictationEvent(event)) => {
            let json =
                serde_json::to_string(&event).expect("a dictation event always serializes to JSON");
            // Fails only when the webview is being torn down.
            drop(
                app.page
                    .webview
                    .evaluate_script(&format!("window.postil?.dictation?.emit?.({json})")),
            );
        }
        Event::UserEvent(AppEvent::BenchPainted) => {
            app.launch_to_paint = Some(app.started.elapsed());
        }
        Event::UserEvent(AppEvent::BenchDone(page)) => {
            app.report = Some(BenchReport {
                // The page posts `bench-painted` before `bench:`; if it somehow did
                // not, the time until now is the honest upper bound.
                launch_to_paint: app.launch_to_paint.unwrap_or_else(|| app.started.elapsed()),
                page,
            });
            close_site(app.jobs, &mut app.quit_by, control_flow);
        }
        _ => {}
    }
}

/// How the request thread hands an answer back: to wry's responder in the app, to a channel
/// in the tests.
type Answer = Box<dyn FnOnce(Reply) + Send>;

/// What the request thread is asked to do ([`spawn_site_worker`]).
#[expect(
    clippy::large_enum_variant,
    reason = "a handful of jobs is queued at a time; boxing would save a few hundred bytes"
)]
enum Job {
    /// Answer a request from the page.
    Answer(Request<Vec<u8>>, Answer),
    /// Drop the site, which stops the agent of a run under way, then say so. Later requests
    /// are dropped unanswered: the page is closing.
    Close,
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Answer(request, _) => f
                .debug_tuple("Answer")
                .field(&request.uri())
                .finish_non_exhaustive(),
            Self::Close => f.write_str("Close"),
        }
    }
}

/// Starts the thread that answers the page's requests and returns the way to send it jobs.
/// It owns `site` and does each job in the order it arrives; on [`Job::Close`] it drops the
/// site, then calls `closed`.
///
/// wry calls the URL scheme handler on the main thread, and answering there would stop the
/// window (input, redraws, Stop) for as long as git, a file read or highlighting takes, so
/// the handler only queues the request. One thread rather than one per request: requests
/// answered one at a time need no lock on `Site` (its project is a `RefCell`), `open` can
/// check for a live run and switch projects with nothing in between, and a request the page
/// sent first (a folder save) lands before a later one (an open). The queue is unbounded: its
/// sender is the page's fetches, a handful at a time, and a bounded queue that filled up
/// would block the main thread.
///
/// An answer may come from any thread (wry's `RequestAsyncResponder` exists for that), but
/// WebKit delivers it on the main thread's run loop and waits for that. So the main thread
/// must never wait for this one while its run loop is stopped: the site is closed by a job
/// while the event loop still runs, and the event loop exits once `closed` tells it.
fn spawn_site_worker(site: Site, closed: impl Fn() + Send + 'static) -> Result<Sender<Job>> {
    let (sender, jobs) = mpsc::channel::<Job>();
    thread::Builder::new()
        .name("page-requests".to_owned())
        .spawn(move || {
            let mut site = Some(site);
            for job in jobs {
                match job {
                    Job::Answer(request, answer) => {
                        // After a close the request is dropped unanswered: answering goes
                        // through the main run loop, which may have stopped by then.
                        if let Some(site) = &site {
                            answer(site.respond(&request));
                        }
                    }
                    Job::Close => {
                        drop(site.take());
                        closed();
                    }
                }
            }
        })
        .map_err(Error::SpawnWorker)?;
    Ok(sender)
}

/// How the event loop waits for its next event: until `quit_by` once the app is quitting, and
/// past it not at all.
fn waiting(quit_by: Option<Instant>) -> ControlFlow {
    match quit_by {
        Some(deadline) if Instant::now() >= deadline => ControlFlow::Exit,
        Some(deadline) => ControlFlow::WaitUntil(deadline),
        None => ControlFlow::Wait,
    }
}

/// How long quitting waits for the request thread to close the site before the app exits
/// anyway. Closing stops a run's agent and takes the snapshot after the run (seconds for a
/// large tree: 3.5 s measured for a 200 MB untracked file, docs/design.md), after any request
/// queued ahead of it. Arbitrary above that: long enough for those, short enough that a
/// request stuck on a dead network volume does not keep a hidden app running. Exiting at the
/// deadline can leave that run's agent behind; accepted over an app that never quits.
const QUIT_WAIT: Duration = Duration::from_secs(10);

/// Asks the request thread to close the site; the event loop exits once it has
/// ([`AppEvent::SiteClosed`]), at `quit_by` (set to [`QUIT_WAIT`] from the first ask), or at
/// once when that thread is gone. Asking twice is harmless: the second close finds no site
/// and says so again.
fn close_site(jobs: &Sender<Job>, quit_by: &mut Option<Instant>, control_flow: &mut ControlFlow) {
    if jobs.send(Job::Close).is_err() {
        *control_flow = ControlFlow::Exit;
        return;
    }
    let deadline = *quit_by.get_or_insert_with(|| Instant::now() + QUIT_WAIT);
    *control_flow = ControlFlow::WaitUntil(deadline);
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

/// The webview that shows the page in `window`, its requests sent to `jobs` (see
/// [`spawn_site_worker`]). It sets `committed` once the page's first navigation commits (see
/// [`Page`]).
fn build_webview(
    event_loop: &EventLoop<AppEvent>,
    window: &Window,
    jobs: Sender<Job>,
    committed: Rc<Cell<bool>>,
) -> Result<WebView> {
    let page_proxy = event_loop.create_proxy();
    let drop_proxy = event_loop.create_proxy();
    let load_proxy = event_loop.create_proxy();
    let file_drag = RefCell::new(FileDrag::default());
    WebViewBuilder::new()
        .with_background_color(canvas_color(window.theme()))
        .with_asynchronous_custom_protocol("postil".to_owned(), move |_webview, request, answer| {
            // The page's own files are static bytes, answered here (owner decision D1 in
            // docs/review-2026-11-23.md): a page reload is never queued behind a slow request.
            if let Some(asset) = page_asset(request.uri().path()) {
                answer.respond(asset);
                return;
            }
            let answer: Answer = Box::new(move |reply| answer.respond(reply));
            // Fails only when the request thread is gone, which in a debug build means it
            // panicked (a release build aborts on a panic): the request is then answered here,
            // so the page shows an error instead of waiting forever.
            if let Err(SendError(Job::Answer(_, answer))) = jobs.send(Job::Answer(request, answer))
            {
                let problem = "the thread that answers the page stopped; restart Postil";
                answer(reply(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "text/plain",
                    problem.as_bytes(),
                ));
            }
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
        // not reach the `postil://` handlers, which read and write the repository. An https
        // link (a thread's pull request) opens in the default browser instead; `open` returns
        // once it has handed the URL over, and is waited for off the event loop.
        .with_navigation_handler(|url| {
            if url.starts_with("https://") {
                let opened = thread::Builder::new()
                    .name("open-link".to_owned())
                    .spawn(move || drop(process::Command::new("/usr/bin/open").arg(url).status()));
                // No thread: the link does nothing, as any other would.
                drop(opened);
                return false;
            }
            url.starts_with("postil://")
        })
        // On macOS, wry fires `Started` from `didCommitNavigation`, before it runs the scripts
        // it queued and stops queueing (wry 0.57.3, `wkwebview/navigation.rs`,
        // `did_commit_navigation`); its docs say only "started loading", and other platforms'
        // backends are unchecked. `ask_page` relies on that order, and no test can check it
        // without a window: recheck it when bumping wry or adding a platform.
        .with_on_page_load_handler(move |event, _| {
            if matches!(event, PageLoadEvent::Started) {
                committed.set(true);
                // Switching or reloading a project stops any active dictation and releases the
                // microphone.
                drop(load_proxy.send_event(AppEvent::DictationCancel));
            }
        })
        .with_url("postil://localhost/")
        .build(window)
        .map_err(Error::Webview)
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

impl fmt::Debug for Page<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The webview is left out: wry's `WebView` has no `Debug`.
        f.debug_struct("Page")
            .field("committed", &self.committed)
            .field("quit", &self.quit)
            .finish_non_exhaustive()
    }
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
///
/// `maximized` fills the screen on start (left `false` for `--bench`, which keeps the fixed
/// 1440 x 900 viewport for repeatable timings).
fn build_window(
    event_loop: &EventLoop<AppEvent>,
    theme: settings::Theme,
    maximized: bool,
) -> Result<Window> {
    let builder = WindowBuilder::new()
        .with_theme(window_theme(theme))
        .with_title("Postil")
        .with_inner_size(LogicalSize::new(1440.0, 900.0))
        .with_maximized(maximized);

    // No title bar: the page draws up to the top edge and the window buttons sit over the
    // sidebar, 14 px in and 16 px down to line up with the sidebar's 44 px top strip.
    #[cfg(target_os = "macos")]
    let builder = builder
        .with_titlebar_transparent(true)
        .with_fullsize_content_view(true)
        .with_title_hidden(true)
        .with_traffic_light_inset(LogicalPosition::new(14.0, 16.0));

    builder.build(event_loop).map_err(Error::Window)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::site::tests::{
        APP_CSS, TempRepo, plain_folder, post_note, run_site, send, wait_for,
    };

    fn drag(paths: &[&Path]) -> Vec<PathBuf> {
        paths.iter().map(|path| path.to_path_buf()).collect()
    }

    #[test]
    fn a_bench_over_the_map_layout_budget_or_without_it_fails() {
        let report = |page: &str| BenchReport {
            launch_to_paint: Duration::ZERO,
            page: page.to_owned(),
        };
        let within = r#"{"diagram": {"layoutMs": 31, "budgetMs": 150, "withinBudget": true}}"#;
        assert_eq!(report(within).over_budget(), None);
        let over = r#"{"diagram": {"layoutMs": 180, "budgetMs": 150, "withinBudget": false}}"#;
        assert_eq!(
            report(over).over_budget().as_deref(),
            Some("the 200-node map layout took 180 ms, over its 150 ms budget")
        );
        assert!(
            report(r#"{"frames": 300}"#).over_budget().is_some(),
            "unmeasured"
        );
        assert!(report("not json").over_budget().is_some());
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
    fn page_messages_parse_as_the_page_posts_them() {
        assert!(matches!(
            AppEvent::parse("drag"),
            Some(AppEvent::DragWindow)
        ));
        assert!(matches!(
            AppEvent::parse("maximize"),
            Some(AppEvent::Maximize)
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
        assert!(matches!(
            AppEvent::parse("dictation:start"),
            Some(AppEvent::DictationStart(opts)) if opts.context.is_empty()
        ));
        assert!(matches!(
            AppEvent::parse(r#"dictation:start:{"context":["src/run.rs"]}"#),
            Some(AppEvent::DictationStart(opts)) if opts.context == ["src/run.rs"]
        ));
        assert!(matches!(
            AppEvent::parse("dictation:stop"),
            Some(AppEvent::DictationStop)
        ));
        assert!(matches!(
            AppEvent::parse("dictation:cancel"),
            Some(AppEvent::DictationCancel)
        ));
        assert!(AppEvent::parse("dictation:start:not-json").is_none());
        assert!(AppEvent::parse("nonsense").is_none());
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
    fn the_event_loop_waits_until_the_quit_deadline_and_exits_past_it() {
        assert_eq!(
            waiting(None),
            ControlFlow::Wait,
            "not quitting: wait for events"
        );
        let ahead = Instant::now() + Duration::from_secs(60);
        assert_eq!(waiting(Some(ahead)), ControlFlow::WaitUntil(ahead));
        // A deadline of now has passed: the clock `waiting` reads is at or after it.
        let now = Instant::now();
        assert_eq!(waiting(Some(now)), ControlFlow::Exit);
    }

    #[test]
    fn the_request_thread_answers_in_order_then_closes_the_site_before_saying_so() {
        let repo = TempRepo::new("worker-close");
        repo.write("a.txt", "a\n");
        let (site, nudges) = run_site(&repo, "ask");
        post_note(&site, "a.txt", 1, 1, "one");
        send(&site, &["N1"]);
        wait_for(&site, &nudges, "waiting");

        let (closed_sender, closed) = mpsc::channel();
        let jobs =
            spawn_site_worker(site, move || closed_sender.send(()).unwrap_or_default()).unwrap();
        let (answer_sender, answers) = mpsc::channel();
        let ask = |uri: &str, tag: &'static str| {
            let answers = answer_sender.clone();
            let request = Request::builder().uri(uri).body(Vec::new()).unwrap();
            let answer: Answer =
                Box::new(move |reply| answers.send((tag, reply.status())).unwrap());
            jobs.send(Job::Answer(request, answer)).unwrap();
        };
        ask("postil://localhost/api/repo", "first");
        ask("postil://localhost/api/notes", "second");
        jobs.send(Job::Close).unwrap();
        ask("postil://localhost/api/repo", "after");
        // Every answer now held is the request thread's: once it has answered or dropped each,
        // the channel closes.
        drop(answer_sender);

        closed
            .recv_timeout(Duration::from_secs(60))
            .expect("the request thread says it closed the site");
        // Said only once the site is gone: the live run has already ended, stopped.
        let log = fs::read_to_string(repo.root.join(".git/postil/runs/R1.jsonl")).unwrap();
        let end = log.lines().last().unwrap();
        assert!(
            end.contains("Postil closed the project while it ran"),
            "{end}"
        );
        // The request after the close is dropped unanswered: answering it could wait on a
        // main run loop that has stopped.
        let answered: Vec<_> = answers.iter().collect();
        assert_eq!(
            answered,
            [("first", StatusCode::OK), ("second", StatusCode::OK)]
        );
    }
}
