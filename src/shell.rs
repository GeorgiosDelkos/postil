//! The window, the webview, and the URL scheme the page talks to.
//!
//! The page is served from `postil://localhost/`. It is compiled into the binary, and it
//! gets its data by `fetch` on the same scheme, which [`Site::respond`] answers in this
//! process. No socket is opened.

use std::borrow::Cow;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Theme, Window, WindowBuilder};
use wry::WebViewBuilder;
use wry::http::header::CONTENT_TYPE;
use wry::http::{Method, Request, Response, StatusCode};

use crate::error::{Error, Result};
use crate::highlight::{Highlighter, Token};
use crate::notes::{self, NewNote};
use crate::repo::Repo;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");
const FUZZY_JS: &str = include_str!("../ui/fuzzy.js");
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

/// Opens the review window for the repository that contains `dir` and returns when the
/// window closes.
///
/// # Errors
///
/// Fails when `dir` is not inside a git working tree, or when the window or the webview
/// cannot be created.
pub fn run(dir: &Path) -> Result<()> {
    serve(dir, None).map(|_| ())
}

/// Opens the window, shows `file`, scrolls through it, and returns the measurements.
///
/// `file` is relative to the repository root.
///
/// # Errors
///
/// As [`run`], and [`Error::BenchInterrupted`] when the window closes first.
pub fn bench(dir: &Path, file: &str) -> Result<BenchReport> {
    serve(dir, Some(file))?.ok_or(Error::BenchInterrupted)
}

/// What the page tells the event loop through `window.ipc.postMessage`.
enum PageMessage {
    /// The pointer went down on the window's drag strip.
    DragWindow,
    /// The benchmark's file is on screen.
    BenchPainted,
    /// The benchmark finished; the payload is its JSON report.
    BenchDone(String),
}

impl PageMessage {
    /// Reads a message as `ui/app.js` posts it.
    fn parse(body: &str) -> Option<Self> {
        match body {
            "drag" => Some(Self::DragWindow),
            "bench-painted" => Some(Self::BenchPainted),
            _ => body
                .strip_prefix("bench:")
                .map(|report| Self::BenchDone(report.to_owned())),
        }
    }
}

fn serve(dir: &Path, bench: Option<&str>) -> Result<Option<BenchReport>> {
    let started = Instant::now();
    let site = Site {
        repo: Repo::open(dir)?,
        highlighter: Highlighter::new(),
        bench: bench.map(str::to_owned),
    };
    // A benchmark of a file that cannot be read would time an empty pane and report it as a
    // result. Fail here instead, before the window opens.
    if let Some(file) = bench {
        site.repo.read(file)?;
    }

    let mut event_loop: EventLoop<PageMessage> = EventLoopBuilder::with_user_event().build();
    let window = build_window(&event_loop)?;
    let canvas = match window.theme() {
        Theme::Dark => DARK_CANVAS,
        _ => LIGHT_CANVAS,
    };

    let proxy = event_loop.create_proxy();
    let webview = WebViewBuilder::new()
        .with_background_color(canvas)
        .with_custom_protocol("postil".to_owned(), move |_webview, request| {
            site.respond(&request)
        })
        .with_ipc_handler(move |message| {
            let message = PageMessage::parse(message.body());
            // The page and this handler are compiled into one binary, so a message that
            // does not parse is a bug in one of them.
            debug_assert!(message.is_some(), "unknown page message");
            if let Some(message) = message {
                // Sending fails only once the event loop is gone, when nobody is left to
                // act on the message.
                drop(proxy.send_event(message));
            }
        })
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
            } => *control_flow = ControlFlow::Exit,
            Event::UserEvent(PageMessage::DragWindow) => {
                // Fails when the pointer button is already up again: there is then nothing
                // to drag.
                drop(window.drag_window());
            }
            Event::UserEvent(PageMessage::BenchPainted) => {
                launch_to_paint = Some(started.elapsed());
            }
            Event::UserEvent(PageMessage::BenchDone(page)) => {
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

fn build_window(event_loop: &EventLoop<PageMessage>) -> Result<Window> {
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
struct Site {
    repo: Repo,
    highlighter: Highlighter,
    /// The file the benchmark opens, when this is a benchmark run.
    bench: Option<String>,
}

/// The reply to `/api/repo`.
#[derive(Serialize)]
struct RepoInfo<'a> {
    name: String,
    branch: String,
    files: Vec<String>,
    bench: Option<&'a str>,
}

/// The reply to `/api/file`.
#[derive(Serialize)]
struct FileView<'a> {
    path: &'a str,
    lines: Vec<Vec<Token<'a>>>,
}

type Reply = Response<Cow<'static, [u8]>>;

