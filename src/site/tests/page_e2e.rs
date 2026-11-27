//! The page's end-to-end tests (`tests/e2e/`): the page in Playwright's WebKit, talking to
//! real sites over HTTP on localhost, each with the fake agent (`tests/fake-agent.mjs`) in the
//! scenario its test asks for, `flood` unless it names another. They catch what the page's
//! unit tests cannot: a click lost because the node under the pointer was replaced while a run
//! streams.
//!
//! The app serves the page on the `postil://` scheme and tells it a run changed by calling
//! `window.postil.runChanged()` (`shell.rs`). A browser driven by Playwright can do neither,
//! so this bridge stands in for the window:
//! - `POST /new` opens a new project, a fresh repository holding `src/a.rs`, `src/b.rs` and
//!   `src/c.rs`, and answers `{"base": "/sN/"}`, `N` counting from 0, so that every test
//!   starts from nothing. Its body `{"scenario": "..."}` names the fake agent's scenario
//!   (`openProject` in the tests sends `flood` unless told another); without one it is
//!   refused. An optional `"files": {"path": "text"}` holds the repository's files instead of
//!   the three.
//! - `POST /new-fresh` makes a site the same way, but it opens no project, has no
//!   `settings.json` and finds no agents, so the page starts on the welcome screen with the
//!   startup wizard open. `POST /new-fresh-gemini` does the same with an `agy` program on
//!   the search path, so the wizard finds the built-in Gemini agent installed.
//! - `POST /new-opencode` opens a project as `POST /new` does, with the built-in OpenCode
//!   agent installed (an `opencode` program that runs the fake agent) as the agent and the
//!   reviewer, and the custom Fake agent turned on beside it: the agents' logos show on
//!   OpenCode's runs and are absent on Fake's. Its body may name the fake agent's scenario
//!   as `POST /new` does; without one it is `reviewer-silent`, a reply with no findings.
//! - `POST /new-memory` opens a project with the fake agent in `plain`, and a fake
//!   `hippius-mem` that is installed and not set up until its `quickstart` runs, which takes
//!   half a second, so the page asks again while it runs.
//! - `POST /new-memory-server` opens a project with the fake agent in the scenario its body
//!   names as `POST /new` does, `plain` without one, and a fake `hippius-mem` that is ready and
//!   serves MCP: every recall finds one memory and one guess, and every remember writes
//!   `mem_01SAVED`.
//! - `POST /new-fresh-memory` makes a site as `POST /new-fresh` does, with a fake
//!   `hippius-mem` whose doctor passes when the body is `{"memory": "ready"}` and fails
//!   otherwise; the site has finished its first check when the reply comes.
//! - `/sN/<path>` is that site's [`Site::respond`] for `postil://localhost/<path>`. Every URL
//!   the page uses is relative, so the page works under the prefix.
//! - `GET /sN/events` is a stream of server-sent events, one per nudge of the site's
//!   [`Notify`]; the tests' init script calls `runChanged` on each. The app coalesces nudges
//!   into at most one pending call, so the page here draws at least as often as in the app.
//!
//! One thread owns the sites and answers every request, one at a time, as
//! `spawn_site_worker` does in the app; a thread per connection reads its request.

