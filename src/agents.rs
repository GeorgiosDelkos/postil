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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
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

/// How long the login shell may take to print its `PATH`. Arbitrary: a slow shell start-up
/// (nvm, conda) takes about a second; longer means it is waiting for something that will not
/// come, and the settings must not hang on it.
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// The search path, found on a background thread. Asking the login shell takes up to a
/// second (more with a slow start-up file), and the URL scheme handler that serves the
/// settings runs on the main thread, which must not wait for it.
///
/// A scan starts when the scanner is made and on every [`Scanner::rescan`]. While the latest
/// scan runs, [`Scanner::path`] is `None` and the page asks again shortly. An older scan
/// that finishes after a newer one started is dropped, so a rescan always shows its own
/// result.
pub(crate) struct Scanner {
    find: Arc<dyn Fn() -> OsString + Send + Sync>,
    /// The number of the latest scan, and the path it found once it finished. Locked only to
    /// read or replace both; never held while a scan runs.
    state: Arc<Mutex<(u64, Option<OsString>)>>,
}

impl Scanner {
    /// Starts the first scan, which calls `find` (in the app, [`search_path`]).
    pub(crate) fn new(find: impl Fn() -> OsString + Send + Sync + 'static) -> Self {
        let scanner = Self {
            find: Arc::new(find),
            state: Arc::new(Mutex::new((0, None))),
        };
        scanner.rescan();
        scanner
    }

    /// Starts a new scan; the result of any scan still running is dropped.
    pub(crate) fn rescan(&self) {
        let scan = {
            let mut state = lock(&self.state);
            *state = (state.0 + 1, None);
            state.0
        };
        let find = Arc::clone(&self.find);
        let state = Arc::clone(&self.state);
        let finish = move || {
            let path = find();
            let mut state = lock(&state);
            if state.0 == scan {
                state.1 = Some(path);
            }
        };
        let spawned = std::thread::Builder::new()
            .name("agent-scan".to_owned())
            .spawn(finish);
        if spawned.is_err() {
            // Fails only when the process is out of threads or memory. The settings then list
            // every agent as not found rather than wait for a scan that never runs.
            lock(&self.state).1 = Some(OsString::new());
        }
    }

    /// The latest scan's search path, or `None` while it runs.
    pub(crate) fn path(&self) -> Option<OsString> {
        lock(&self.state).1.clone()
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
/// `PATH` arrived, the shell is killed if it still runs. A reader still blocked then (no end
/// marker, and a background job holding the pipe) ends when that job does; accepted, as it
/// holds nothing but the pipe.
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
    // Fails when the shell already exited, which is the usual case and fine.
    drop(child.kill());
    drop(child.wait());
    path
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

    /// A temporary directory removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("postil-agents-{name}-{}", std::process::id()));
            drop(fs::remove_dir_all(&dir));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, text: &str, mode: u32) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.0));
        }
    }

    #[test]
    fn a_program_is_found_only_as_an_executable_file_in_an_absolute_directory() {
        let first = Dir::new("first");
        let second = Dir::new("second");
        first.file("agent", "", 0o644);
        let wanted = second.file("agent", "", 0o755);
        fs::create_dir(first.0.join("tool")).unwrap();
        let path = std::env::join_paths([
            "relative/dir".as_ref(),
            first.0.as_path(),
            second.0.as_path(),
        ])
        .unwrap();

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
        let dir = Dir::new("shell");
        // Stands in for zsh: prints like a noisy .zshrc, then runs the command it was given
        // (`-i -l -c COMMAND`, so the command is the fourth argument) with a known PATH.
        let shell = dir.file(
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
        let dir = Dir::new("loud-shell");
        // 200 kB of noise: three times what the pipe holds on macOS.
        let shell = dir.file(
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
        let dir = Dir::new("daemon-shell");
        // Like a .zshrc that starts a daemon without redirecting its output: the job keeps
        // the pipe open for 30 s after the shell exits.
        let shell = dir.file(
            "daemon",
            concat!(
                "#!/bin/sh\n",
                "sleep 30 &\n",
                "PATH=/opt/daemon/bin exec /bin/sh -c \"$4\"\n",
            ),
            0o755,
        );
        let started = Instant::now();
        assert_eq!(
            login_shell_path(&shell, LOGIN_SHELL_TIMEOUT),
            Some(OsString::from("/opt/daemon/bin"))
        );
        assert!(
            started.elapsed() < LOGIN_SHELL_TIMEOUT,
            "read up to the end marker only"
        );
    }

    #[test]
    fn a_login_shell_that_fails_hangs_or_is_missing_gives_no_path() {
        let dir = Dir::new("bad-shell");
        let failing = dir.file("failing", "#!/bin/sh\nexit 3\n", 0o755);
        let silent = dir.file("silent", "#!/bin/sh\necho no markers here\n", 0o755);
        let hanging = dir.file("hanging", "#!/bin/sh\nexec sleep 30\n", 0o755);

        assert_eq!(login_shell_path(&failing, LOGIN_SHELL_TIMEOUT), None);
        assert_eq!(login_shell_path(&silent, LOGIN_SHELL_TIMEOUT), None);
        assert_eq!(
            login_shell_path(&dir.0.join("absent"), LOGIN_SHELL_TIMEOUT),
            None
        );

        let started = Instant::now();
        assert_eq!(login_shell_path(&hanging, Duration::from_millis(200)), None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a hanging shell is killed at the timeout, not waited for"
        );
    }

    /// Waits for the scanner's current scan, for at most five seconds.
    fn finished(scanner: &Scanner) -> OsString {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(path) = scanner.path() {
                return path;
            }
            assert!(Instant::now() < deadline, "the scan did not finish");
            std::thread::sleep(Duration::from_millis(5));
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
        first_running.recv().unwrap();
        assert_eq!(
            scanner.path(),
            None,
            "the first scan is held, so it is still running"
        );

        scanner.rescan();
        assert_eq!(finished(&scanner), "new");
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