impl Site {
    fn respond(&self, request: &Request<Vec<u8>>) -> Reply {
        let uri = request.uri();
        match uri.path() {
            "/" => reply(StatusCode::OK, "text/html", INDEX_HTML.as_bytes()),
            "/app.css" => reply(StatusCode::OK, "text/css", APP_CSS.as_bytes()),
            "/app.js" => reply(StatusCode::OK, "text/javascript", APP_JS.as_bytes()),
            "/fuzzy.js" => reply(StatusCode::OK, "text/javascript", FUZZY_JS.as_bytes()),
            "/fonts/Geist-Variable.woff2" => reply(StatusCode::OK, "font/woff2", GEIST),
            "/fonts/GeistMono-Variable.woff2" => reply(StatusCode::OK, "font/woff2", GEIST_MONO),
            "/api/repo" => json_or_error(self.repo_info()),
            "/api/notes" => match *request.method() {
                Method::GET => json_or_error(self.notes()),
                Method::POST => json_or_error(self.add_note(request.body())),
                _ => reply(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "text/plain",
                    &b"GET the notes, or POST a new one"[..],
                ),
            },
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

    fn repo_info(&self) -> Result<Vec<u8>> {
        let info = RepoInfo {
            name: self.repo.name(),
            branch: self.repo.branch()?,
            files: self.repo.files()?,
            bench: self.bench.as_deref(),
        };
        Ok(to_json(&info))
    }

    fn file_view(&self, path: &str) -> Result<Vec<u8>> {
        let text = self.repo.read(path)?;
        let lines = self.highlighter.lines(path, &text)?;
        Ok(to_json(&FileView { path, lines }))
    }

    /// The reply to `GET /api/notes`: every note, moved to where its snippet is now. Reads
    /// the review file and never writes it.
    fn notes(&self) -> Result<Vec<u8>> {
        let review = notes::current(&self.repo.review_file(), |path| self.repo.read(path))?;
        Ok(to_json(&review))
    }

    /// The reply to `POST /api/notes`: adds the note in the body, saves, and returns all
    /// notes.
    fn add_note(&self, body: &[u8]) -> Result<Vec<u8>> {
        let new: NewNote = serde_json::from_slice(body)
            .map_err(|error| Error::InvalidNote(format!("unreadable request: {error}")))?;
        let review = notes::add(&self.repo.review_file(), new, |path| self.repo.read(path))?;
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
            let status = match error {
                Error::OutsideRepository { .. } | Error::InvalidNote(_) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
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

        fn site(&self) -> Site {
            Site {
                repo: Repo::open(&self.root).unwrap(),
                highlighter: Highlighter::new(),
                bench: None,
            }
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
        let request = serde_json::json!({
            "path": path,
            "lines": { "start": start, "end": end },
            "body": body,
        });
        let (status, reply) = post(site, NOTES, &request.to_string());
        assert_eq!(status, StatusCode::OK, "{reply}");
        serde_json::from_str(&reply).unwrap()
    }

    /// The notes' `[id, start, end, state]`, the fields that anchoring changes.
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
                    note["state"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    fn anchor(id: &str, start: u64, end: u64, state: &str) -> (String, u64, u64, String) {
        (id.to_owned(), start, end, state.to_owned())
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
        assert_eq!(note["state"], "open");

        let file = site.repo.review_file();
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
            [anchor("N1", 2, 3, "open"), anchor("N2", 1, 1, "open")]
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
            [anchor("N1", 4, 5, "open")]
        );
        // Reading never writes, so a hand edit cannot be overwritten by the page refreshing.
        let stored = || -> serde_json::Value {
            serde_json::from_str(&fs::read_to_string(site.repo.review_file()).unwrap()).unwrap()
        };
        assert_eq!(anchors(&stored()), [anchor("N1", 2, 3, "open")]);

        // Adding a note saves every note where it is now.
        post_note(&site, "src/a.rs", 1, 1, "On new 1.");
        assert_eq!(
            anchors(&stored()),
            [anchor("N1", 4, 5, "open"), anchor("N2", 1, 1, "open")]
        );

        repo.write("src/a.rs", "new 1\nnew 2\na\nb\nC\nd\n");
        assert_eq!(
            anchors(&get_json(&site, NOTES)),
            [anchor("N1", 4, 5, "outdated"), anchor("N2", 1, 1, "open")],
            "the snippet is gone: the note keeps its last saved lines"
        );

        repo.write("src/a.rs", "b\nc\n");
        assert_eq!(
            anchors(&get_json(&site, NOTES)),
            [anchor("N1", 1, 2, "open"), anchor("N2", 1, 1, "outdated")]
        );

        fs::remove_file(repo.root.join("src/a.rs")).unwrap();
        assert_eq!(
            anchors(&get_json(&site, NOTES)),
            [
                anchor("N1", 4, 5, "outdated"),
                anchor("N2", 1, 1, "outdated")
            ]
        );
    }

    #[test]
    fn a_review_file_that_cannot_be_read_or_written_is_reported_with_the_next_step() {
        let repo = TempRepo::new("notes-io");
        repo.write("a.txt", "x\n");
        let site = repo.site();
        let file = site.repo.review_file();
        let request = r#"{"path":"a.txt","lines":{"start":1,"end":1},"body":"b"}"#;

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
        let file = site.repo.review_file();
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let by_hand = r#"{"version": 1, "notes": [{"id": "N5", "path": "a.txt",
            "lines": {"start": 2, "end": 2}, "snippet": "y", "body": "From a script.",
            "state": "open"}]}"#;
        fs::write(&file, by_hand).unwrap();

        let unchanged = get_json(&site, NOTES);
        assert_eq!(anchors(&unchanged), [anchor("N5", 2, 2, "open")]);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            by_hand,
            "nothing moved, so the file is not rewritten"
        );

        let review = post_note(&site, "a.txt", 1, 1, "Mine.");
        assert_eq!(
            anchors(&review),
            [anchor("N5", 2, 2, "open"), anchor("N6", 1, 1, "open")]
        );
    }

