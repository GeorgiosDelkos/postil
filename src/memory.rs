//! hippius-mem, the team's shared memory (docs/review-assist.md, "Milestone 9: team memory"):
//! whether its program is installed and set up, setting up a trial vault, and the memories
//! the page shows, which its MCP server answers ("The memory server" below).
//!
//! Postil never bundles hippius-mem. It looks for the program on the agents' search path
//! ([`Scanner`]) and asks `hippius-mem doctor` whether it is set up: exit 0 is
//! [`State::Ready`], any other exit is [`State::Unconfigured`] with doctor's message, and no
//! program is [`State::Missing`] (spike S3, 2026-11-26, hippius-mem 0.7.1). hippius-mem exits 1
//! for every error, so the code does not say why doctor failed; its message does, and the
//! page offers Set up only when it says there is no configuration (`ui/settings.js`,
//! `canSetUp`). Set up runs `hippius-mem quickstart --no-wire`, which writes hippius-mem's
//! configuration and a local trial vault, and with `--no-wire` changes no other tool's
//! configuration.
//!
//! doctor picks its profile by the git origin of its working directory, so it runs in the
//! open project's root when one is open (the agents run in worktrees of that repository,
//! which share its origin), and in the temporary directory otherwise. Opening another project
//! checks again ([`Memory::open_project`]).
//!
//! Both run on threads of their own: the thread that answers the page serves one request at
//! a time, and a `doctor` that hangs, or a first `quickstart` that downloads its model, would
//! freeze every view. The page asks again every 250 ms while a job runs
//! (`ui/settings-dialog.js`).
//!
//! # Invariants
//!
//! Every job (a check or a set up) gets the next number when it starts. Only the latest job
//! may store the state it found, so a check that a newer check or a set up replaced cannot
//! overwrite the newer result, whichever finishes last. At most one set up runs: a second one
//! is refused while the first runs, and so is a check, whose `doctor` would race the set up's
//! writes. So the set up's own result is always stored, and its job is always the latest. A
//! set up may start while an older check runs: that check's result is then dropped.
//!
//! # The memory server
//!
//! The page's recalls, a memory's body and history, and Refresh are calls to one hippius-mem
//! MCP server per window (`client.rs`), started on the first call, in the open project's root
//! for the reason doctor runs there. Every call runs on the thread named `memory`, which alone
//! owns the server; the routes only queue a task and answer with what is known: the answer,
//! or "pending", and the page asks again every 250 ms (owner decision, 2026-11-26: no new
//! nudge). The queue holds [`QUEUE_CAP`] tasks at most, each once; a full queue drops the
//! oldest recall for a file that is no longer open, then the oldest other recall or read.
//! Answers are kept for the session, a failure only until it is read once, so asking again
//! runs the call again. Refresh, a new check, a set up, a Rescan and another project drop them.
//!
//! - The first call on a server that just started has [`FIRST_CALL_TIMEOUT`], its start
//!   included, and every later one [`CALL_TIMEOUT`]. A call that runs out of its limit is
//!   reported, and its server is stopped: a server that hangs must not hold the memory thread,
//!   and with it every later call. (A reply that came late would do no harm: replies are matched
//!   by id, `client.rs`.)
//! - A server that exits is started again once, at once, and the call runs again; a second
//!   exit within [`RESTART_WINDOW`] is reported, and no server starts again until a new check
//!   (Rescan, or Check in Settings). A server stopped for a timeout, another project, or the
//!   window closing did not exit on its own and counts for nothing.
//! - A server whose `tools/list` lacks a tool Postil calls, or that cannot start for want of
//!   the program or its configuration, is not asked again until a new check either.
//!
//! Every recall passes the repository explicitly, named as hippius-mem names it
//! ([`Repo::memory_name`]): a server in a checkout that `hippius-mem init` did not provision
//! binds none (spike S3).
//!
//! Lock order: the state's lock is never held while a call waits, and nothing else is locked
//! under it. Opening another project and closing the window kill the server's group under it,
//! by the pid the worker clears under it before it reaps the process, so neither signals a pid
//! that was reused.

mod client;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fmt;
use std::hash::Hash;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::agents::{self, Scanner};
use crate::error::{Error, Result};
use crate::memory::client::{CallError, Server, StartError};
pub(crate) use crate::memory::client::{Detail, Recall, Refreshed};
use crate::repo::Repo;

/// The program's name on the search path.
pub(crate) const PROGRAM: &str = "hippius-mem";

/// How long `doctor` may take. Arbitrary: it answered in under 30 ms against a trial vault on
/// 2026-11-26 (hippius-mem 0.7.1); one that takes 10 s is waiting for something that will not come.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `quickstart` may take. Arbitrary: its first run downloads a 129 MB embedding model
/// (bge-small-en-v1.5, spike S3), which takes a minute on a slow link; `doctor`'s limit would
/// kill every first set up.
const SETUP_TIMEOUT: Duration = Duration::from_secs(180);

/// How long a call to a memory server that answered before may take. Arbitrary: a recall in a
/// trial vault answered in under 100 ms on 2026-11-26 (hippius-mem 0.7.1).
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the first call on a server that just started may take, the start included.
/// Arbitrary, chosen to cover the warmup as [`SETUP_TIMEOUT`] covers the model's download:
/// hippius-mem answers `initialize` before its warmup (the embedding model's load and the
/// op-log's sync, `hippius-mem/src/server.rs` at db9261b), and the first `recall` waits for it.
/// A Hippius vault's cold warmup was measured near 20 s, so [`CALL_TIMEOUT`] would end every
/// one. The page shows the recall pending meanwhile.
const FIRST_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How long `hippius-mem --version` may take. Arbitrary: it prints one line.
const VERSION_TIMEOUT: Duration = Duration::from_secs(2);

/// A second exit within this long of the first stops restarting the server (module docs).
/// From the plan's edge case: "a second death in a minute".
const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// The most tasks queued for the memory thread. Arbitrary: far above the files an engineer
/// opens in the seconds a slow recall takes, and small enough that the oldest is still wanted.
const QUEUE_CAP: usize = 32;

