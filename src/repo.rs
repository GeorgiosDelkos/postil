//! Reading a repository by running the `git` binary.
//!
//! No git library is linked: the app needs a file list, a branch name, snapshots of the
//! working tree and the diffs between them, and `git` already knows the ignore rules.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::error::{Error, Result};

/// A git working tree on disk.
#[derive(Debug)]
pub(crate) struct Repo {
    /// The top-level directory of the working tree.
    root: PathBuf,
    /// This working tree's git directory: `.git`, or `.git/worktrees/<name>` for a linked
    /// worktree, so each worktree keeps its own notes.
    git_dir: PathBuf,
    /// Held while a snapshot writes its index ([`Repo::snapshot`]): a run's snapshot and the
    /// changes view's may come at once.
    snapshotting: Mutex<()>,
    /// How many times a copy of the real index was started, for tests: git
    /// replaces the index file on every write, so nothing on disk tells a copy apart.
    #[cfg(test)]
    copies: AtomicU64,
}

impl Repo {
    /// Finds the working tree that contains `dir`.
    ///
    /// # Errors
    ///
    /// [`Error::NotAFolder`] when `dir` is not a directory, [`Error::NotARepository`] when it
    /// is not inside a working tree ([`Error::TooBroad`] when it is also the home folder or
    /// the root), and the git errors otherwise.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        // Only a path that is missing or not a directory is "not a folder". Any other
        // failure (no permission to look) is left to git, whose message says what it is.
        let not_a_folder = match fs::metadata(dir) {
            Ok(metadata) => !metadata.is_dir(),
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        };
        if not_a_folder {
            return Err(Error::NotAFolder {
                path: dir.to_owned(),
            });
        }
        // One option per call: git ends each path with a newline, which a path can also hold,
        // so two paths in one output could not be told apart.
        let root = match rev_parse_path(dir, "--show-toplevel") {
            Ok(root) => root,
            // `git` runs in the C locale (see `git`), so this message is stable. A folder
            // that has a `.git` of its own is a broken repository (a linked worktree whose
            // git directory is gone, say), not a plain folder: `git init` there would be
            // wrong, so git's own error stands.
            Err(Error::Git { stderr, .. })
                if stderr.contains("not a git repository") && !dir.join(".git").exists() =>
            {
                let path = dir.to_owned();
                // Offering `git init` for these would put every folder below them in one
                // repository, and listing its files would walk the whole disk.
                return Err(if is_home_or_root(dir) {
                    Error::TooBroad { path }
                } else {
                    Error::NotARepository { path }
                });
            }
            Err(error) => return Err(error),
        };
        let git_dir = rev_parse_path(dir, "--absolute-git-dir")?;
        Ok(Self {
            root,
            git_dir,
            snapshotting: Mutex::new(()),
            #[cfg(test)]
            copies: AtomicU64::new(0),
        })
    }

    /// Opens the working tree that contains `dir`, and when there is none, makes `dir` a new
    /// git repository (`git init`) first. So it never makes a repository inside another one,
    /// whatever the caller checked before, and it never makes the home folder or the root
    /// one, which [`Repo::open`] refuses as [`Error::TooBroad`].
    ///
    /// # Errors
    ///
    /// As [`Repo::open`] except [`Error::NotARepository`], and the git error when `git init`
    /// fails.
    pub(crate) fn init(dir: &Path) -> Result<Self> {
        match Self::open(dir) {
            Err(Error::NotARepository { .. }) => {
                git(dir, &["init", "--quiet"])?;
                Self::open(dir)
            }
            opened => opened,
        }
    }

    /// The top-level directory of the working tree.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Where this working tree's notes are stored. Inside the git directory, so the file is
    /// never committed and needs no `.gitignore` entry.
    pub(crate) fn review_file(&self) -> PathBuf {
        self.git_dir.join("postil").join("review.json")
    }

    /// Where this working tree's open folders are kept (`folders.rs`): beside the review file.
    pub(crate) fn folders_file(&self) -> PathBuf {
        self.git_dir.join("postil").join("folders.json")
    }

    /// Where this working tree's run logs are stored (`run.rs`): beside the review file, for
    /// the same reason.
    pub(crate) fn runs_dir(&self) -> PathBuf {
        self.git_dir.join("postil").join("runs")
    }

    /// The directory name of the working tree, for display.
    pub(crate) fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// The checked-out branch, or an empty string when `HEAD` is detached.
    pub(crate) fn branch(&self) -> Result<String> {
        let branch = git(&self.root, &["branch", "--show-current"])?;
        Ok(branch.trim_end().to_owned())
    }

    /// Every tracked file and every untracked file that is not ignored, sorted, relative to
    /// the root. Repositories inside the working tree are left out, a submodule as well as an
    /// untracked one: they are directories, not files to open, and their files are not this
    /// repository's.
    pub(crate) fn files(&self) -> Result<Vec<String>> {
        let tracked_args = ["ls-files", "-z", "--stage"];
        let tracked = git(&self.root, &tracked_args)?;
        let mut files = Vec::new();
        for entry in tracked.split_terminator('\0') {
            // `<mode> <object> <stage>\t<path>`; a submodule is mode 160000, a gitlink.
            let Some((fields, path)) = entry.split_once('\t') else {
                return Err(Error::Git {
                    command: tracked_args.join(" "),
                    // git exited successfully and wrote nothing to stderr; this field then
                    // says what was wrong with its output instead.
                    stderr: format!("(no error output, but an entry without a tab: {entry:?})"),
                });
            };
            if !fields.starts_with("160000 ") {
                files.push(path.to_owned());
            }
        }
        let untracked = git(
            &self.root,
            &["ls-files", "-z", "--others", "--exclude-standard"],
        )?;
        // git lists an untracked repository as its directory, with a trailing slash.
        files.extend(
            untracked
                .split_terminator('\0')
                .filter(|path| !path.ends_with('/'))
                .map(str::to_owned),
        );
        files.sort_unstable();
        // During a merge conflict git lists a conflicted path once per stage.
        files.dedup();
        Ok(files)
    }

    /// Takes a snapshot of the working tree, every tracked file and every untracked file that
    /// is not ignored, as a git tree, and returns its id. Postil takes one before a run and one
    /// after it, and the changes view shows the diff between them (docs/design.md, "The
    /// changes view").
    ///
    /// The tree is written through an index of Postil's own, `<git dir>/postil-snapshot.index`
    /// beside the real one, so the engineer's index, branch and stash are left as they are. It
    /// is kept between snapshots, so git hashes again only the files whose stat data changed
    /// since the last one, untracked files included. It starts from a copy of the real index,
    /// and again whenever the two track different ignored files (one force-added, or one taken
    /// out of git and added to `.gitignore`), which `git add --all` cannot reconcile; each copy
    /// hashes the untracked files in full once. A snapshot index git cannot read is copied
    /// again at once; one that fails `git add` or `write-tree` is removed, so the next snapshot
    /// starts afresh. `git add` runs the repository's clean filters (git-lfs copies a changed
    /// file into `.git/lfs`). The tree and its blobs are objects no ref holds, so `git gc` may
    /// prune them once they are older than its prune period (two weeks by default).
    ///
    /// # Errors
    ///
    /// [`Error::CopyIndex`] when the real index cannot be copied, and the git errors, such as
    /// another window's snapshot holding git's lock on the index.
    pub(crate) fn snapshot(&self) -> Result<String> {
        let _writing = self
            .snapshotting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let index = self.git_dir.join("postil-snapshot.index");
        if !index.exists() || self.ignored_differ(&index) {
            self.copy_index(&index)?;
        }
        let tree = run_git(&self.root, Some(&index), &["add", "--all"])
            .and_then(|_| run_git(&self.root, Some(&index), &["write-tree"]));
        if tree.is_err() {
            // An index git cannot use would fail every later snapshot too.
            drop(fs::remove_file(&index));
        }
        Ok(tree?.trim_end().to_owned())
    }

    /// Copies the real index to `index`, through a file of its own renamed into place: a copy
    /// cut short, or one made at the same moment by another window or another `Repo` of this
    /// working tree, never leaves a torn index where git reads it.
    fn copy_index(&self, index: &Path) -> Result<()> {
        // A unique name only: nothing is ordered by it.
        static COPIES: AtomicU64 = AtomicU64::new(0);
        let real = self.git_dir.join("index");
        let copy = self.git_dir.join(format!(
            "postil-snapshot.{}.{}.copy",
            std::process::id(),
            COPIES.fetch_add(1, Ordering::Relaxed)
        ));
        #[cfg(test)]
        self.copies.fetch_add(1, Ordering::Relaxed);
        let copied = match fs::copy(&real, &copy) {
            // A repository with nothing staged yet has no index: git starts an empty one.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            copied => copied.and_then(|_| fs::rename(&copy, index)),
        };
        copied.map_err(|source| {
            drop(fs::remove_file(&copy));
            Error::CopyIndex {
                from: real,
                to: index.to_owned(),
                source,
            }
        })
    }

    /// Whether the snapshot index `index` must start again from the real one: the files that
    /// are tracked, ignored and still on disk differ between them. `git add --all` never adds
    /// an ignored file (one force-added since the copy), and goes on updating one an index
    /// already has (one taken out of git and ignored). A deleted file is left out: `git add
    /// --all` drops it from ours, while the real index keeps it until the deletion is staged,
    /// so counting it would copy the index again on every snapshot. An index git cannot read
    /// must start again too.
    fn ignored_differ(&self, index: &Path) -> bool {
        let args = [
            "ls-files",
            "-z",
            "--cached",
            "--ignored",
            "--exclude-standard",
        ];
        let listed = |index: Option<&Path>| -> Option<HashSet<String>> {
            let list = run_git(&self.root, index, &args).ok()?;
            // During a merge conflict a path is listed once per stage.
            Some(list.split_terminator('\0').map(str::to_owned).collect())
        };
        match (listed(Some(index)), listed(None)) {
            (Some(ours), Some(real)) => ours
                .symmetric_difference(&real)
                .any(|path| self.root.join(path).symlink_metadata().is_ok()),
            // The real index unreadable: the copy then says why.
            _ => true,
        }
    }

    /// The changes from snapshot `before` to snapshot `after` ([`Repo::snapshot`]), as
    /// `git diff` prints them: a unified diff with three lines of context, renames shown as a
    /// removal and an addition, paths after `a/` and `b/`. The options and settings given
    /// fix the format whatever the engineer's git config says (`ui/patch.js` reads it):
    /// prefixes, quoting only of paths with control characters, quotes or backslashes, blank
    /// context lines kept, and no textconv drivers, whose text would not match the files.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when an id is not a hexadecimal object id (they come from run
    /// logs, which a hand edit can change), and the git errors, such as a snapshot that
    /// `git gc` pruned.
    pub(crate) fn diff(&self, before: &str, after: &str) -> Result<String> {
        for id in [before, after] {
            if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(Error::BadRequest(format!(
                    "snapshot {id:?}: expected a git object id"
                )));
            }
        }
        git(
            &self.root,
            &[
                "-c",
                "core.quotePath=false",
                "-c",
                "diff.suppressBlankEmpty=false",
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--unified=3",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                before,
                after,
            ],
        )
    }

    /// Reads one file as text. `file` is a path relative to the root, as [`Repo::files`]
    /// returns it.
    pub(crate) fn read(&self, file: &str) -> Result<String> {
        // The path comes from the page. Only plain components are allowed, so the path itself
        // cannot climb above the root (`..`) or be absolute. A symlink inside the tree is
        // still followed, wherever it points: the reader sees what an editor would show.
        let inside = Path::new(file)
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
        if !inside || file.is_empty() {
            return Err(Error::OutsideRepository {
                path: file.to_owned(),
            });
        }

        let path = self.root.join(file);
        fs::read_to_string(&path).map_err(|source| Error::Read { path, source })
    }
}

