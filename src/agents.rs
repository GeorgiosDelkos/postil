//! The coding agents Postil knows, and which of them are installed.
//!
//! An agent is a name and the command that starts it as an ACP server (docs/design.md,
//! "Agents"). Postil ships a catalog of agents with a known ACP command ([`BUILT_IN`]); the
//! engineer adds others in the settings (`settings.rs`, `CustomAgent`).
//!
//! An agent is installed when its program is found on the search path. That path is the
//! login shell's `PATH` followed by this process's: an app started from the Dock or Finder
//! gets launchd's short `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`), which leaves out
//! `~/.local/bin`, Homebrew, nvm and the other places agents are installed, so the login
//! shell is asked for the `PATH` a terminal would have.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{BufRead, BufReader, IsTerminal};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::Duration;

/// An agent in the catalog.
#[derive(Debug)]
pub(crate) struct BuiltIn {
    /// The name shown in the settings and stored in `settings.json`.
    pub(crate) name: &'static str,
    /// The program whose presence means the agent is installed.
    pub(crate) program: &'static str,
    /// The command that starts the agent as an ACP server.
    pub(crate) command: &'static str,
}

/// The catalog. Each command was checked against the tool's own `--help` on 2026-11-22 where
/// the tool was at hand (Gemini `--acp`, Grok `agent stdio`); Claude and Codex speak ACP
/// through the adapters docs/design.md names, and OpenCode through its `acp` command. The
/// ACP milestone confirms each one end to end. Cursor's CLI has no ACP mode, so it is not
/// listed; a custom agent covers it if it gains one.
pub(crate) const BUILT_IN: [BuiltIn; 5] = [
    BuiltIn {
        name: "Claude",
        program: "claude",
        command: "npx -y @agentclientprotocol/claude-agent-acp",
    },
    BuiltIn {
        name: "Codex",
        program: "codex",
        command: "npx -y @zed-industries/codex-acp",
    },
    BuiltIn {
        name: "Gemini",
        program: "gemini",
        command: "gemini --acp",
    },
    BuiltIn {
        name: "Grok",
        program: "grok",
        command: "grok agent stdio",
    },
    BuiltIn {
        name: "OpenCode",
        program: "opencode",
        command: "opencode acp",
    },
];

/// The agent a run goes to: its name in the settings and the command that starts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentChoice {
    pub(crate) name: String,
    pub(crate) command: String,
}

/// How long the login shell may take to print its `PATH`. Arbitrary: a slow shell start-up
/// (nvm, conda) takes about a second; longer means it is waiting for something that will not
/// come, and the settings must not hang on it.
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// The search path, found on a background thread. Asking the login shell takes up to a
/// second (more with a slow start-up file), and the thread that serves the settings answers
/// every page request one at a time, so none should wait behind it.
///
/// The first scan starts when the path is first asked for, so a launch that never opens the
/// settings never runs the engineer's start-up files; another starts on every
/// [`Scanner::rescan`]. While the latest scan runs, [`Scanner::path`] is `None` and the page
/// asks again shortly. An older scan that finishes after a newer one started is dropped, so
/// a rescan always shows its own result.
#[derive(Clone)]
pub(crate) struct Scanner {
    find: Arc<dyn Fn() -> OsString + Send + Sync>,
    /// The number of the latest scan (0 before the first), and the path it found once it
    /// finished. Locked only to read or replace both; never held while a scan runs.
    state: Arc<Mutex<(u64, Option<OsString>)>>,
    /// Signalled whenever a scan stores its path, for [`Scanner::wait`].
    found: Arc<Condvar>,
}

impl Scanner {
    /// A scanner whose scans call `find` (in the app, [`search_path`]). No scan runs yet.
    pub(crate) fn new(find: impl Fn() -> OsString + Send + Sync + 'static) -> Self {
        Self {
            find: Arc::new(find),
            state: Arc::new(Mutex::new((0, None))),
            found: Arc::new(Condvar::new()),
        }
    }

    /// Starts a new scan; the result of any scan still running is dropped.
    pub(crate) fn rescan(&self) {
        self.start(&mut lock(&self.state));
    }

    /// The latest scan's search path, or `None` while it runs. The first call starts the
    /// first scan.
    pub(crate) fn path(&self) -> Option<OsString> {
        let mut state = lock(&self.state);
        if state.0 == 0 {
            self.start(&mut state);
        }
        state.1.clone()
    }