/// The longest recall text sent, in bytes. Arbitrary: a query that long is no sharper, and it
/// keeps a request far below a pipe's 16 KiB buffer (`client.rs`, module docs).
const RECALL_TEXT_LIMIT: usize = 4096;

/// The words doctor's and the server's error has when there is no configuration (spike S3).
const NO_CONFIGURATION: &str = "failed to load configuration";

/// The most of each output stream kept, in bytes. Arbitrary: far above the few lines
/// `doctor` and `quickstart` print; the rest is read and dropped.
const MAX_OUTPUT: u64 = 64 * 1024;

/// Where hippius-mem stands on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum State {
    /// Not on the search path.
    Missing,
    /// Installed at `path`, and `doctor` did not pass.
    Unconfigured {
        path: PathBuf,
        /// Why it did not pass: doctor's own message (its standard error, as it printed it),
        /// or that it did not answer within its limit or could not start. `None` when it
        /// exited 1 and printed nothing.
        problem: Option<String>,
    },
    /// Installed at `path`, and `doctor` passed.
    Ready { path: PathBuf },
}

impl State {
    fn path(&self) -> Option<&Path> {
        match self {
            Self::Missing => None,
            Self::Unconfigured { path, .. } | Self::Ready { path } => Some(path),
        }
    }
}

/// The last set up, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum Setup {
    Running,
    /// `quickstart` exited 0; `output` is what it printed, which names the paths it wrote.
    Succeeded {
        output: String,
    },
    /// `quickstart` refused or failed; `error` is its own message, as it printed it, or why
    /// it did not run.
    Failed {
        error: String,
    },
}

/// What is known: the state (`None` while a check runs, or before the first check), and the
/// last set up (`None` before the first).
#[derive(Debug)]
pub(crate) struct Status {
    pub(crate) state: Option<State>,
    pub(crate) setup: Option<Setup>,
}

/// Checks and sets up hippius-mem in the background (module docs, "Invariants"). One per
/// window; cloning shares the state, so each job's thread holds a clone.
#[derive(Clone)]
pub(crate) struct Memory {
    /// Where the program is looked for; a job waits for its scan.
    agents: Scanner,
    shared: Arc<Shared>,
    /// How long `doctor` may take: always [`DOCTOR_TIMEOUT`] outside tests.
    doctor_timeout: Duration,
    /// How long a call may take: always [`CALL_TIMEOUT`] outside tests.
    call_timeout: Duration,
    /// How long the first call on a new server may take: always [`FIRST_CALL_TIMEOUT`] outside
    /// tests.
    first_call_timeout: Duration,
    /// Always [`RESTART_WINDOW`] outside tests.
    restart_window: Duration,
    /// The home the program runs in, in tests of the real program: the engineer's own
    /// configuration and vault are never touched.
    #[cfg(test)]
    isolation: Option<Isolation>,
}

struct Shared {
    inner: Mutex<Inner>,
    /// Signalled whenever a job or a task stores its result.
    changed: Condvar,
    /// Wakes the memory thread: a task was queued, the project changed, or the window closed.
    work: Condvar,
}

/// Locked only to read a field, replace one whole, or change one with a single std call (a
/// push, an insert, a remove, as `calls` is changed in place); never held while a job or a
/// call runs.
#[derive(Debug, Default)]
struct Inner {
    /// The number of the latest job, 0 before the first.
    job: u64,
    state: Option<State>,
    setup: Option<Setup>,
    /// The open project, where the program runs (module docs); `None` with no project.
    project: Option<Project>,
    /// The memory server's tasks and answers (module docs, "The memory server").
    calls: Calls,
    /// How many jobs have ended, stored or dropped, so a test can wait for a dropped one.
    #[cfg(test)]
    finished: u64,
}

/// The two kinds of job.
#[derive(Debug, Clone, Copy)]
enum Job {
    Check,
    Setup,
}