use std::collections::BTreeMap;
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
    let start = match path {
        "/new" => {
            let asked: serde_json::Value =
                serde_json::from_slice(request.body()).unwrap_or_default();
            // Refused, not defaulted: a misspelled field would otherwise run `flood` and fail
            // the test far from here. A 400 and not a panic, which would end this thread and
            // leave the suite to its timeout.
            let Some(scenario) = asked["scenario"].as_str() else {
                let body = &b"POST /new needs {\"scenario\": \"...\"}"[..];
                write_reply(
                    &mut stream,
                    &reply(StatusCode::BAD_REQUEST, "text/plain", body),
                );
                return;
            };
            // Refused when not a map of path to text, for the reason above.
            let files = match asked.get("files") {
                None => None,
                Some(files) => match serde_json::from_value(files.clone()) {
                    Ok(files) => Some(files),
                    Err(error) => {
                        let body = format!("POST /new: files must map paths to text: {error}");
                        write_reply(
                            &mut stream,
                            &reply(StatusCode::BAD_REQUEST, "text/plain", body.into_bytes()),
                        );
                        return;
                    }
                },
            };
            Some(Start::FakeAgent {
                scenario: scenario.to_owned(),
                files,
            })
        }
        "/new-fresh" => Some(Start::FirstRun),
        "/new-memory" => Some(Start::Memory),
        "/new-memory-server" => {
            let asked: serde_json::Value =
                serde_json::from_slice(request.body()).unwrap_or_default();
            let scenario = asked["scenario"].as_str().unwrap_or("plain");
            Some(Start::MemoryServer(scenario.to_owned()))
        }
        "/new-fresh-memory" => {
            let asked: serde_json::Value =
                serde_json::from_slice(request.body()).unwrap_or_default();
            Some(Start::FirstRunWithMemory {
                ready: asked["memory"] == "ready",
            })
        }
        "/new-fresh-gemini" => Some(Start::FirstRunWithGemini),
        "/new-opencode" => {
            let asked: serde_json::Value =
                serde_json::from_slice(request.body()).unwrap_or_default();
            let scenario = asked["scenario"].as_str().unwrap_or("reviewer-silent");
            Some(Start::OpenCode(scenario.to_owned()))
        }
        _ => None,
    };
    if let Some(start) = start {
        let base = format!("/s{}/", sites.len());
        sites.push(bridged(sites.len(), start));
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

/// What a new bridged site starts from.
#[derive(Debug)]
enum Start {
    /// The repository open as the project, with the fake agent in `scenario`, holding `files`
    /// in place of the three default files when given.
    FakeAgent {
        scenario: String,
        files: Option<BTreeMap<String, String>>,
    },
    /// No project, no `settings.json` and an empty search path, as on a first launch: the
    /// page shows the welcome screen with the startup wizard over it.
    FirstRun,
    /// As [`Start::FirstRun`], with an `agy` program (Gemini's) on the search path, so the wizard
    /// finds one built-in agent installed.
    FirstRunWithGemini,
    /// The repository open as the project, with the built-in OpenCode agent installed, chosen
    /// and reviewing, played by the fake agent in this scenario, and the custom Fake agent
    /// (`plain`) turned on.
    OpenCode(String),
    /// The repository open as the project, with the fake agent in `plain`, and a fake
    /// `hippius-mem` that its set up makes ready.
    Memory,
    /// The repository open as the project, with the fake agent in this scenario, and a fake
    /// `hippius-mem` server whose recalls find one memory and one guess.
    MemoryServer(String),
    /// As [`Start::FirstRun`], with a fake `hippius-mem` that is ready or not set up.
    FirstRunWithMemory { ready: bool },
}

/// A new site, the `number`th the bridge opened, on a new repository holding `src/a.rs`,
/// `src/b.rs` and `src/c.rs`. With [`Start::FirstRun`] the repository supplies only the
/// configuration and home directories.
fn bridged(number: usize, start: Start) -> Bridged {
    let repo = TempRepo::new(&format!("e2e-{number}"));
    if let Start::FakeAgent {
        files: Some(files), ..
    } = &start
    {
        for (path, text) in files {
            repo.write(path, text);
        }
    } else {
        repo.write("src/a.rs", "one\ntwo\nthree\n");
        // For the file filter to list several matches.
        repo.write("src/b.rs", "bee\n");
        repo.write("src/c.rs", "sea\n");
    }
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
    let site = match start {
        Start::FakeAgent { scenario, .. } => agent_site(&repo, &scenario, notify),
        // An empty search path, so agents installed on the developer's or runner's PATH
        // cannot change the wizard's first-run defaults.
        Start::FirstRun => Site::new(
            Some(&repo.config()),
            Some(&repo.home()),
            None,
            no_agents(),
            notify,
        ),
        Start::FirstRunWithGemini => {
            // An empty script: the wizard asks it for its models, and it exits at once, so the
            // model select offers only the agent's default.
            let bin = repo.root.join(".git/test-bin");
            fs::create_dir_all(&bin).unwrap();
            let agy = bin.join("agy");
            fs::write(&agy, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&agy, fs::Permissions::from_mode(0o755)).unwrap();
            let search_path = bin.into_os_string();
            Site::new(
                Some(&repo.config()),
                Some(&repo.home()),
                None,
                Scanner::new(move || search_path.clone()),
                notify,
            )
        }
        Start::OpenCode(scenario) => opencode_site(&repo, &scenario, notify),
        Start::Memory => memory_site(&repo, notify),
        Start::MemoryServer(scenario) => {
            let dir = repo.root.join(".git/memory-bin");
            let script = super::memory::server_script(&dir, "hippius-mem 0.7.1", "");
            let script = format!("#!/bin/sh\n{script}");
            agent_site(&repo, &scenario, notify).with_memory(fake_memory(&repo, &script))
        }
        Start::FirstRunWithMemory { ready } => {
            let doctor = if ready { "exit 0" } else { "exit 1" };
            let memory = fake_memory(&repo, &format!("#!/bin/sh\n{doctor}\n"));
            let site = Site::new(
                Some(&repo.config()),
                Some(&repo.home()),
                None,
                no_agents(),
                notify,
            )
            .with_memory(memory);
            // The page's first read then shows the state, so Skip saves from it.
            site.memory.wait();
            site
        }
    };
    Bridged {
        // A stub quota source: a live one would read the developer's real credentials and
        // call the providers with them from a test.
        site: site.with_quota_poller(QuotaPoller::new(|_, _| {
            QuotaReport::unavailable("e2e stub")
        })),
        listeners,
        _repo: repo,
    }
}

/// The site of [`Start::OpenCode`] on `repo`, its project open, OpenCode played by the fake
/// agent in `scenario`.
fn opencode_site(repo: &TempRepo, scenario: &str, notify: Notify) -> Site {
    repo.commit();
    // OpenCode starts as `opencode acp`; the script drops the `acp` and runs the fake agent.
    let bin = repo.root.join(".git/test-bin");
    fs::create_dir_all(&bin).unwrap();
    let opencode = bin.join("opencode");
    let fake = fake_agent(scenario);
    fs::write(&opencode, format!("#!/bin/sh\nexec {fake}\n")).unwrap();
    fs::set_permissions(&opencode, fs::Permissions::from_mode(0o755)).unwrap();
    // This process's PATH after it, for the script to find `node`.
    let path = std::env::var_os("PATH").expect("PATH is set");
    let search_path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)))
            .expect("no directory on PATH holds a colon");

    let site = Site::new(
        Some(&repo.config()),
        Some(&repo.home()),
        None,
        Scanner::new(move || search_path.clone()),
        notify,
    );
    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();
    let settings = serde_json::json!({
        "customAgents": [{ "name": "Fake", "command": fake_agent("plain") }],
        "enabledAgents": ["OpenCode", "Fake"],
        "agent": { "name": "OpenCode" },
        "reviewers": [{ "name": "OpenCode" }],
    });
    let (status, body) = save(&site, &settings.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
    site
}

/// The site of [`Start::Memory`] on `repo`: hippius-mem is a script in a folder of its own,
/// configured once its `quickstart --no-wire` ran.
fn memory_site(repo: &TempRepo, notify: Notify) -> Site {
    let configured = repo.root.join(".git/memory-bin/configured");
    let script = format!(
        r#"#!/bin/sh
case "$*" in
doctor) [ -f {configured} ] && exit 0; exit 1 ;;
"quickstart --no-wire")
  /bin/sleep 0.5
  /usr/bin/touch {configured}
  echo "Trial vault ready at /tmp/postil-e2e-vault." ;;
*) exit 9 ;;
esac
"#,
        configured = configured.display()
    );
    agent_site(repo, "plain", notify).with_memory(fake_memory(repo, &script))
}

/// A memory whose `hippius-mem` is `script`, alone in a folder of its own in `repo`'s git
/// directory.
fn fake_memory(repo: &TempRepo, script: &str) -> crate::memory::Memory {
    let bin = repo.root.join(".git/memory-bin");
    fs::create_dir_all(&bin).unwrap();
    let program = bin.join(crate::memory::PROGRAM);
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let search_path = bin.into_os_string();
    crate::memory::Memory::new(Scanner::new(move || search_path.clone()))
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
