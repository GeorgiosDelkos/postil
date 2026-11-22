//! The crate's error type.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// A specialized `Result` for this crate.
pub(crate) type Result<T> = std::result::Result<T, Error>;

/// Why opening the repository or the window failed, why a file could not be shown, or why
/// the notes could not be read or saved.
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
    /// The page sent a request this process cannot read; the text says why.
    BadRequest(String),
    /// No note has the id the page named; another window or a hand edit removed it.
    NoSuchNote {
        /// The id as the page sent it.
        id: String,
    },
    /// The page asked for something that needs an open project, and none is open.
    NoProject,
    /// The path given to open is not a directory.
    NotAFolder {
        /// The path as given.
        path: PathBuf,
    },
    /// The directory given to open is not inside a git working tree.
    NotARepository {
        /// The directory as given.
        path: PathBuf,
    },
    /// The directory given to open is the home folder or the root and not in a repository:
    /// too broad to make one.
    TooBroad {
        /// The directory as given.
        path: PathBuf,
    },
    /// The list of recent projects could not be written.
    WriteRecent {
        /// The list's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The home directory, which holds the app's configuration, is not known.
    NoHome,
    /// The page asked for a note that cannot be made; the text says why.
    InvalidNote(String),
    /// The lines the page showed for a new note are no longer where it showed them, and are
    /// not at exactly one other place in the file.
    NoteTextChanged {
        /// The file the note was written on, relative to the repository root.
        path: String,
    },
    /// The review file exists but could not be read.
    ReadReview {
        /// The review file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The review file is not JSON of the expected shape.
    ParseReview {
        /// The review file's path.
        path: PathBuf,
        /// What the JSON parser reported, with the line and column.
        source: serde_json::Error,
    },
    /// The review file parses but breaks a rule of the format.
    InvalidReview {
        /// The review file's path.
        path: PathBuf,
        /// Which rule, and where.
        problem: String,
    },
    /// The review file could not be written.
    WriteReview {
        /// The review file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The page sent settings that break a rule (`settings.rs`); the text says which.
    InvalidSettings(String),
    /// The settings file on disk cannot be used, so saving over it is refused until it is
    /// reset.
    /// The text says why, naming the file.
    SettingsLocked(String),
    /// The settings file changed after the page read it (another window, or a hand edit).
    SettingsChanged,
    /// The settings file could not be written.
    WriteSettings {
        /// The settings file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
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
            Self::BadRequest(problem) => write!(f, "{problem}"),
            Self::NoSuchNote { id } => write!(f, "there is no note {id}"),
            Self::NoProject => write!(f, "no project is open (open a folder first)"),
            Self::NotAFolder { path } => write!(f, "{} is not a folder", path.display()),
            Self::NotARepository { path } => {
                write!(f, "{} is not in a git repository", path.display())
            }
            Self::TooBroad { path } => write!(
                f,
                "{} is not in a git repository (open a project folder inside it)",
                path.display()
            ),
            Self::WriteRecent { path, .. } => {
                write!(f, "cannot update the recent projects in {}", path.display())
            }
            Self::NoHome => write!(f, "cannot find the home directory (is HOME set?)"),
            Self::InvalidNote(problem) => write!(f, "cannot save the note: {problem}"),
            Self::NoteTextChanged { path } => write!(
                f,
                "cannot save the note: the selected lines of {path} changed on disk since they \
                 were shown (copy the note, press Escape, and select the lines again)"
            ),
            Self::ReadReview { path, .. } => write!(
                f,
                "cannot read {} (fix its permissions or encoding, or move it aside to start \
                 with no notes)",
                path.display()
            ),
            // The app never rewrites a file it cannot read, so the notes in it are safe until
            // the engineer fixes or moves it.
            Self::ParseReview { path, .. } => write!(
                f,
                "cannot parse {} (fix it, or move it aside to start with no notes)",
                path.display()
            ),
            Self::InvalidReview { path, problem } => write!(
                f,
                "{}: {problem} (fix it, or move it aside to start with no notes)",
                path.display()
            ),
            Self::WriteReview { path, .. } => write!(
                f,
                "cannot write {} (check that its directory is writable; the note was not saved)",
                path.display()
            ),
            Self::InvalidSettings(problem) => write!(f, "cannot save the settings: {problem}"),
            Self::SettingsLocked(problem) => write!(
                f,
                "cannot save the settings: {problem} (fix the file, or reset the settings)"
            ),
            Self::SettingsChanged => write!(
                f,
                "cannot save the settings: the file changed since this window read it (the \
                 dialog now shows the file; make the change again)"
            ),
            Self::WriteSettings { path, .. } => write!(
                f,
                "cannot write {} (check that its directory is writable; the settings were not \
                 saved)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SpawnGit(source)
            | Self::Read { source, .. }
            | Self::ReadReview { source, .. }
            | Self::WriteReview { source, .. }
            | Self::WriteRecent { source, .. }
            | Self::WriteSettings { source, .. } => Some(source),
            Self::ParseReview { source, .. } => Some(source),
            Self::Highlight(source) => Some(source),
            Self::Window(source) => Some(source),
            Self::Webview(source) => Some(source),
            Self::Git { .. }
            | Self::OutsideRepository { .. }
            | Self::BenchInterrupted
            | Self::BadRequest(_)
            | Self::NoSuchNote { .. }
            | Self::NoProject
            | Self::NotAFolder { .. }
            | Self::NotARepository { .. }
            | Self::TooBroad { .. }
            | Self::NoHome
            | Self::InvalidNote(_)
            | Self::NoteTextChanged { .. }
            | Self::InvalidReview { .. }
            | Self::InvalidSettings(_)
            | Self::SettingsLocked(_)
            | Self::SettingsChanged => None,
        }
    }
}