impl Memory {
    /// A memory whose program is looked for on `agents`' path. No job runs yet.
    pub(crate) fn new(agents: Scanner) -> Self {
        Self {
            agents,
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner::default()),
                changed: Condvar::new(),
                work: Condvar::new(),
            }),
            doctor_timeout: DOCTOR_TIMEOUT,
            call_timeout: CALL_TIMEOUT,
            first_call_timeout: FIRST_CALL_TIMEOUT,
            restart_window: RESTART_WINDOW,
            #[cfg(test)]
            isolation: None,
        }
    }

    /// What is known now. Never waits for a job. The first call starts the first check, so a
    /// launch that never asks runs no `doctor`. Looks at the disk once when the state names
    /// the program: a program removed since its check is [`State::Missing`] at once.
    pub(crate) fn status(&self) -> Status {
        let mut inner = self.lock();
        if inner.job == 0 {
            // A thread that cannot start leaves the job number at 0, so the next call tries
            // again; meanwhile the state is unknown, which the page shows as a check.
            drop(self.start(&mut inner, Job::Check));
        }
        let removed = inner
            .state
            .as_ref()
            .and_then(State::path)
            .is_some_and(|path| !agents::is_executable(path));
        if removed {
            inner.state = Some(State::Missing);
        }
        Status {
            state: inner.state.clone(),
            setup: inner.setup.clone(),
        }
    }

    /// Starts a new check: finds the program and runs `doctor`. The state is unknown until it
    /// ends. Does not wait for it.
    ///
    /// # Errors
    ///
    /// - [`Error::MemorySetupRunning`] while a set up runs: its own check follows it.
    /// - [`Error::StartMemory`] when the thread cannot start.
    pub(crate) fn check(&self) -> Result<()> {
        let mut inner = self.lock();
        if inner.setup == Some(Setup::Running) {
            return Err(Error::MemorySetupRunning);
        }
        self.start(&mut inner, Job::Check)
    }

    /// Starts a set up: runs `quickstart --no-wire`, then `doctor`. The state shown before
    /// stays until both end. Does not wait for them.
    ///
    /// # Errors
    ///
    /// - [`Error::MemorySetupRunning`] while another set up runs.
    /// - [`Error::StartMemory`] when the thread cannot start.
    pub(crate) fn setup(&self) -> Result<()> {
        let mut inner = self.lock();
        if inner.setup == Some(Setup::Running) {
            return Err(Error::MemorySetupRunning);
        }
        self.start(&mut inner, Job::Setup)
    }

    /// Runs doctor and the memory server in `repo`'s root from now on (module docs), and checks
    /// again there unless nothing was checked yet (the first check runs there) or a set up
    /// runs (its own check follows it). The last project's server is stopped and its answers
    /// dropped. Runs `git` once ([`Repo::memory_name`]); does not wait for the check.
    pub(crate) fn open_project(&self, repo: &Repo) {
        let project = Project {
            root: repo.root().to_owned(),
            repo: repo.memory_name(),
        };
        let mut inner = self.lock();
        inner.project = Some(project);
        inner.calls.switch();
        self.shared.work.notify_all();
        if inner.job == 0 || inner.setup == Some(Setup::Running) {
            return;
        }
        // Fails only when the process is out of threads. The last project's state then stays
        // shown, and Check or Rescan, which report the error, try again.
        drop(self.start(&mut inner, Job::Check));
    }

    /// Starts `job` as the latest one, with the state already locked so two first calls start
    /// one check. On failure, `inner` is left as it was. The thread starts before `inner`
    /// changes, which is safe: it takes the lock only once its job ran, after this returns.
    fn start(&self, inner: &mut Inner, job: Job) -> Result<()> {
        let number = inner.job + 1;
        let memory = self.clone();
        let dir = inner
            .project
            .as_ref()
            .map_or_else(std::env::temp_dir, |project| project.root.clone());
        let name = match job {
            Job::Check => "memory-check",
            Job::Setup => "memory-setup",
        };
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || memory.run(job, number, &dir))
            .map_err(Error::StartMemory)?;
        inner.job = number;
        match job {
            Job::Check => inner.state = None,
            Job::Setup => inner.setup = Some(Setup::Running),
        }
        Ok(())
    }

    /// The job's thread: runs it in `dir` and stores what it found.
    fn run(&self, job: Job, number: u64, dir: &Path) {
        // Blocks until the login shell has said where programs are, a few seconds at most
        // and only once per app.
        let search_path = self.agents.wait();
        let program = find(&search_path);
        let at = Place {
            search_path: &search_path,
            dir,
        };
        let setup = match (job, &program) {
            (Job::Check, _) => None,
            (Job::Setup, None) => Some(Setup::Failed {
                error: format!("{PROGRAM} is not installed on PATH"),
            }),
            (Job::Setup, Some(path)) => Some(self.quickstart(path, &at)),
        };
        let state = match program {
            None => State::Missing,
            Some(path) => self.doctor(path, &at),
        };

        let mut inner = self.lock();
        // A set up is always the latest job when it ends (module docs, "Invariants"); its
        // `Running` is cleared only here.
        if let Some(setup) = setup {
            inner.setup = Some(setup);
        }
        if inner.job == number {
            inner.state = Some(state);
            // The state the engineer sees is new, so the memory server's is too: what it found
            // unusable is asked again, and a start or call begun before this check can neither
            // store what it finds nor mark the server unusable (module docs, "The memory
            // server"). Here, at the end, and not when the check starts: a server start that
            // fails during the check would otherwise latch its finding over the new state.
            inner.calls.reset();
            self.shared.work.notify_all();
        }
        #[cfg(test)]
        {
            inner.finished += 1;
        }
        self.shared.changed.notify_all();
    }

    /// The state `doctor` reports for the program at `path`.
    fn doctor(&self, path: PathBuf, at: &Place<'_>) -> State {
        let command = self.command(&path, at, &["doctor"]);
        let problem = match run(command, self.doctor_timeout) {
            Outcome::Exited { code: Some(0), .. } => return State::Ready { path },
            // It exits 1 for every error, so only its message says why (module docs).
            Outcome::Exited { stderr, .. } if !stderr.is_empty() => Some(stderr),
            Outcome::Exited { code: Some(1), .. } => None,
            Outcome::Exited { code, .. } => {
                let code = code.map_or_else(|| "a signal".to_owned(), |code| format!("{code}"));
                Some(format!("{PROGRAM} doctor exited with {code}"))
            }
            Outcome::TimedOut => Some(format!(
                "{PROGRAM} doctor did not answer within {} s",
                self.doctor_timeout.as_secs()
            )),
            // Removed between the search and the start.
            Outcome::NotStarted(error) if error.kind() == io::ErrorKind::NotFound => {
                return State::Missing;
            }
            Outcome::NotStarted(error) => Some(format!("cannot start {}: {error}", path.display())),
        };
        State::Unconfigured { path, problem }
    }

    /// Runs `quickstart --no-wire` with the program at `path`.
    fn quickstart(&self, path: &Path, at: &Place<'_>) -> Setup {
        let command = self.command(path, at, &["quickstart", "--no-wire"]);
        match run(command, SETUP_TIMEOUT) {
            Outcome::Exited {
                code: Some(0),
                stdout,
                ..
            } => Setup::Succeeded { output: stdout },
            // Its message, such as "a config already exists", goes to standard error.
            Outcome::Exited { stdout, stderr, .. } => Setup::Failed {
                error: if stderr.is_empty() { stdout } else { stderr },
            },
            Outcome::TimedOut => Setup::Failed {
                error: format!(
                    "{PROGRAM} quickstart did not finish within {} s",
                    SETUP_TIMEOUT.as_secs()
                ),
            },
            Outcome::NotStarted(error) => Setup::Failed {
                error: format!("cannot start {}: {error}", path.display()),
            },
        }
    }

    /// The program at `path` with `args`, on the search path, in the job's directory (module
    /// docs), without colours. hippius-mem reads a `hippius-mem.toml` in that directory before
    /// the global configuration, and picks its profile by the directory's git origin, so the
    /// open project's own file and origin decide what doctor checks, as they will for the
    /// agents' server (docs/review-assist.md, open question 6). Never Postil's own working
    /// directory, which is `/` from the Finder and any folder from a terminal: it is no
    /// project's.
    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            reason = "tests run the program in a home of their own"
        )
    )]
    fn command(&self, path: &Path, at: &Place<'_>, args: &[&str]) -> Command {
        let mut command = Command::new(path);
        command
            .args(args)
            .current_dir(at.dir)
            .env("PATH", at.search_path)
            .env("NO_COLOR", "1");
        #[cfg(test)]
        if let Some(isolation) = &self.isolation {
            isolation.apply(&mut command);
        }
        command
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Poisoning is ignored: no code panics with a field half-changed under this lock. Each
        // field is replaced whole or changed by one std call (a push, an insert, a remove), and
        // an allocation failure aborts rather than panics.
        self.shared
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for Memory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Memory")
            .field("inner", &*self.lock())
            .finish_non_exhaustive()
    }
}

