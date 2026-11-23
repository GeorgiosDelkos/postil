//! Reading a repository by running the `git` binary.
//!
//! No git library is linked: the app needs a file list, a branch name, snapshots of the
//! working tree and the diffs between them, and `git` already knows the ignore rules.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

/// A git working tree on disk.
#[derive(Debug)]
pub(crate) struct Repo {
    /// The top-level directory of the working tree.
    root: PathBuf,
    /// This working tree's git directory: `.git`, or `.git/worktrees/<name>` for a linked
    /// worktree, so each worktree keeps its own notes.
    git_dir: PathBuf,
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
        // git prints one line per option, in the order asked.
        let args = ["rev-parse", "--show-toplevel", "--absolute-git-dir"];
        let paths = match git(dir, &args) {
            Ok(paths) => paths,
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
        let mut paths = paths.lines();
        let (Some(root), Some(git_dir)) = (paths.next(), paths.next()) else {
            return Err(Error::Git {
                command: args.join(" "),
                // git exited successfully and wrote nothing to stderr; this field then says
                // what was wrong with its output instead.
                stderr: "(no error output, but fewer than two lines on stdout)".to_owned(),
            });
        };
        Ok(Self {
            root: PathBuf::from(root),
            git_dir: PathBuf::from(git_dir),
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
    /// the root.
    pub(crate) fn files(&self) -> Result<Vec<String>> {
        let listing = git(
            &self.root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        let mut files: Vec<String> = listing.split_terminator('\0').map(str::to_owned).collect();
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
    /// The tree is written through a temporary index, a copy of the real one so git can skip
    /// files whose stat data did not change: the engineer's index, branch and stash are left
    /// as they are. The tree and its blobs are objects no ref holds, so `git gc` may prune
    /// them once they are older than its prune period (two weeks by default).
    ///
    /// # Errors
    ///
    /// [`Error::WriteSnapshot`] when the temporary index cannot be made, and the git errors.
    pub(crate) fn snapshot(&self) -> Result<String> {
        // Each snapshot has an index of its own: two windows, or two runs' snapshots, may
        // overlap. It lives in the system's temporary directory, not the git directory, so a
        // snapshot needs no write access there beyond the objects git adds.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let index = std::env::temp_dir().join(format!(
            "postil-snapshot-{}-{}.index",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let failed = |source| Error::WriteSnapshot {
            path: index.clone(),
            source,
        };
        match fs::copy(self.git_dir.join("index"), &index) {
            Ok(_) => {}
            // A repository with nothing staged yet has no index: git starts an empty one.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(failed(error)),
        }
        let tree = run_git(&self.root, Some(&index), &["add", "--all"])
            .and_then(|_| run_git(&self.root, Some(&index), &["write-tree"]));
        // A leftover index is only a stray file; the snapshot is what matters.
        drop(fs::remove_file(&index));
        Ok(tree?.trim_end().to_owned())
    }

    /// The changes from snapshot `before` to snapshot `after` ([`Repo::snapshot`]), as
    /// `git diff` prints them: a unified diff with three lines of context, renames shown as a
    /// removal and an addition.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when an id is not a hexadecimal object id (they come from run
    /// logs, which a hand edit can change), and the git errors, such as a snapshot that
    /// `git gc` pruned.
    pub(crate) fn diff(&self, before: &str, after: &str) -> Result<String> {
        for id in [before, after] {
            if id.len() < 4 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(Error::BadRequest(format!(
                    "snapshot {id:?}: expected a git object id"
                )));
            }
        }
        git(
            &self.root,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                "--unified=3",
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

/// Runs `git` in `dir`, with `index` as its index file when given, and returns what it
/// printed.
fn run_git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
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

    // A file name that is not UTF-8 is shown with replacement characters and then fails to
    // open. Accepted: the page and its JSON are UTF-8, so such a name cannot round-trip.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
