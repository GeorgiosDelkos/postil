//! Helpers the unit tests of several modules share.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// An empty directory in the temporary directory, removed on drop. A leftover of the same
/// name, from a process with a reused pid, is removed first.
#[derive(Debug)]
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// `name` must differ between tests: they run in parallel in one process.
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("postil-{}-tmp-{name}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).expect("the temporary directory is writable");
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a leftover directory in the temporary directory fails no test.
        drop(fs::remove_dir_all(&self.0));
    }
}

/// Makes a named pipe at `path`, through which a process the test starts tells it something
/// happened: the test's read ends when the process has written and closed the pipe.
pub(crate) fn make_fifo(path: &Path) {
    let made = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo runs");
    assert!(made.success(), "mkfifo {}", path.display());
}

/// Reads the named pipe at `path` until its writer closes it, and returns what it wrote.
///
/// # Panics
///
/// When no writer has written and closed the pipe within `wait`.
pub(crate) fn read_fifo(path: &Path, wait: Duration) -> String {
    let (sender, receiver) = mpsc::channel();
    let fifo = path.to_owned();
    // Opening a pipe blocks until a writer opens it, so the read runs on a thread of its own
    // and the wait is bounded here. A read still blocked after a timeout ends with the test
    // process.
    thread::spawn(move || drop(sender.send(fs::read_to_string(fifo))));
    receiver
        .recv_timeout(wait)
        .unwrap_or_else(|_| panic!("nothing wrote {} in {wait:?}", path.display()))
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// Whether the tests run as root, which file modes do not stop: a test that relies on a mode
/// to make a write fail skips itself then, saying so.
pub(crate) fn skip_as_root() -> bool {
    let id = Command::new("id").arg("-u").output().expect("id runs");
    let root = String::from_utf8_lossy(&id.stdout).trim() == "0";
    if root {
        eprintln!("skipped: running as root, which file modes do not stop");
    }
    root
}