/// The memory server's side of [`Memory`] (module docs, "The memory server").
impl Memory {
    /// The recall for the open file `path`, and the label of the node selected in it when
    /// there is one, as far as it is known. Marks `path` as the file open now, so a full queue
    /// keeps its recall. Never waits for the call.
    ///
    /// # Errors
    ///
    /// [`Error::NoProject`] with no project open; [`Error::StartMemory`] when the memory
    /// thread cannot start.
    pub(crate) fn recall_file(&self, path: &str, node: Option<&str>) -> Result<Reply<Recall>> {
        let text = match node {
            Some(label) => format!("{path} {label}"),
            None => path.to_owned(),
        };
        let query = Query {
            text: cut(text),
            file: Some(path.to_owned()),
        };
        self.lock().calls.open_file = Some(path.to_owned());
        self.recall(query)
    }

    /// The recall for `text`, a run's prompt and changed paths, cut at [`RECALL_TEXT_LIMIT`].
    ///
    /// # Errors
    ///
    /// As [`Memory::recall_file`].
    pub(crate) fn recall_run(&self, text: String) -> Result<Reply<Recall>> {
        self.recall(Query {
            text: cut(text),
            file: None,
        })
    }

    fn recall(&self, query: Query) -> Result<Reply<Recall>> {
        let text = query.text.clone();
        self.ask(Task::Recall(query), |calls| {
            settled(&mut calls.recalls, &text)
        })
    }

    /// Memory `id` whole, its history with it, as far as it is known.
    ///
    /// # Errors
    ///
    /// As [`Memory::recall_file`].
    pub(crate) fn detail(&self, id: MemoryId) -> Result<Reply<Detail>> {
        let key = id.clone();
        self.ask(Task::Detail(id), |calls| settled(&mut calls.details, &key))
    }

    /// Pulls the team's latest notes into the server's index: the first call queues it, and
    /// each call after answers "pending" until it ended, then once with how it ended. The
    /// answers kept are dropped when it succeeds.
    ///
    /// # Errors
    ///
    /// As [`Memory::recall_file`].
    pub(crate) fn refresh(&self) -> Result<Reply<Refreshed>> {
        self.ask(Task::Refresh, |calls| calls.refreshed.take())
    }

    /// Stops the memory thread and its server: the window closes. Does not wait for either.
    pub(crate) fn close(&self) {
        let mut inner = self.lock();
        inner.calls.closed = true;
        inner.calls.kill_server();
        self.shared.work.notify_all();
    }

    /// The answer `answered` finds, else what stops the call, else `task` queued and
    /// "pending". Starts the memory thread on the first call.
    fn ask<T>(
        &self,
        task: Task,
        answered: impl FnOnce(&mut Calls) -> Option<Answer<T>>,
    ) -> Result<Reply<T>> {
        let mut inner = self.lock();
        if let Some(answer) = answered(&mut inner.calls) {
            return Ok(match answer {
                Ok(found) => Reply::Ready(found),
                Err(error) => Reply::Failed { error },
            });
        }
        if let Some(reply) = inner.calls.health.reply() {
            return Ok(reply);
        }
        if inner.project.is_none() {
            return Err(Error::NoProject);
        }
        if !inner.calls.worker {
            let memory = self.clone();
            std::thread::Builder::new()
                .name("memory".to_owned())
                .spawn(move || memory.serve())
                .map_err(Error::StartMemory)?;
            inner.calls.worker = true;
        }
        inner.calls.enqueue(task);
        self.shared.work.notify_all();
        Ok(Reply::Pending)
    }

    /// The memory thread: runs the queued tasks one at a time on the server it alone owns,
    /// until the window closes.
    fn serve(&self) {
        let mut server: Option<Running> = None;
        loop {
            match self.next_task(server.as_ref().map(|running| running.epoch)) {
                Wake::Closed => return self.stop(server.take()),
                Wake::Moved => self.stop(server.take()),
                Wake::Task(next) => self.run_task(&mut server, &next),
            }
        }
    }

