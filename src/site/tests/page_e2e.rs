//! The page's end-to-end tests (`tests/e2e/`): the page in Playwright's WebKit, talking to
//! real sites over HTTP on localhost, each with the fake agent (`tests/fake-agent.mjs`) in its
//! `flood` scenario. They catch what the page's unit tests cannot: a click lost because the
//! node under the pointer was replaced while a run streams.
//!
//! The app serves the page on the `postil://` scheme and tells it a run changed by calling
//! `window.postil.runChanged()` (`shell.rs`). A browser driven by Playwright can do neither,
//! so this bridge stands in for the window:
//! - `POST /new` opens a new project, a fresh repository holding `src/a.rs`, `src/b.rs` and
//!   `src/c.rs`, and answers `{"base": "/sN/"}`, `N` counting from 0, so that every test
//!   starts from nothing.
//! - `/sN/<path>` is that site's [`Site::respond`] for `postil://localhost/<path>`. Every URL
//!   the page uses is relative, so the page works under the prefix.
//! - `GET /sN/events` is a stream of server-sent events, one per nudge of the site's
//!   [`Notify`]; the tests' init script calls `runChanged` on each. The app coalesces nudges
//!   into at most one pending call, so the page here draws at least as often as in the app.
//!
//! One thread owns the sites and answers every request, one at a time, as
//! `spawn_site_worker` does in the app; a thread per connection reads its request.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::PoisonError;
use std::thread;

use super::*;

/// The open event streams of one site's pages.
type Listeners = Arc<Mutex<Vec<TcpStream>>>;

/// A site the bridge serves. Fields drop in order: the site stops its agent before the
/// repository the agent runs in is removed.
struct Bridged {
    site: Site,
    listeners: Listeners,
    _repo: TempRepo,
}

#[test]
#[ignore = "needs Playwright's WebKit in tests/e2e: see the README, Development"]
fn page_end_to_end() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (requests, received) = mpsc::channel();
    // Blocks in `accept` until the test process ends: nothing waits for it.
    thread::spawn(move || accept(&listener, &requests));
    let serving = thread::spawn(move || serve(received));

    let status = Command::new("node")
        // A test that hangs fails after two minutes instead of holding `cargo test`: a test
        // takes about a second, and each of its waits fails after 10 s.
        .args(["--test", "--test-timeout=120000", "page.test.js"])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e"))
        .env("POSTIL_E2E_URL", format!("http://{address}/"))
        .status()
        .expect("node is on PATH");

    let mut stop = TcpStream::connect(address).unwrap();
    stop.write_all(b"POST /done HTTP/1.1\r\n\r\n").unwrap();
    serving.join().unwrap();
    assert!(
        status.success(),
        "the page's end-to-end tests failed: {status}"
    );
}

/// A request read whole, with the connection to answer it on.
type Asked = (Request<Vec<u8>>, TcpStream);

/// Reads each connection's request on a thread of its own and hands it to [`serve`]. WebKit
/// opens connections ahead of the requests it sends on them, so a loop that read connections
/// in turn would wait on an idle one while the page waits on its reply.
fn accept(listener: &TcpListener, requests: &mpsc::Sender<Asked>) {
    for stream in listener.incoming() {
        let stream = stream.unwrap();
        let requests = requests.clone();
        thread::spawn(move || {
            // A connection closed before it sent a whole request has nothing to answer.
            if let Some(request) = read_request(&stream) {
                // Fails only once `serve` returned, at the end of the test.
                drop(requests.send((request, stream)));
            }
        });
    }
}

/// Answers requests until `POST /done`, then drops the sites, which stops their agents.
fn serve(requests: mpsc::Receiver<Asked>) {
    let mut sites = Vec::new();
    for (request, stream) in requests {
        if request.uri().path() == "/done" {
            break;
        }
        answer(&mut sites, request, stream);
    }
}

fn answer(sites: &mut Vec<Bridged>, mut request: Request<Vec<u8>>, mut stream: TcpStream) {
    let path = request.uri().path();
    if path == "/new" {
        let base = format!("/s{}/", sites.len());
        sites.push(bridged(sites.len()));
        let body = serde_json::json!({ "base": base }).to_string();
        write_reply(
            &mut stream,
            &reply(StatusCode::OK, "application/json", body.into_bytes()),
        );
        return;
    }
    let found = path
        .strip_prefix("/s")
        .and_then(|path| path.split_once('/'))
        .and_then(|(number, rest)| Some((sites.get(number.parse::<usize>().ok()?)?, rest)));
    let Some((bridged, rest)) = found else {
        let missing = reply(StatusCode::NOT_FOUND, "text/plain", &b"no such site"[..]);
        write_reply(&mut stream, &missing);
        return;
    };
    if rest == "events" {
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                    cache-control: no-store\r\n\r\n";
        // A page that stops reading its events without closing them would otherwise block
        // the agent's thread in `Notify`; a timed-out write drops the stream instead.
        if stream.set_write_timeout(Some(WAIT)).is_ok() && stream.write_all(head.as_bytes()).is_ok()
        {
            let mut listeners = bridged
                .listeners
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            listeners.push(stream);
        }
        return;
    }
    let query = request
        .uri()
        .query()
        .map_or(String::new(), |query| format!("?{query}"));
    *request.uri_mut() = format!("postil://localhost/{rest}{query}").parse().unwrap();
    write_reply(&mut stream, &bridged.site.respond(&request));
}

/// A new site on a fresh repository, the `number`th the bridge opened.
fn bridged(number: usize) -> Bridged {
    let repo = TempRepo::new(&format!("e2e-{number}"));
    repo.write("src/a.rs", "one\ntwo\nthree\n");
    // For the file filter to list several matches.
    repo.write("src/b.rs", "bee\n");
    repo.write("src/c.rs", "sea\n");
    let listeners = Listeners::default();
    let notify: Notify = {
        let listeners = Arc::clone(&listeners);
        Arc::new(move || {
            // A list of streams holds no invariant a panic could break.
            let mut listeners = listeners.lock().unwrap_or_else(PoisonError::into_inner);
            // A page that closed drops out on its first failed write.
            listeners.retain_mut(|stream| stream.write_all(b"data: run\n\n").is_ok());
        })
    };
    Bridged {
        site: agent_site(&repo, "flood", notify),
        listeners,
        _repo: repo,
    }
}

/// Reads one HTTP/1.1 request: its line, its headers (only `content-length` is used) and
/// its body. `None` when the connection closed or sent something else.
fn read_request(stream: &TcpStream) -> Option<Request<Vec<u8>>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?, parts.next()?);
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Request::builder()
        .method(method)
        .uri(target)
        .body(body)
        .ok()
}

/// Writes `reply` with its headers and closes the connection after it.
fn write_reply(stream: &mut TcpStream, reply: &Reply) {
    let mut head = format!("HTTP/1.1 {}\r\n", reply.status());
    for (name, value) in reply.headers() {
        let value = value.to_str().expect("the site sends ASCII headers");
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let length = reply.body().len();
    head.push_str(&format!(
        "cache-control: no-store\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n"
    ));
    // A page that closed while it waited is not the test's failure: the tests assert on what
    // the page shows.
    drop(
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(reply.body())),
    );
}