    /// The latest scan's search path, waiting for the scan to finish. Starts the first scan
    /// when none ran. A rescan started while this waits is waited for too.
    pub(crate) fn wait(&self) -> OsString {
        let mut state = lock(&self.state);
        if state.0 == 0 {
            self.start(&mut state);
        }
        loop {
            if let Some(path) = &state.1 {
                return path.clone();
            }
            state = self
                .found
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Starts a scan, with the state already locked so two first calls start one scan. The
    /// scan's thread takes the lock only after `find` returns.
    fn start(&self, state: &mut (u64, Option<OsString>)) {
        *state = (state.0 + 1, None);
        let scan = state.0;
        let find = Arc::clone(&self.find);
        let shared = Arc::clone(&self.state);
        let found = Arc::clone(&self.found);
        let finish = move || {
            let path = find();
            let mut state = lock(&shared);
            if state.0 == scan {
                state.1 = Some(path);
                found.notify_all();
            }
        };
        let spawned = std::thread::Builder::new()
            .name("agent-scan".to_owned())
            .spawn(finish);
        if spawned.is_err() {
            // Fails only when the process is out of threads or memory. The settings then list
            // every agent as not found rather than wait for a scan that never runs. A `wait`
            // blocked on an earlier scan, which this one replaced, is woken to read it.
            state.1 = Some(OsString::new());
            self.found.notify_all();
        }
    }
}

impl fmt::Debug for Scanner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scanner")
            .field("state", &*lock(&self.state))
            .finish_non_exhaustive()
    }
}

/// Locks the scan state. Poisoning is ignored: the state is replaced whole, so a panic in a
/// scan cannot leave it half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The directories to look for agents in: the login shell's `PATH`, then this process's.
/// Either part is left out when it is unknown.
///
/// Started from a terminal (standard input is one), this process's `PATH` already is the
/// login shell's, so the shell is not asked: an interactive shell started under a terminal
/// can take the terminal over, and the engineer's prompt would stop answering.
pub(crate) fn search_path() -> OsString {
    let from_terminal = std::io::stdin().is_terminal();
    let shell = std::env::var_os("SHELL")
        .filter(|_| !from_terminal)
        .map(PathBuf::from);
    let login = shell.and_then(|shell| login_shell_path(&shell, LOGIN_SHELL_TIMEOUT));
    let own = std::env::var_os("PATH");
    let parts = [login, own];
    std::env::join_paths(
        parts
            .iter()
            .flatten()
            .flat_map(|path| std::env::split_paths(path)),
    )
    .expect("split_paths never yields a part holding `:`, the one thing join_paths refuses")
}

/// The markers around the `PATH` in the login shell's output.
const START: &str = "__POSTIL_PATH_START__";
const END: &str = "__POSTIL_PATH_END__";

/// The `PATH` that `shell` sets up for an interactive login, or `None` when the shell cannot
/// be started, prints nothing usable, or does not print it within `timeout`.
///
/// The shell runs `-i -l -c` so both its login and its interactive start-up files run: nvm
/// and most installers add themselves to `.zshrc` or `.bashrc`, which only interactive shells
/// read. Those files may print, so the value is read between two marker lines. The command
/// uses only `echo`, `;` and an absolute `printenv`, which sh, bash, zsh and fish all accept.
///
/// The shell runs in its own process group, so even with a terminal at hand it cannot make
/// itself the terminal's foreground job. Its output is read on a thread of its own while it
/// runs, so output larger than the pipe holds does not block it, and the reading stops at the
/// end marker: a background job that a start-up file left holding the pipe cannot keep it
/// waiting. The timeout covers starting, printing and reading; after it, or after the
/// `PATH` arrived, the shell's process group is killed ([`kill_group`]), which closes the
/// pipe, and so ends a reader still waiting on it, unless a process outside the group holds
/// it too.
fn login_shell_path(shell: &Path, timeout: Duration) -> Option<OsString> {
    let mut child = Command::new(shell)
        .process_group(0)
        .args(["-i", "-l", "-c"])
        .arg(format!("echo {START}; /usr/bin/printenv PATH; echo {END}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let path = child.stdout.take().and_then(|stdout| {
        let (sender, receiver) = mpsc::sync_channel(1);
        let read = move || {
            // The receiver is gone once the timeout passed; nobody wants the value then.
            drop(sender.send(marked_path(BufReader::new(stdout))));
        };
        let reader = std::thread::Builder::new()
            .name("login-shell-path".to_owned())
            .spawn(read);
        reader.ok()?;
        receiver.recv_timeout(timeout).ok().flatten()
    });
    kill_group(child.id());
    // Fails when the group's kill already took the shell, or it had exited: both fine.
    drop(child.kill());
    drop(child.wait());
    path
}