    /// Waits for what the memory thread does next, and marks a task it takes as running.
    /// `server` is the epoch of the server it runs, if any.
    fn next_task(&self, server: Option<u64>) -> Wake {
        let mut inner = self.lock();
        loop {
            let Inner { calls, project, .. } = &mut *inner;
            if calls.closed {
                return Wake::Closed;
            }
            if server.is_some_and(|epoch| epoch != calls.epoch) {
                return Wake::Moved;
            }
            if let Some(task) = calls.queue.pop_front() {
                calls.running = Some(task.clone());
                return Wake::Task(Next {
                    task,
                    epoch: calls.epoch,
                    project: project
                        .clone()
                        .expect("a task is queued only with a project open, which stays open"),
                });
            }
            inner = self
                .shared
                .work
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn run_task(&self, server: &mut Option<Running>, next: &Next) {
        let repo = next.project.repo.as_deref();
        match &next.task {
            Task::Recall(query) => {
                let arguments = RecallArguments {
                    text: &query.text,
                    repo,
                };
                let done = self.perform(server, next, |server, deadline| {
                    server.call_tool("recall", &arguments, deadline)
                });
                self.store(next, done, |calls, answer| {
                    calls.recalls.insert(query.text.clone(), answer);
                });
            }
            Task::Detail(id) => {
                let arguments = serde_json::json!({ "id": id.0 });
                let done = self.perform(server, next, |server, deadline| {
                    let note = server.call_tool("get", &arguments, deadline)?;
                    let history = server.call_tool("history", &arguments, deadline)?;
                    Ok(Detail::new(note, history))
                });
                self.store(next, done, |calls, answer| {
                    calls.details.insert(id.clone(), answer);
                });
            }
            Task::Refresh => {
                let done = self.perform(server, next, |server, deadline| {
                    server.call_tool("refresh", &serde_json::json!({}), deadline)
                });
                self.store(next, done, |calls, answer| {
                    if answer.is_ok() {
                        calls.recalls.clear();
                        calls.details.clear();
                    }
                    calls.refreshed = Some(answer);
                });
            }
        }
    }

    /// Runs `call` on the server, starting one first when none runs, and restarting it once
    /// when it exits (module docs).
    fn perform<T>(
        &self,
        server: &mut Option<Running>,
        next: &Next,
        mut call: impl FnMut(&mut Server, Instant) -> std::result::Result<T, CallError>,
    ) -> Done<T> {
        let mut restarted = false;
        loop {
            let stderr = match self.attempt(server, next, &mut call) {
                Ok(found) => return Done::Found(found),
                Err(Failure::Done(done)) => return done,
                Err(Failure::TimedOut(limit)) => {
                    self.stop(server.take());
                    return Done::Failed(format!(
                        "{PROGRAM} did not answer within {}",
                        duration_words(limit)
                    ));
                }
                Err(Failure::Exited(stderr)) => stderr,
            };
            self.stop(server.take());
            match self.died(next.epoch, &stderr) {
                Death::Restart if !restarted => restarted = true,
                Death::Restart => return Done::Failed(exited(&stderr)),
                Death::Halted(error) => return Done::Failed(error),
                Death::Stopped => return Done::Unusable,
            }
        }
    }

    /// Runs `call` once, on the server, starting one first when none runs. One deadline covers
    /// the start and the call: [`FIRST_CALL_TIMEOUT`] on a new server, else [`CALL_TIMEOUT`].
    fn attempt<T>(
        &self,
        server: &mut Option<Running>,
        next: &Next,
        call: &mut impl FnMut(&mut Server, Instant) -> std::result::Result<T, CallError>,
    ) -> std::result::Result<T, Failure<T>> {
        let limit = match server {
            Some(_) => self.call_timeout,
            None => self.first_call_timeout,
        };
        let deadline = Instant::now() + limit;
        let failure = |error| match error {
            CallError::Exited { stderr } => Failure::Exited(stderr),
            CallError::TimedOut => Failure::TimedOut(limit),
            CallError::Refused(message) | CallError::Unreadable(message) => {
                Failure::Done(Done::Failed(message))
            }
        };
        if server.is_none() {
            let running = self.spawn_server(next)?;
            let running = server.insert(running);
            if let Err(error) = self.handshake(running, next, deadline) {
                self.stop(server.take());
                return Err(error.map_or(Failure::Done(Done::Unusable), failure));
            }
        }
        let running = server.as_mut().expect("a server was started above");
        call(&mut running.server, deadline).map_err(failure)
    }

    /// Starts a server for `next`'s project and publishes its pid at once (module docs, "Lock
    /// order"), so closing the window or opening another project stops it even while its
    /// handshake hangs. A server that cannot start says why in the state's health.
    fn spawn_server<T>(&self, next: &Next) -> std::result::Result<Running, Failure<T>> {
        let search_path = self.agents.wait();
        let Some(path) = find(&search_path) else {
            self.unusable(next, Health::Missing);
            return Err(Failure::Done(Done::Unusable));
        };
        let at = Place {
            search_path: &search_path,
            dir: &next.project.root,
        };
        let server = match Server::spawn(self.command(&path, &at, &[])) {
            Ok(server) => server,
            // Removed between the search and the start.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.unusable(next, Health::Missing);
                return Err(Failure::Done(Done::Unusable));
            }
            Err(error) => {
                let error = format!("cannot start {}: {error}", path.display());
                return Err(Failure::Done(Done::Failed(error)));
            }
        };
        let mut inner = self.lock();
        if inner.calls.closed || inner.calls.epoch != next.epoch {
            drop(inner);
            drop(server);
            return Err(Failure::Done(Done::Unusable));
        }
        inner.calls.server = Some(server.pid());
        Ok(Running {
            server,
            epoch: next.epoch,
            path,
        })
    }

    /// Runs `running`'s MCP handshake by `deadline`. On failure, what the caller makes of the
    /// call's error, or `None` when the health now says why (no configuration, or a server
    /// that lacks a tool); the caller stops the server either way.
    fn handshake(
        &self,
        running: &mut Running,
        next: &Next,
        deadline: Instant,
    ) -> std::result::Result<(), Option<CallError>> {
        match running.server.handshake(deadline) {
            Ok(()) => Ok(()),
            Err(StartError::Failed(CallError::Exited { stderr }))
                if stderr.contains(NO_CONFIGURATION) =>
            {
                self.unusable(next, Health::Unconfigured(stderr));
                Err(None)
            }
            Err(StartError::Failed(error)) => Err(Some(error)),
            Err(StartError::Incompatible { missing, server }) => {
                let search_path = self.agents.wait();
                let at = Place {
                    search_path: &search_path,
                    dir: &next.project.root,
                };
                let version = self.version(&running.path, &at).unwrap_or(server);
                let health = Health::Incompatible {
                    server_version: version,
                    missing,
                };
                self.unusable(next, health);
                Err(None)
            }
        }
    }

    /// The version `hippius-mem --version` prints, without the program's name; `None` when it
    /// prints none.
    fn version(&self, path: &Path, at: &Place<'_>) -> Option<String> {
        let command = self.command(path, at, &["--version"]);
        let Outcome::Exited {
            code: Some(0),
            stdout,
            ..
        } = run(command, VERSION_TIMEOUT)
        else {
            return None;
        };
        let version = stdout.strip_prefix(PROGRAM).unwrap_or(&stdout).trim();
        (!version.is_empty()).then(|| version.to_owned())
    }

    /// Records that the server cannot serve: no task is run until a new check.
    fn unusable(&self, next: &Next, health: Health) {
        let mut inner = self.lock();
        if !inner.calls.closed && inner.calls.epoch == next.epoch {
            inner.calls.health = health;
            // Each would fail the same way; the page's next ask reads the health.
            inner.calls.queue.clear();
        }
    }

    /// What the server's exit means: it was stopped (another project, the window closing), or
    /// it died, once or twice within the window (module docs).
    fn died(&self, epoch: u64, stderr: &str) -> Death {
        let mut inner = self.lock();
        let calls = &mut inner.calls;
        if calls.closed || calls.epoch != epoch {
            return Death::Stopped;
        }
        let now = Instant::now();
        let again = calls
            .last_death
            .is_some_and(|last| now.duration_since(last) < self.restart_window);
        calls.last_death = Some(now);
        if !again {
            return Death::Restart;
        }
        let error = format!(
            "{PROGRAM} exited twice within {}, and is not started again until Rescan in \
             Settings{}",
            duration_words(self.restart_window),
            last_words(stderr)
        );
        calls.health = Health::Halted(error.clone());
        calls.queue.clear();
        Death::Halted(error)
    }

    /// Stores what a task found, unless the project changed while it ran.
    fn store<T>(&self, next: &Next, done: Done<T>, put: impl FnOnce(&mut Calls, Answer<T>)) {
        let mut inner = self.lock();
        let calls = &mut inner.calls;
        calls.running = None;
        if calls.epoch == next.epoch {
            match done {
                Done::Found(found) => put(calls, Ok(found)),
                Done::Failed(error) => put(calls, Err(error)),
                Done::Unusable => {}
            }
        }
        self.shared.changed.notify_all();
    }

    /// Ends `server`: its pid is cleared under the lock before it is reaped (module docs,
    /// "Lock order").
    fn stop(&self, server: Option<Running>) {
        let Some(running) = server else { return };
        self.lock().calls.server = None;
        drop(running);
    }
}

/// A memory's id, checked: `mem_` and one or more digits or capital letters, as hippius-mem
/// writes them (a ULID). Nothing else reaches a call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MemoryId(String);

impl MemoryId {
    /// # Errors
    ///
    /// [`Error::BadRequest`] for anything that is not such an id.
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let valid = raw.strip_prefix("mem_").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
        });
        if !valid {
            return Err(Error::BadRequest(format!(
                "memory id {raw:?}: expected mem_ and digits or capital letters"
            )));
        }
        Ok(Self(raw.to_owned()))
    }
}

