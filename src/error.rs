//! The crate's error type.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// A specialized `Result` for this crate.
pub(crate) type Result<T> = std::result::Result<T, Error>;

/// Why opening the repository or the window failed, or why a file could not be shown.
#[derive(Debug)]
pub enum Error {
    /// The `git` binary could not be started.
    SpawnGit(io::Error),
    /// A `git` command exited with a failure.
    Git {
        /// The arguments `git` was run with.
        command: String,
        /// What `git` wrote to standard error.
        stderr: String,
    },
    /// The page asked for a path that is not a plain relative path inside the repository.
    OutsideRepository {
        /// The path as the page sent it.
        path: String,
    },
    /// A file in the repository could not be read as UTF-8 text.
    Read {
        /// The file's path on disk.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The syntax highlighter failed on a file.
    Highlight(tree_sitter_highlight::Error),
    /// The operating system refused to create the window.
    Window(tao::error::OsError),
    /// The system webview could not be created.
    Webview(wry::Error),
    /// The window was closed before the benchmark page reported its measurements.
    BenchInterrupted,
}

// Written by hand instead of with `thiserror` to keep the dependency tree small
// (docs/design.md, "Dependencies").
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnGit(_) => write!(f, "cannot run git (is it installed and on PATH?)"),
            Self::Git { command, stderr } => write!(f, "git {command} failed: {}", stderr.trim()),
            Self::OutsideRepository { path } => {
                write!(f, "not a relative path inside the repository: {path}")
            }
            Self::Read { path, .. } => write!(f, "cannot read {} as text", path.display()),
            Self::Highlight(_) => write!(f, "cannot highlight the file"),
            Self::Window(_) => write!(f, "cannot create the window"),
            Self::Webview(_) => write!(f, "cannot create the webview"),
            Self::BenchInterrupted => write!(f, "window closed before the benchmark finished"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SpawnGit(source) | Self::Read { source, .. } => Some(source),
            Self::Highlight(source) => Some(source),
            Self::Window(source) => Some(source),
            Self::Webview(source) => Some(source),
            Self::Git { .. } | Self::OutsideRepository { .. } | Self::BenchInterrupted => None,
        }
    }
}
