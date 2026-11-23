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
    /// Another process held the review file's lock for as long as a change waits for it.
    ReviewLocked {
        /// The lock file's path.
        path: PathBuf,
        /// How long the change waited, in seconds.
        seconds: u64,
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
    /// The pane widths could not be saved.
    WriteLayout {
        /// The layout file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// An agent's program is not on the search path.
    AgentNotFound {
        /// The program as the agent's command names it.
        program: String,
    },
    /// An agent's process could not be started, or the threads around it could not.
    StartAgent {
        /// The agent's command.
        command: String,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// An agent did not answer a start-up request in time.
    AgentSilent {
        /// The ACP method it did not answer.
        step: &'static str,
        /// How long the app waited.
        seconds: u64,
    },
    /// An agent's process ended.
    AgentExited {
        /// The end of what it wrote to standard error.
        stderr: String,
    },
    /// An agent answered a request with an error.
    AgentRefused {
        /// The ACP method it refused.
        step: &'static str,
        /// The JSON-RPC error code.
        code: i64,
        /// The agent's message.
        message: String,
    },
    /// An agent speaks another protocol version or replied in a shape the app cannot read;
    /// the text says which.
    AgentProtocol(String),
    /// A run log could not be created or written.
    WriteRun {
        /// The log, or its directory.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The git index could not be copied to the index a snapshot of the working tree is
    /// written through.
    CopyIndex {
        /// The real index.
        from: PathBuf,
        /// Postil's snapshot index.
        to: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The open folders of the file list could not be saved.
    WriteFolders {
        /// `folders.json`.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// A run log could not be read.
    ReadRun {
        /// The log, or its directory.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// A prompt was sent while the agent's previous turn is still running.
    TurnRunning,
    /// A run was asked for while another window runs one on the same working tree.
    RunElsewhere,
    /// A permission request or question was answered that is not open: already answered,
    /// withdrawn by the agent, or cancelled.
    NoSuchRequest,
    /// A run was asked for by an id that names no readable run log.
    NoSuchRun {
        /// The id asked for, `R2`.
        id: String,
        /// Why it cannot be shown: no such log, or what is wrong with it.
        problem: String,
    },
}

// Written by hand instead of with `thiserror` to keep the dependency tree small
// (docs/design.md, "Dependencies").
impl fmt::Display for Error {
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per variant; split, a message would sit apart from its variant's peers"
    )]
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
                "cannot write {} (check that its directory is writable; the change was not saved)",
                path.display()
            ),
            Self::ReviewLocked { path, seconds } => write!(
                f,
                "another process held {} for {seconds} s, so the change was not saved (try \
                 again; `lsof` on that file names the process)",
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
            Self::WriteLayout { path, .. } => write!(
                f,
                "cannot save the pane widths to {} (they apply in this window only)",
                path.display()
            ),
            Self::AgentNotFound { program } => write!(
                f,
                "cannot find the agent's program {program} (install it, or change the agent's \
                 command in Settings)"
            ),
            Self::StartAgent { command, .. } => write!(f, "cannot start the agent ({command})"),
            Self::AgentSilent { step, seconds } => write!(
                f,
                "the agent did not answer {step} within {seconds} s (run it in a terminal: it \
                 may be waiting for you to sign in)"
            ),
            Self::AgentExited { stderr } if stderr.is_empty() => {
                write!(f, "the agent exited without a message")
            }
            Self::AgentExited { stderr } => write!(f, "the agent exited: {stderr}"),
            // ACP's code for "sign in first". Login is not handled in version 1 of the app
            // (docs/design.md, "Agents").
            Self::AgentRefused {
                step,
                code: -32000,
                message,
            } => write!(
                f,
                "the agent needs you to sign in (run it in a terminal and sign in there): \
                 {step} answered {message}"
            ),
            Self::AgentRefused {
                step,
                code,
                message,
            } => write!(f, "the agent refused {step}: {message} ({code})"),
            Self::AgentProtocol(problem) => write!(f, "the agent cannot be used: {problem}"),
            Self::WriteRun { path, .. } => write!(
                f,
                "cannot write the run log {} (check that the directory is writable)",
                path.display()
            ),
            Self::CopyIndex { from, to, .. } => write!(
                f,
                "cannot copy the git index {} to {} for a snapshot of the working tree (check \
                 that both can be read and written)",
                from.display(),
                to.display()
            ),
            Self::WriteFolders { path, .. } => write!(
                f,
                "cannot save the open folders to {} (check that the git directory is writable)",
                path.display()
            ),
            Self::ReadRun { path, .. } => write!(f, "cannot read the run log {}", path.display()),
            Self::TurnRunning => write!(
                f,
                "the agent is still working on the last prompt (wait for it, or cancel it)"
            ),
            Self::RunElsewhere => write!(
                f,
                "another Postil window is running an agent on this working tree (wait for it, \
                 or stop it there)"
            ),
            Self::NoSuchRequest => write!(
                f,
                "that request is no longer open (it was answered, withdrawn or cancelled)"
            ),
            Self::NoSuchRun { id, problem } => write!(f, "cannot show the run {id}: {problem}"),
        }
    }
}

/// The error and its sources on one line, for the page to show.
pub(crate) fn error_chain(error: &Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SpawnGit(source)
            | Self::Read { source, .. }
            | Self::ReadReview { source, .. }
            | Self::WriteReview { source, .. }
            | Self::WriteRecent { source, .. }
            | Self::WriteSettings { source, .. }
            | Self::StartAgent { source, .. }
            | Self::WriteRun { source, .. }
            | Self::CopyIndex { source, .. }
            | Self::WriteFolders { source, .. }
            | Self::WriteLayout { source, .. }
            | Self::ReadRun { source, .. } => Some(source),
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
            | Self::ReviewLocked { .. }
            | Self::InvalidSettings(_)
            | Self::SettingsLocked(_)
            | Self::SettingsChanged
            | Self::AgentNotFound { .. }
            | Self::AgentSilent { .. }
            | Self::AgentExited { .. }
            | Self::AgentRefused { .. }
            | Self::AgentProtocol(_)
            | Self::TurnRunning
            | Self::RunElsewhere
            | Self::NoSuchRequest
            | Self::NoSuchRun { .. } => None,
        }
    }
}