/// A route's answer, as the page reads it: `state` names the variant.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum Reply<T> {
    /// Asked; ask again.
    Pending,
    Ready(T),
    /// hippius-mem cannot run here; `message` says why, in its words when it gave some.
    Unavailable {
        reason: UnavailableReason,
        message: String,
    },
    /// This hippius-mem lacks the tools `missing`.
    #[serde(rename_all = "camelCase")]
    Incompatible {
        server_version: String,
        postil_version: &'static str,
        missing: Vec<&'static str>,
    },
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum UnavailableReason {
    Missing,
    Unconfigured,
}

/// What a call found, or why it failed.
type Answer<T> = std::result::Result<T, String>;

/// The open project, as the program runs in it.
#[derive(Debug, Clone)]
struct Project {
    root: PathBuf,
    /// The repository's name in hippius-mem ([`Repo::memory_name`]).
    repo: Option<String>,
}

/// A call the memory thread runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Task {
    Recall(Query),
    Detail(MemoryId),
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Query {
    /// What is recalled for, and the key its answer is kept under.
    text: String,
    /// The file it was asked for while open, for a file's recall.
    file: Option<String>,
}

/// The memory server's tasks and answers, in the state's lock (module docs).
#[derive(Default)]
struct Calls {
    /// Whether the memory thread was started; it runs until `closed`.
    worker: bool,
    closed: bool,
    /// Counts the projects opened. A task's answer is stored only in the epoch it was taken
    /// in, and a server of an older one is stopped.
    epoch: u64,
    /// The file the page shows, the one whose recall a full queue keeps.
    open_file: Option<String>,
    /// At most [`QUEUE_CAP`] tasks, each once, none of them `running`.
    queue: VecDeque<Task>,
    running: Option<Task>,
    recalls: HashMap<String, Answer<Recall>>,
    details: HashMap<MemoryId, Answer<Detail>>,
    /// The last refresh's ending, until read.
    refreshed: Option<Answer<Refreshed>>,
    health: Health,
    /// When the server last exited on its own (module docs).
    last_death: Option<Instant>,
    /// The running server's pid, while the memory thread has not reaped it.
    server: Option<u32>,
}

impl Calls {
    /// Forgets what made the server unusable, and the answers kept: a new check. Starts a new
    /// epoch, so nothing begun before it stores an answer or a health, and its server stops.
    fn reset(&mut self) {
        self.epoch += 1;
        self.health = Health::Usable;
        self.last_death = None;
        self.recalls.clear();
        self.details.clear();
    }

    /// Another project opened: everything of the last one goes, its server first.
    fn switch(&mut self) {
        self.reset();
        self.queue.clear();
        self.open_file = None;
        self.refreshed = None;
        self.kill_server();
    }

    /// Kills the running server's group, which the memory thread then reaps.
    fn kill_server(&self) {
        if let Some(pid) = self.server {
            agents::kill_group(pid);
        }
    }