#[cfg(test)]
impl Repo {
    /// How many times [`Repo::snapshot`] started copying the real index.
    pub(crate) fn index_copies(&self) -> u64 {
        self.copies.load(Ordering::Relaxed)
    }
}

/// Whether `dir` is the home folder or the root of the disk, after resolving links.
fn is_home_or_root(dir: &Path) -> bool {
    let Ok(dir) = dir.canonicalize() else {
        return false;
    };
    let home = std::env::var_os("HOME").and_then(|home| Path::new(&home).canonicalize().ok());
    dir.parent().is_none() || home.is_some_and(|home| home == dir)
}

/// Runs `git` in `dir` and returns what it printed.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    run_git(dir, None, args)
}

/// Runs `git rev-parse <option>` in `dir` and returns the one path it prints, byte for byte:
/// a path may hold a newline, or bytes that are not UTF-8.
fn rev_parse_path(dir: &Path, option: &str) -> Result<PathBuf> {
    let mut path = run_git_bytes(dir, None, &["rev-parse", option])?;
    // git ends the path with one newline; any other newline is part of the path.
    if path.pop() != Some(b'\n') || path.is_empty() {
        return Err(Error::Git {
            command: format!("rev-parse {option}"),
            // git exited successfully and wrote nothing to stderr; this field then says what
            // was wrong with its output instead.
            stderr: "(no error output, but no path on stdout)".to_owned(),
        });
    }
    Ok(PathBuf::from(OsString::from_vec(path)))
}

/// Runs `git` in `dir`, with `index` as its index file when given, and returns what it
/// printed.
fn run_git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
    // A file name that is not UTF-8 is shown with replacement characters and then fails to
    // open. Accepted: the page and its JSON are UTF-8, so such a name cannot round-trip.
    run_git_bytes(dir, index, args).map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// [`run_git`], returning the bytes git printed.
fn run_git_bytes(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        // Messages in English whatever the user's locale: `Repo::open` matches one of them.
        .env("LC_ALL", "C")
        // The repository is the one `-C` names. These variables, inherited from a terminal
        // that a script or a hook left them set in, would point git somewhere else, `git init`
        // included.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().map_err(Error::SpawnGit)?;

    if !output.status.success() {
        return Err(Error::Git {
            command: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(output.stdout)
}
