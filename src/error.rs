//! The crate's error type.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// A specialized `Result` for this crate.
pub(crate) type Result<T> = std::result::Result<T, Error>;

/// How many offered models [`Error::ModelNotOffered`] names before it counts the rest.
/// Arbitrary: enough to see the kind of id the agent now uses, short enough for one line of
/// the run card (agy offered 11 on 2026-11-26).
const MODELS_LISTED: usize = 8;

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
    /// A file is too large to show: over 20 MB, the limit in `repo.rs`.
    FileTooLarge {
        /// The file's path on disk.
        path: PathBuf,
        /// Its size on disk, in bytes; the limit plus one when it cannot be measured.
        bytes: u64,
    },
    /// The syntax highlighter failed on a file.
    Highlight {
        /// The file's path in the repository, as the page asked for it.
        path: String,
        /// What the highlighter reported.
        source: tree_sitter_highlight::Error,
    },
    /// The operating system refused to create the window.
    Window(tao::error::OsError),
    /// The system webview could not be created.
    Webview(wry::Error),
    /// The thread that answers the page's requests could not be started.
    SpawnWorker(io::Error),
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
    /// Another writer (a window, a run, a script) held the review file's lock for as long as a
    /// change waits for it.
    ReviewLocked {
        /// The lock file's path.
        path: PathBuf,
        /// How long the change waited, in seconds.
        seconds: u64,
    },
    /// The analysis file exists but could not be read.
    ReadAnalysis {
        /// The analysis file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The analysis file is not JSON of the expected shape.
    ParseAnalysis {
        /// The analysis file's path.
        path: PathBuf,
        /// What the JSON parser reported, with the line and column.
        source: serde_json::Error,
    },
    /// The analysis file parses but breaks a rule of the format.
    InvalidAnalysis {
        /// The analysis file's path.
        path: PathBuf,
        /// Which rule, and where.
        problem: String,
    },
    /// The analysis file could not be written.
    WriteAnalysis {
        /// The analysis file's path, or its lock file's.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The analysis file could not be moved to the Trash (Start over).
    TrashAnalysis {
        /// The analysis file's path.
        path: PathBuf,
        /// Foundation's reason, as an I/O error.
        source: io::Error,
    },
    /// Another writer held the analysis file's lock for as long as a change waits for it.
    AnalysisLocked {
        /// The lock file's path.
        path: PathBuf,
        /// How long the change waited, in seconds.
        seconds: u64,
    },
    /// An analysis run was refused before a thread was made: the analyzer is not installed,
    /// or an analysis of this window is under way. The text says which.
    AnalysisRefused(String),
    /// A diagram was asked for by an id the analysis does not hold.
    NoSuchDiagram {
        /// The id asked for, `D3`.
        id: String,
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
    /// The settings choose a model the agent does not offer (any more): an agent update can
    /// drop a model id, and running another model instead would go unnoticed.
    ModelNotOffered {
        /// The model the settings choose.
        model: String,
        /// The models the agent offers, in its order.
        offered: Vec<String>,
    },
    /// The settings choose a model for an agent that offers no model choice.
    ModelNotChoosable {
        /// The model the settings choose.
        model: String,
    },
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
    /// The usage ledger exists but could not be read.
    ReadUsage {
        /// The ledger file.
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
    /// A run, or a thread's end, was asked for while another window holds a lock it takes:
    /// it runs an agent in the thread, ends or forgets the thread, or starts a run.
    RunElsewhere,
    /// A run was asked for while the most runs a repository may have at once are live.
    TooManyRuns {
        /// The threads whose runs are live, in any window.
        live: Vec<String>,
    },
    /// A thread's worktree was to be made while the repository has the most worktrees it may
    /// have, and no idle thread's could be freed.
    TooManyWorktrees {
        /// The most worktrees a repository may have.
        cap: usize,
        /// The threads whose worktrees could not be freed, each with why: `R1 (at work in
        /// another run)`.
        kept: Vec<String>,
    },
    /// A run was to start while the worktrees' volume has less free space than the floor,
    /// after every idle thread's worktree that could be freed was.
    DiskFull {
        /// The bytes free.
        free: u64,
        /// The bytes Postil keeps free.
        floor: u64,
        /// As in [`Error::TooManyWorktrees`].
        kept: Vec<String>,
    },
    /// A pull request was asked for that cannot be opened as the thread stands; the text says
    /// why and what to do.
    PullRequestRefused(String),
    /// `gh` is not on the login shell's PATH.
    NoGh,
    /// A `gh` command failed.
    GitHub {
        /// `gh pr create`.
        command: String,
        /// What gh said.
        message: String,
    },
    /// Another project was asked to be opened while the open one has a run or a review under
    /// way, without asking to stop it.
    RunInOpenProject,
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
    /// Notes were sent in a repository with no commit: a thread starts from one.
    NoCommit,
    /// A thread's worktree, or the folder above it, could not be made or moved, or git's
    /// record of it could not be removed.
    WriteWorktree {
        /// The folder.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// A thread's worktree would sit under a folder whose path holds a colon, which
    /// `GIT_CEILING_DIRECTORIES` cannot name, so git there could search above it.
    ColonInPath {
        /// The worktree's path.
        path: PathBuf,
    },
    /// The record of threads, `threads.json`, cannot be read or parsed.
    ReadThreads {
        /// The file's path.
        path: PathBuf,
        /// What is wrong with it.
        problem: String,
    },
    /// The record of threads, or its lock, cannot be written.
    WriteThreads {
        /// The file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// A thread's worktree holds changes Postil did not commit, or a commit it did not make,
    /// so ending the thread could lose them.
    ThreadChanged {
        /// The worktree's folder.
        path: PathBuf,
        /// What is there: the changed files, or the unexpected commit.
        found: String,
    },
    /// A thread's record names a folder outside Postil's worktrees folder for the repository,
    /// or one whose `.git` names no worktree record of it, so Postil does not delete it.
    NotAWorktree {
        /// The folder.
        path: PathBuf,
    },
    /// A thread's changes do not fit the engineer's working tree, so none were applied.
    ApplyConflict {
        /// What `git apply` said, naming the files.
        stderr: String,
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
            Self::FileTooLarge { path, bytes } => write!(
                f,
                "{} is {} MB, too large to show (the limit is {} MB)",
                path.display(),
                bytes.div_ceil(1_000_000),
                crate::repo::MAX_READ / 1_000_000
            ),
            Self::Highlight { path, .. } => write!(f, "cannot highlight {path}"),
            Self::Window(_) => write!(f, "cannot create the window"),
            Self::Webview(_) => write!(f, "cannot create the webview"),
            Self::SpawnWorker(_) => write!(f, "cannot start the thread that answers the page"),
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
                "another writer held {} for {seconds} s, so the change was not saved (try \
                 again; `lsof` on that file names the process)",
                path.display()
            ),
            // As for the review file: a file that cannot be read is never written over, so the
            // diagrams in it are safe until the engineer fixes or moves it.
            Self::ReadAnalysis { path, .. } => write!(
                f,
                "cannot read {} (fix its permissions or encoding, or move it aside to start over)",
                path.display()
            ),
            Self::ParseAnalysis { path, .. } => write!(
                f,
                "cannot parse {} (fix it, or move it aside to start over)",
                path.display()
            ),
            Self::InvalidAnalysis { path, problem } => write!(
                f,
                "{}: {problem} (fix it, or move it aside to start over)",
                path.display()
            ),
            Self::TrashAnalysis { path, .. } => write!(
                f,
                "cannot move {} to the Trash (move it aside by hand to start over)",
                path.display()
            ),
            Self::WriteAnalysis { path, .. } => write!(
                f,
                "cannot write {} (check that its directory is writable; the analysis was not \
                 saved)",
                path.display()
            ),
            Self::AnalysisLocked { path, seconds } => write!(
                f,
                "another writer held {} for {seconds} s, so the analysis was not saved (try \
                 again; `lsof` on that file names the process)",
                path.display()
            ),
            Self::AnalysisRefused(problem) => write!(f, "cannot analyze: {problem}"),
            Self::NoSuchDiagram { id } => write!(f, "the analysis has no diagram {id}"),
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
            Self::ModelNotOffered { model, offered } => {
                write!(
                    f,
                    "the agent does not offer the model {model:?}; choose another for it in \
                     Settings (offered: "
                )?;
                if offered.is_empty() {
                    write!(f, "none")?;
                }
                for (index, value) in offered.iter().take(MODELS_LISTED).enumerate() {
                    let comma = if index == 0 { "" } else { ", " };
                    write!(f, "{comma}{value}")?;
                }
                if offered.len() > MODELS_LISTED {
                    write!(f, ", and {} more", offered.len() - MODELS_LISTED)?;
                }
                write!(f, ")")
            }
            Self::ModelNotChoosable { model } => write!(
                f,
                "the agent does not let Postil choose its model; set it to Agent default in \
                 Settings (configured: {model:?})"
            ),
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
            Self::ReadUsage { path, .. } => {
                write!(f, "cannot read the usage ledger {}", path.display())
            }
            Self::ReadRun { path, .. } => write!(f, "cannot read the run log {}", path.display()),
            Self::TurnRunning => write!(
                f,
                "the agent is still working on the last prompt (wait for it, or cancel it)"
            ),
            Self::RunElsewhere => write!(
                f,
                "another Postil window is running an agent in this thread, or Postil is \
                 starting, ending or freeing it, or opening its pull request (wait for it, or \
                 stop it there)"
            ),
            Self::TooManyWorktrees { cap, kept } => write!(
                f,
                "this repository has {cap} worktrees, the most Postil keeps, and none could be \
                 freed{} (end a thread: Apply or Discard)",
                held_by(kept)
            ),
            Self::DiskFull { free, floor, kept } => write!(
                f,
                "{} GB is free on the worktrees' disk, under the {} GB Postil keeps free, after \
                 freeing the idle threads' worktrees{} (end a thread, or free space on the disk)",
                free / 1_000_000_000,
                floor / 1_000_000_000,
                held_by(kept)
            ),
            Self::TooManyRuns { live } => write!(
                f,
                "{} agents already work in this repository, in the threads of {} (wait for \
                 one to end, or stop one)",
                live.len(),
                live.join(", ")
            ),
            Self::PullRequestRefused(why) => write!(f, "no pull request was opened: {why}"),
            Self::NoGh => write!(
                f,
                "GitHub's gh command is not installed (brew install gh, then gh auth login)"
            ),
            Self::GitHub { command, message } => write!(f, "{command} failed: {message}"),
            Self::RunInOpenProject => write!(
                f,
                "a run is under way in the open project (wait for it, stop it, or ask again \
                 with \"stop\": true to stop it and open this one)"
            ),
            Self::NoSuchRequest => write!(
                f,
                "that request is no longer open (it was answered, withdrawn or cancelled)"
            ),
            Self::NoSuchRun { id, problem } => write!(f, "cannot show the run {id}: {problem}"),
            Self::NoCommit => write!(
                f,
                "the repository has no commit yet, and an agent works on a branch that starts \
                 from one (commit once, then send)"
            ),
            Self::WriteWorktree { path, .. } => write!(
                f,
                "cannot make, move or remove {} (check that the folder above it is writable)",
                path.display()
            ),
            Self::ColonInPath { path } => write!(
                f,
                "cannot put an agent's worktree at {}: git cannot be kept from searching above \
                 a folder whose path holds a colon (rename the folder or volume that has it)",
                path.display()
            ),
            Self::ReadThreads { path, problem } => write!(
                f,
                "cannot read the threads in {}: {problem} (fix the file, or move it aside: \
                 Postil then forgets its threads, and their worktrees stay on disk)",
                path.display()
            ),
            Self::WriteThreads { path, .. } => write!(
                f,
                "cannot write {} (check that the git directory is writable)",
                path.display()
            ),
            Self::ThreadChanged { path, found } => write!(
                f,
                "the worktree {} holds work Postil did not commit: {found} (commit it there to \
                 keep it with the thread, or remove it, then try again)",
                path.display()
            ),
            Self::NotAWorktree { path } => write!(
                f,
                "{} is not a worktree of this repository, so Postil leaves it alone (fix the \
                 thread's path in threads.json, or delete the folder yourself)",
                path.display()
            ),
            Self::ApplyConflict { stderr } => write!(
                f,
                "the thread's changes do not fit your working tree, so none were applied: {} \
                 (commit or stash your changes to those files, then apply again)",
                stderr.trim()
            ),
        }
    }
}

/// `"; kept: R1 (at work in another run); R4 (...)"` for the threads `kept`, each with why
/// its worktree could not be freed, or nothing when there are none.
fn held_by(kept: &[String]) -> String {
    if kept.is_empty() {
        String::new()
    } else {
        format!("; kept: {}", kept.join("; "))
    }
}

/// The error and its sources on one line, each after a colon, as the page and the `postil`
/// command show it.
pub fn error_chain(error: &Error) -> String {
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
            | Self::SpawnWorker(source)
            | Self::Read { source, .. }
            | Self::ReadReview { source, .. }
            | Self::WriteReview { source, .. }
            | Self::ReadAnalysis { source, .. }
            | Self::WriteAnalysis { source, .. }
            | Self::TrashAnalysis { source, .. }
            | Self::WriteRecent { source, .. }
            | Self::WriteSettings { source, .. }
            | Self::StartAgent { source, .. }
            | Self::WriteRun { source, .. }
            | Self::CopyIndex { source, .. }
            | Self::WriteFolders { source, .. }
            | Self::WriteLayout { source, .. }
            | Self::ReadUsage { source, .. }
            | Self::ReadRun { source, .. }
            | Self::WriteWorktree { source, .. }
            | Self::WriteThreads { source, .. } => Some(source),
            Self::ParseReview { source, .. } | Self::ParseAnalysis { source, .. } => Some(source),
            Self::Highlight { source, .. } => Some(source),
            Self::Window(source) => Some(source),
            Self::Webview(source) => Some(source),
            Self::Git { .. }
            | Self::OutsideRepository { .. }
            | Self::FileTooLarge { .. }
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
            | Self::InvalidAnalysis { .. }
            | Self::AnalysisLocked { .. }
            | Self::AnalysisRefused(_)
            | Self::NoSuchDiagram { .. }
            | Self::InvalidSettings(_)
            | Self::SettingsLocked(_)
            | Self::SettingsChanged
            | Self::AgentNotFound { .. }
            | Self::AgentSilent { .. }
            | Self::AgentExited { .. }
            | Self::AgentRefused { .. }
            | Self::AgentProtocol(_)
            | Self::ModelNotOffered { .. }
            | Self::ModelNotChoosable { .. }
            | Self::TurnRunning
            | Self::RunElsewhere
            | Self::TooManyRuns { .. }
            | Self::TooManyWorktrees { .. }
            | Self::DiskFull { .. }
            | Self::PullRequestRefused(_)
            | Self::NoGh
            | Self::GitHub { .. }
            | Self::RunInOpenProject
            | Self::NoSuchRequest
            | Self::NoSuchRun { .. }
            | Self::NoCommit
            | Self::ColonInPath { .. }
            | Self::ReadThreads { .. }
            | Self::ThreadChanged { .. }
            | Self::NotAWorktree { .. }
            | Self::ApplyConflict { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_not_offered_names_up_to_eight_offered_and_counts_the_rest() {
        let offered = |count: usize| Error::ModelNotOffered {
            model: "old".to_owned(),
            offered: (1..=count).map(|n| format!("m{n}")).collect(),
        };
        let start = "the agent does not offer the model \"old\"; choose another for it in \
                     Settings (offered: ";
        assert_eq!(offered(0).to_string(), format!("{start}none)"));
        assert_eq!(
            offered(8).to_string(),
            format!("{start}m1, m2, m3, m4, m5, m6, m7, m8)"),
            "exactly the limit: all named, none counted"
        );
        assert_eq!(
            offered(9).to_string(),
            format!("{start}m1, m2, m3, m4, m5, m6, m7, m8, and 1 more)")
        );
    }
}