    /// Queues `task` unless it is queued or running already. A full queue drops one first:
    /// the oldest recall for a file that is not open, else the oldest recall or read.
    fn enqueue(&mut self, task: Task) {
        if self.running.as_ref() == Some(&task) || self.queue.contains(&task) {
            return;
        }
        if self.queue.len() == QUEUE_CAP {
            let open = self.open_file.as_deref();
            let closed_file = self.queue.iter().position(|queued| match queued {
                Task::Recall(query) => query.file.is_some() && query.file.as_deref() != open,
                Task::Detail(_) | Task::Refresh => false,
            });
            let index = closed_file
                .or_else(|| {
                    self.queue
                        .iter()
                        .position(|queued| !matches!(queued, Task::Refresh))
                })
                .expect("a full queue holds one refresh at most, and more than one task");
            self.queue.remove(index);
        }
        self.queue.push_back(task);
    }
}

impl fmt::Debug for Calls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sizes only: the answers are teammates' notes.
        f.debug_struct("Calls")
            .field("worker", &self.worker)
            .field("closed", &self.closed)
            .field("epoch", &self.epoch)
            .field("queued", &self.queue.len())
            .field("running", &self.running.is_some())
            .field("recalls", &self.recalls.len())
            .field("details", &self.details.len())
            .field("health", &self.health)
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

/// Whether the server can be asked.
#[derive(Debug, Clone, Default)]
enum Health {
    #[default]
    Usable,
    Missing,
    /// It found no configuration; its message.
    Unconfigured(String),
    Incompatible {
        server_version: String,
        missing: Vec<&'static str>,
    },
    /// It exited twice within the window; the error the page shows.
    Halted(String),
}

impl Health {
    /// What every call answers while the server cannot be asked; `None` when it can.
    fn reply<T>(&self) -> Option<Reply<T>> {
        Some(match self {
            Self::Usable => return None,
            Self::Missing => Reply::Unavailable {
                reason: UnavailableReason::Missing,
                message: format!("{PROGRAM} is not installed on PATH"),
            },
            Self::Unconfigured(message) => Reply::Unavailable {
                reason: UnavailableReason::Unconfigured,
                message: message.clone(),
            },
            Self::Incompatible {
                server_version,
                missing,
            } => Reply::Incompatible {
                server_version: server_version.clone(),
                postil_version: env!("CARGO_PKG_VERSION"),
                missing: missing.clone(),
            },
            Self::Halted(error) => Reply::Failed {
                error: error.clone(),
            },
        })
    }
}

/// What the memory thread does next.
enum Wake {
    Closed,
    /// Another project opened: its server stops.
    Moved,
    Task(Next),
}

/// A task taken from the queue, with what it runs in.
struct Next {
    task: Task,
    epoch: u64,
    project: Project,
}

/// The memory thread's server, the epoch it was started in, and its program.
#[derive(Debug)]
struct Running {
    server: Server,
    epoch: u64,
    path: PathBuf,
}

/// How a task ended.
enum Done<T> {
    Found(T),
    Failed(String),
    /// Nothing to store: the server cannot be asked (the health says why), or the project
    /// changed.
    Unusable,
}

/// Why an attempt at a call did not find its answer.
enum Failure<T> {
    /// The server's output ended; the end of its standard error.
    Exited(String),
    /// No answer came within this limit.
    TimedOut(Duration),
    Done(Done<T>),
}

/// What a server's exit means (`Memory::died`).
enum Death {
    Restart,
    Halted(String),
    Stopped,
}

#[derive(Serialize)]
struct RecallArguments<'a> {
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<&'a str>,
}

/// The answer kept in `answers` under `key`: a found one stays, a failure is read once.
fn settled<K: Hash + Eq, T: Clone>(
    answers: &mut HashMap<K, Answer<T>>,
    key: &K,
) -> Option<Answer<T>> {
    match answers.get(key)? {
        Ok(found) => Some(Ok(found.clone())),
        Err(_) => answers.remove(key),
    }
}

/// `text` cut at [`RECALL_TEXT_LIMIT`], on a character boundary.
fn cut(mut text: String) -> String {
    text.truncate(text.floor_char_boundary(RECALL_TEXT_LIMIT));
    text
}

fn exited(stderr: &str) -> String {
    format!("{PROGRAM} exited{}", last_words(stderr))
}

/// `stderr`, the end of what the server printed there, after a colon; nothing when empty.
fn last_words(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

/// `duration` as the page says it, in whole seconds: every limit here is.
fn duration_words(duration: Duration) -> String {
    format!("{} s", duration.as_secs())
}

/// Why team memory cannot be turned on in `state`, or `None` when it is ready.
pub(crate) fn not_ready(state: Option<&State>) -> Option<String> {
    match state {
        Some(State::Ready { .. }) => None,
        None => Some(format!("{PROGRAM} is still being checked")),
        Some(State::Missing) => Some(format!("{PROGRAM} is not installed on PATH")),
        Some(State::Unconfigured { problem: None, .. }) => Some(format!(
            "{PROGRAM} is not set up (set up a trial vault in Settings first)"
        )),
        Some(State::Unconfigured {
            problem: Some(problem),
            ..
        }) => Some(problem.clone()),
    }
}

/// Where a job runs the program: the search path it was found on, which is its `PATH`, and its
/// working directory.
struct Place<'a> {
    search_path: &'a OsStr,
    dir: &'a Path,
}

/// Where the program is on `search_path`, if it is there.
fn find(search_path: &OsStr) -> Option<PathBuf> {
    agents::find_program(PROGRAM, search_path)
}

/// How a run of the program ended.
#[derive(Debug)]
enum Outcome {
    /// It closed its output and exited; `code` is `None` when a signal ended it. Each stream
    /// is its text, at most [`MAX_OUTPUT`] bytes of it, without trailing whitespace.
    Exited {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    /// It did not close its output within the limit, and was killed.
    TimedOut,
    NotStarted(io::Error),
}

/// Runs `command` with standard input closed, so it can never wait for an answer, reading its
/// output as it runs, for at most `timeout`.
///
/// It runs in its own process group. Once its output is closed, or at the timeout, the group
/// is killed ([`agents::kill_group`]) before the program is reaped, so its pid, and with it
/// the group id, cannot have been reused, and nothing it left running stays. A program that
/// exited is a zombie by then, which the kill does not change, so its exit code is kept.
fn run(mut command: Command, timeout: Duration) -> Outcome {
    let spawned = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return Outcome::NotStarted(error),
    };