    #[test]
    fn bad_note_requests_are_refused_and_nothing_is_written() {
        let repo = TempRepo::new("notes-bad");
        repo.write("a.txt", "x\ny\n");
        let site = repo.site();

        let cases = [
            (
                r#"{"path":"a.txt","lines":{"start":2,"end":3},"body":"b"}"#,
                "past the end",
            ),
            (
                r#"{"path":"a.txt","lines":{"start":0,"end":1},"body":"b"}"#,
                "1-based",
            ),
            (
                r#"{"path":"a.txt","lines":{"start":2,"end":1},"body":"b"}"#,
                "1-based",
            ),
            (
                r#"{"path":"a.txt","lines":{"start":1,"end":1},"body":" \n"}"#,
                "no text",
            ),
            (
                r#"{"path":"../a.txt","lines":{"start":1,"end":1},"body":"b"}"#,
                "inside",
            ),
            (r#"{"path":"a.txt","lines":{"start":1,"end":1}}"#, "body"),
            (
                r#"{"path":"a.txt","lines":{"start":1,"end":1},"body":"b","x":1}"#,
                "`x`",
            ),
            ("not json", "unreadable request"),
        ];
        for (request, expected) in cases {
            let (status, reply) = post(&site, NOTES, request);
            assert_eq!(status, StatusCode::BAD_REQUEST, "{request}: {reply}");
            assert!(reply.contains(expected), "{request}: {reply}");
        }
        assert!(!site.repo.review_file().exists());

        let request = Request::builder()
            .method(Method::DELETE)
            .uri(NOTES)
            .body(Vec::new())
            .unwrap();
        assert_eq!(
            site.respond(&request).status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[test]
    fn a_broken_review_file_is_reported_and_never_overwritten() {
        let repo = TempRepo::new("notes-broken");
        repo.write("a.txt", "x\n");
        let site = repo.site();
        let file = site.repo.review_file();
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let note = r#"{"id":"N1","path":"a.txt","lines":{"start":1,"end":1},"snippet":"x",
            "body":"b","state":"open"}"#;

        let cases = [
            ("{\"version\": 1, \"notes\": [", "cannot parse".to_owned()),
            (r#"{"version": 2, "notes": []}"#, "version 2".to_owned()),
            (r#"{"notes": []}"#, "cannot parse".to_owned()),
            (
                &*format!(r#"{{"version": 1, "notes": [{note}, {note}]}}"#),
                "N1 is used twice".to_owned(),
            ),
        ];
        for (contents, expected) in cases {
            fs::write(&file, contents).unwrap();

            let (status, reply) = get(&site, NOTES);
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{contents}");
            assert!(reply.contains(&expected), "{contents}: {reply}");
            assert!(reply.contains(&*file.to_string_lossy()), "{reply}");
            assert!(reply.contains("move it aside"), "{reply}");

            let request = r#"{"path":"a.txt","lines":{"start":1,"end":1},"body":"b"}"#;
            let (status, _) = post(&site, NOTES, request);
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{contents}");
            assert_eq!(fs::read_to_string(&file).unwrap(), contents);
        }
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

    #[test]
    fn repo_is_found_from_a_subdirectory() {
        let repo = TempRepo::new("subdir");
        repo.write("src/a.rs", "fn a() {}\n");

        let site = Site {
            repo: Repo::open(&repo.root.join("src")).unwrap(),
            highlighter: Highlighter::new(),
            bench: Some("src/a.rs".to_owned()),
        };
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
            PageMessage::parse("drag"),
            Some(PageMessage::DragWindow)
        ));
        assert!(matches!(
            PageMessage::parse("bench-painted"),
            Some(PageMessage::BenchPainted)
        ));
        assert!(matches!(
            PageMessage::parse("bench:{\"a\":1}"),
            Some(PageMessage::BenchDone(report)) if report == "{\"a\":1}"
        ));
        assert!(PageMessage::parse("nonsense").is_none());
    }

    #[test]
    fn query_value_decodes_what_encode_uri_component_writes() {
        // encodeURIComponent("src/my file+é.rs")
        let query = "path=src%2Fmy%20file%2B%C3%A9.rs";
        assert_eq!(query_value(query, "path").unwrap(), "src/my file+é.rs");
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
