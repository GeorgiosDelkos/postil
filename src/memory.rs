//! hippius-mem, the team's shared memory (docs/review-assist.md, "Milestone 9: team memory"):
//! whether its program is installed and set up, and setting up a trial vault.
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

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::agents::{self, Scanner};
use crate::error::{Error, Result};

/// The program's name on the search path.
pub(crate) const PROGRAM: &str = "hippius-mem";

/// How long `doctor` may take. Arbitrary: it answered in under 30 ms against a trial vault on
/// 2026-11-26 (hippius-mem 0.7.1); one that takes 10 s is waiting for something that will not come.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `quickstart` may take. Arbitrary: its first run downloads a 129 MB embedding model
/// (bge-small-en-v1.5, spike S3), which takes a minute on a slow link; `doctor`'s limit would
/// kill every first set up.
const SETUP_TIMEOUT: Duration = Duration::from_secs(180);

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
    /// The home the program runs in, in tests of the real program: the engineer's own
    /// configuration and vault are never touched.
    #[cfg(test)]
    isolation: Option<Isolation>,
}

struct Shared {
    inner: Mutex<Inner>,
    /// Signalled whenever a job stores its result.
    changed: Condvar,
}

/// Locked only to read or replace fields whole; never held while a job runs.
#[derive(Debug, Default)]
struct Inner {
    /// The number of the latest job, 0 before the first.
    job: u64,
    state: Option<State>,
    setup: Option<Setup>,
    /// The open project's root, where the program runs (module docs); `None` with no project.
    project: Option<PathBuf>,
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
            }),
            doctor_timeout: DOCTOR_TIMEOUT,
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

    /// Runs doctor in `root` from now on (module docs), and checks again there unless nothing
    /// was checked yet (the first check runs there) or a set up runs (its own check follows
    /// it). Does not wait for the check.
    pub(crate) fn open_project(&self, root: &Path) {
        let mut inner = self.lock();
        inner.project = Some(root.to_owned());
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
        let dir = inner.project.clone().unwrap_or_else(std::env::temp_dir);
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
        // Poisoning is ignored: every field is replaced whole, so a panic cannot leave one
        // half-written.
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