    let deadline = Instant::now() + timeout;
    let stdout = read_on_thread(child.stdout.take());
    let stderr = read_on_thread(child.stderr.take());
    let stdout = receive(&stdout, deadline);
    let stderr = stdout.as_ref().and_then(|_| receive(&stderr, deadline));
    agents::kill_group(child.id());
    // Fails when the group's kill already took it, or it had exited: both fine.
    drop(child.kill());
    let code = child.wait().ok().and_then(|status| status.code());
    match (stdout, stderr) {
        (Some(stdout), Some(stderr)) => Outcome::Exited {
            code,
            stdout,
            stderr,
        },
        _ => Outcome::TimedOut,
    }
}

/// Reads `pipe` to its end on a thread of its own, so a program that fills one stream's pipe
/// while the other is read does not block. The receiver gets the text once.
fn read_on_thread(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    if let Some(mut pipe) = pipe {
        let read = move || {
            let mut bytes = Vec::new();
            // A read error ends the text; what came before it is kept.
            drop((&mut pipe).take(MAX_OUTPUT).read_to_end(&mut bytes));
            // The rest is read and dropped, so the program never blocks on a full pipe.
            drop(io::copy(&mut pipe, &mut io::sink()));
            let text = String::from_utf8_lossy(&bytes).trim_end().to_owned();
            // The receiver is gone once the timeout passed; nobody wants the text then.
            drop(sender.send(text));
        };
        // A thread that cannot start drops the sender with the closure, which `receive`
        // reads as no output.
        drop(
            std::thread::Builder::new()
                .name("memory-output".to_owned())
                .spawn(read),
        );
    }
    receiver
}

/// The text from `receiver`, or `None` when it did not come by `deadline`.
fn receive(receiver: &mpsc::Receiver<String>, deadline: Instant) -> Option<String> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(text) => Some(text),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(String::new()),
    }
}

/// The environment of the real program in tests: a home of its own, with every variable that
/// would point it elsewhere removed.
#[cfg(test)]
#[derive(Debug, Clone)]
struct Isolation {
    home: PathBuf,
    /// The embedding model's cache: the engineer's when it is there, so a run does not
    /// download 129 MB. hippius-mem takes lock files in it, and downloads the model into it
    /// when the model is missing.
    model_cache: Option<PathBuf>,
}

#[cfg(test)]
impl Isolation {
    fn apply(&self, command: &mut Command) {
        command.env("HOME", &self.home);
        // Any HIPPIUS_MEM_* variable overrides the configuration file, and the XDG ones move
        // it, the vault and the cache out of the home.
        for (key, _) in std::env::vars_os() {
            let key_text = key.to_string_lossy();
            if key_text.starts_with("HIPPIUS_MEM_") || key_text.starts_with("XDG_") {
                command.env_remove(&key);
            }
        }
        command.env_remove("FASTEMBED_CACHE_DIR");
        if let Some(cache) = &self.model_cache {
            command.env("FASTEMBED_CACHE_DIR", cache);
        }
    }
}

#[cfg(test)]
impl Memory {
    /// Replaces how long `doctor` may take.
    pub(crate) fn with_doctor_timeout(mut self, timeout: Duration) -> Self {
        self.doctor_timeout = timeout;
        self
    }

    /// Replaces how long a call to the memory server may take.
    pub(crate) fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Replaces how long the first call on a new server may take.
    pub(crate) fn with_first_call_timeout(mut self, timeout: Duration) -> Self {
        self.first_call_timeout = timeout;
        self
    }

    /// Replaces the window within which a second exit halts the server.
    pub(crate) fn with_restart_window(mut self, window: Duration) -> Self {
        self.restart_window = window;
        self
    }

    /// Remembers `note` (the arguments of hippius-mem's `remember`) through a server of its
    /// own in `dir`, which exits before this returns, and gives the memory's id. hippius-mem
    /// 0.7.1 has no command that remembers: a test seeds its vault this way.
    pub(crate) fn remember(&self, dir: &Path, note: &serde_json::Value) -> String {
        #[derive(serde::Deserialize)]
        struct Remembered {
            id: String,
        }
        let search_path = self.agents.wait();
        let path = find(&search_path).expect("the test checked that hippius-mem is installed");
        let at = Place {
            search_path: &search_path,
            dir,
        };
        let deadline = Instant::now() + self.call_timeout;
        let mut server = Server::spawn(self.command(&path, &at, &[])).expect("the server starts");
        server
            .handshake(deadline)
            .expect("the server answers its handshake");
        let remembered: Remembered = server
            .call_tool("remember", note, deadline)
            .expect("hippius-mem remembers the note");
        remembered.id
    }

    /// Waits until the memory thread has run every task queued.
    pub(crate) fn wait_idle(&self) {
        let mut inner = self.lock();
        while !inner.calls.queue.is_empty() || inner.calls.running.is_some() {
            inner = self
                .shared
                .changed
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Runs the program in `home`, reading the embedding model from `model_cache`.
    pub(crate) fn with_home(mut self, home: &Path, model_cache: Option<&Path>) -> Self {
        self.isolation = Some(Isolation {
            home: home.to_owned(),
            model_cache: model_cache.map(Path::to_owned),
        });
        self
    }

    /// Waits until `count` jobs have ended, the dropped ones included.
    pub(crate) fn wait_finished(&self, count: u64) {
        let mut inner = self.lock();
        while inner.finished < count {
            inner = self
                .shared
                .changed
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The state once no job runs, waiting for the latest one. Starts the first check when
    /// none ran.
    pub(crate) fn wait(&self) -> State {
        drop(self.status());
        let mut inner = self.lock();
        loop {
            if let (Some(state), false) = (&inner.state, inner.setup == Some(Setup::Running)) {
                return state.clone();
            }
            inner = self
                .shared
                .changed
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}