/// Kills every process in the group `leader` leads: the shell and whatever its start-up files
/// left running in it, such as a command that hung (the shell is killed at the timeout, and
/// the command would run on). They belong to this one probe, not to any terminal. A process
/// that left the group (`setsid`, as daemons do) is out of reach. The leader is not reaped
/// until after this call, so its pid, and with it the group id, cannot have been reused.
/// `/bin/kill` with a negative id signals the group; it fails when the group is already
/// gone, which is fine. Run as a command because signalling a group from Rust needs `libc`
/// and `unsafe`.
pub(crate) fn kill_group(leader: u32) {
    let killed = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{leader}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    drop(killed);
}

/// The line between the [`START`] and [`END`] lines of `out`, read only as far as `END`.
fn marked_path(out: impl BufRead) -> Option<OsString> {
    let mut lines = out.lines().map_while(Result::ok);
    lines.find(|line| line == START)?;
    let path = lines.next()?;
    let path = path.trim();
    (lines.next()? == END && !path.is_empty()).then(|| OsString::from(path))
}

/// Where `program` is: itself when it is a path (it contains a `/`), else the first
/// executable file of that name in `search_path`. `None` when there is none.
pub(crate) fn find_program(program: &str, search_path: &OsStr) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    std::env::split_paths(search_path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The program a command starts: its first word. Commands are split on whitespace, without
/// quoting (`settings.rs` refuses quotes), so a program path with a space cannot be named.
pub(crate) fn program_of(command: &str) -> Option<&str> {
    command.split_whitespace().next()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Instant;

    use super::*;
    use crate::testing::TempDir;

    /// Writes `text` to the file `name` in `dir`, with permissions `mode`.
    fn file(dir: &TempDir, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn a_program_is_found_only_as_an_executable_file_in_an_absolute_directory() {
        let first = TempDir::new("agents-first");
        let second = TempDir::new("agents-second");
        file(&first, "agent", "", 0o644);
        let wanted = file(&second, "agent", "", 0o755);
        fs::create_dir(first.path().join("tool")).unwrap();
        let path =
            std::env::join_paths(["relative/dir".as_ref(), first.path(), second.path()]).unwrap();

        assert_eq!(
            find_program("agent", &path),
            Some(wanted.clone()),
            "the first match is not executable, so the second is the one"
        );
        assert_eq!(
            find_program("tool", &path),
            None,
            "a directory is not a program"
        );
        assert_eq!(find_program("missing", &path), None);
        assert_eq!(
            find_program(wanted.to_str().unwrap(), OsStr::new("")),
            Some(wanted.clone()),
            "a path is taken as it is, whatever the search path"
        );
    }

    #[test]
    fn the_login_shell_path_is_read_between_the_markers_whatever_its_start_up_prints() {
        let dir = TempDir::new("agents-shell");
        // Stands in for zsh: prints like a noisy .zshrc, then runs the command it was given
        // (`-i -l -c COMMAND`, so the command is the fourth argument) with a known PATH.
        let shell = file(
            &dir,
            "fake-shell",
            concat!(
                "#!/bin/sh\n",
                "echo 'welcome back'\n",
                "PATH=/opt/agents/bin:/usr/bin exec /bin/sh -c \"$4\"\n",
            ),
            0o755,
        );
        assert_eq!(
            login_shell_path(&shell, LOGIN_SHELL_TIMEOUT),
            Some(OsString::from("/opt/agents/bin:/usr/bin"))
        );
    }

    #[test]
    fn start_up_output_larger_than_the_pipe_does_not_block_the_shell() {
        let dir = TempDir::new("agents-loud-shell");
        // 200 kB of noise: three times what the pipe holds on macOS.
        let shell = file(
            &dir,
            "loud",
            concat!(
                "#!/bin/sh\n",
                "head -c 200000 /dev/zero | tr '\\0' x\n",
                "echo\n",
                "PATH=/opt/loud/bin exec /bin/sh -c \"$4\"\n",
            ),
            0o755,
        );
        assert_eq!(
            login_shell_path(&shell, LOGIN_SHELL_TIMEOUT),
            Some(OsString::from("/opt/loud/bin"))
        );
    }

    #[test]
    fn a_background_job_holding_the_output_open_does_not_hold_up_the_path() {
        let dir = TempDir::new("agents-daemon-shell");
        // Like a .zshrc that starts a daemon without redirecting its output: the job keeps
        // the pipe open for 30 s after the shell exits.
        let shell = file(
            &dir,
            "daemon",
            concat!(
                "#!/bin/sh\n",
                "sleep 30 &\n",
                "PATH=/opt/daemon/bin exec /bin/sh -c \"$4\"\n",
            ),
            0o755,
        );
        // Reading to the end of the output would wait the 30 s and time out with no path.
        assert_eq!(
            login_shell_path(&shell, LOGIN_SHELL_TIMEOUT),
            Some(OsString::from("/opt/daemon/bin"))
        );
    }

    #[test]
    fn only_a_non_empty_line_between_both_markers_is_a_path() {
        let read = |text: &str| marked_path(text.as_bytes());
        let marked = |middle: &str| format!("noise\n{START}\n{middle}\n{END}\n");
        assert_eq!(read(&marked(" /a:/b ")), Some(OsString::from("/a:/b")));
        assert_eq!(read(&marked("  ")), None, "an empty PATH is no PATH");
        assert_eq!(
            read(&format!("{START}\n/a\n")),
            None,
            "cut off before the end"
        );
        assert_eq!(read(&format!("{START}\n/a\nnot the end\n")), None);
        assert_eq!(read("/a\n"), None, "no markers");
    }

    #[test]
    fn a_login_shell_that_fails_hangs_or_is_missing_gives_no_path() {
        let dir = TempDir::new("agents-bad-shell");
        let failing = file(&dir, "failing", "#!/bin/sh\nexit 3\n", 0o755);
        let silent = file(&dir, "silent", "#!/bin/sh\necho no markers here\n", 0o755);
        // Like a start-up file whose command hangs: the shell waits on a child that the
        // timeout must not leave running. The child's pid goes to a file for the check below.
        let pid_file = dir.path().join("hung.pid");
        let script = format!(
            "#!/bin/sh\nsleep 30 &\necho $! > {}\nwait\n",
            pid_file.display()
        );
        let hanging = file(&dir, "hanging", &script, 0o755);

        assert_eq!(login_shell_path(&failing, LOGIN_SHELL_TIMEOUT), None);
        assert_eq!(login_shell_path(&silent, LOGIN_SHELL_TIMEOUT), None);
        assert_eq!(
            login_shell_path(&dir.path().join("absent"), LOGIN_SHELL_TIMEOUT),
            None
        );

        let started = Instant::now();
        assert_eq!(login_shell_path(&hanging, Duration::from_secs(2)), None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a hanging shell is killed at the timeout, not waited for"
        );
        let pid = fs::read_to_string(&pid_file).unwrap();
        let alive = || {
            Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        // Killed with the group; launchd reaps it a moment later.
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive() {
            assert!(Instant::now() < deadline, "the hung command still runs");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_rescan_shows_its_own_result_even_when_an_older_scan_finishes_later() {
        // The first scan to run takes the gate, says so on `entered`, waits for `release`,
        // and finds "old"; any later scan finds "new" at once. The rescan starts only after
        // the first scan took the gate, so the held scan is the older one.
        let (release, gate) = mpsc::channel::<()>();
        let (entered, first_running) = mpsc::channel::<()>();
        let gate = Mutex::new(Some((gate, entered)));
        let scanner = Scanner::new(move || {
            // Taken in its own statement: a guard in the `match` would be held while the
            // first scan waits, and the second scan would wait for the lock.
            let first = lock(&gate).take();
            match first {
                Some((gate, entered)) => {
                    entered.send(()).unwrap();
                    gate.recv().expect("the test releases the first scan");
                    OsString::from("old")
                }
                None => OsString::from("new"),
            }
        });
        assert_eq!(scanner.path(), None, "asking starts the first scan");
        first_running.recv().unwrap();
        assert_eq!(
            scanner.path(),
            None,
            "the first scan is held, so it is still running"
        );

        std::thread::scope(|scope| {
            // Whether it starts waiting before the rescan or after, a waiter gets the newest
            // scan's path: the older scan is held until the end of this scope.
            let waiting = scope.spawn(|| scanner.wait());
            scanner.rescan();
            assert_eq!(waiting.join().unwrap(), "new");
        });
        release.send(()).unwrap();
        // Each scan thread holds a clone of the state until it ends, after its store.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&scanner.state) > 1 {
            assert!(Instant::now() < deadline, "the first scan did not end");
            std::thread::yield_now();
        }
        assert_eq!(
            scanner.path(),
            Some(OsString::from("new")),
            "the older scan's result was dropped"
        );
    }

    #[test]
    fn the_built_in_names_are_unique_whatever_the_case() {
        // Settings name agents by these names (`enabledAgents`, `agent`, `reviewers`), and
        // custom names are refused when they match one regardless of case.
        for (index, agent) in BUILT_IN.iter().enumerate() {
            for other in &BUILT_IN[index + 1..] {
                assert!(
                    !agent.name.eq_ignore_ascii_case(other.name),
                    "{}",
                    agent.name
                );
            }
        }
    }
}
