//! Reading a repository by running the `git` binary.
//!
//! No git library is linked: the app needs a file list, a branch name, snapshots of the
//! working tree and the diffs between them, and `git` already knows the ignore rules.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::error::{Error, Result};

/// The largest file [`Repo::read`] reads, in bytes: 20 MB, the owner's choice (2026-11-23).
/// Larger files are refused rather than shown: the page holds every line of the open file,
/// and a file of hundreds of megabytes is held several times over (the text, its tokens,
/// their JSON) while the request thread works on it.
pub(crate) const MAX_READ: u64 = 20_000_000;

/// The branch a new linked worktree checks out ([`Repo::add_worktree`]).
// `Copy`: a small value of two borrows, passed by value (clippy's `needless_pass_by_value`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Checkout<'a> {
    /// A new branch, started at commit `fork`.
    New { branch: &'a str, fork: &'a str },
    /// A branch that exists, checked out nowhere else.
    Existing { branch: &'a str },
}

/// A local branch's branch on a remote ([`Repo::upstream`]).
#[derive(Debug)]
pub(crate) struct Upstream {
    pub(crate) remote: String,
    /// Its name there: `develop` for a local `dev` that tracks `origin/develop`.
    pub(crate) branch: String,
}

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
        let root = match rev_parse_path(dir, Search::Up, "--show-toplevel") {
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
        let git_dir = rev_parse_path(dir, Search::Up, "--absolute-git-dir")?;
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
        self.postil_dir().join("review.json")
    }

    /// Where this working tree's open folders are kept (`folders.rs`): beside the review file.
    pub(crate) fn folders_file(&self) -> PathBuf {
        self.postil_dir().join("folders.json")
    }

    /// Where this working tree's analysis is kept (`analysis.rs`): beside the review file, for
    /// the same reason.
    pub(crate) fn analysis_file(&self) -> PathBuf {
        self.postil_dir().join("analysis.json")
    }

    /// Where this working tree's run logs are stored (`run/log.rs`): beside the review file, for
    /// the same reason.
    pub(crate) fn runs_dir(&self) -> PathBuf {
        self.postil_dir().join("runs")
    }

    /// The directory that holds every file Postil keeps for this working tree.
    fn postil_dir(&self) -> PathBuf {
        self.git_dir.join("postil")
    }

    /// Where [`Repo::snapshot`] keeps its index and the copies it renames into place: a
    /// directory of their own, so once it exists a `postil/` that cannot be written (where the
    /// review file is) does not stop snapshots.
    fn snapshot_dir(&self) -> PathBuf {
        self.postil_dir().join("snapshot")
    }

    /// The directory name of the working tree, for display.
    pub(crate) fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// The name hippius-mem gives this repository's notes: the last segment of its `origin`
    /// remote, as [`memory_repo_name`] reads it. `None` with no `origin`, or one it cannot
    /// read, when hippius-mem binds no repository either. Runs `git` once.
    pub(crate) fn memory_name(&self) -> Option<String> {
        // Fails when there is no `origin`: no name then, as for hippius-mem.
        let url = git(&self.root, &["remote", "get-url", "origin"]).ok()?;
        memory_repo_name(&url).map(str::to_owned)
    }

    /// The checked-out branch, or an empty string when `HEAD` is detached.
    pub(crate) fn branch(&self) -> Result<String> {
        let branch = git(&self.root, &["branch", "--show-current"])?;
        Ok(branch.trim_end().to_owned())
    }

    /// Every tracked file and every untracked file that is not ignored, sorted, relative to
    /// the root. Repositories inside the working tree are left out, a submodule as well as an
    /// untracked one: they are directories, not files to open, and their files are not this
    /// repository's. So are tracked files git keeps off the disk, those outside a sparse
    /// checkout.
    pub(crate) fn files(&self) -> Result<Vec<String>> {
        let tracked_args = ["ls-files", "-z", "--stage", "-t"];
        let tracked = git(&self.root, &tracked_args)?;
        let mut files = Vec::new();
        for entry in tracked.split_terminator('\0') {
            // `<tag> <mode> <object> <stage>\t<path>`. A submodule is mode 160000, a gitlink.
            // Tag `S` is skip-worktree: a file outside a sparse checkout, which is not on
            // disk, or one hidden from `git status` by `update-index --skip-worktree`, which
            // is. Only `S` entries cost a look at the disk.
            let Some((fields, path)) = entry.split_once('\t') else {
                return Err(Error::Git {
                    command: tracked_args.join(" "),
                    // git exited successfully and wrote nothing to stderr; this field then
                    // says what was wrong with its output instead.
                    stderr: format!("(no error output, but an entry without a tab: {entry:?})"),
                });
            };
            let mut fields = fields.split(' ');
            let (tag, mode) = (fields.next(), fields.next());
            let gone = tag == Some("S") && fs::symlink_metadata(self.root.join(path)).is_err();
            if !gone && mode != Some("160000") {
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

    /// The files commit `commit` holds, relative to the root, submodules left out as
    /// [`Repo::files`] leaves them out. Reads the commit's tree, not the disk.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when `commit` is not a hexadecimal object id, and the git error,
    /// such as a commit the repository does not have.
    pub(crate) fn files_at(&self, commit: &str) -> Result<HashSet<String>> {
        check_object_id("commit", commit)?;
        let listed = git(&self.root, &["ls-tree", "-r", "-z", "--full-tree", commit])?;
        // `<mode> <type> <object>\t<path>`. A submodule is mode 160000, a gitlink.
        Ok(listed
            .split_terminator('\0')
            .filter_map(|entry| entry.split_once('\t'))
            .filter(|(fields, _)| !fields.starts_with("160000"))
            .map(|(_, path)| path.to_owned())
            .collect())
    }

    /// Takes a snapshot of the working tree, every tracked file and every untracked file that
    /// is not ignored, as a git tree, and returns its id. Postil takes one before a run and one
    /// after it, and the changes view shows the diff between them (docs/design.md, "The
    /// changes view").
    ///
    /// The tree is written through an index of Postil's own, `<git dir>/postil/snapshot/index`,
    /// so the engineer's index, branch and stash are left as they are. It is kept between
    /// snapshots, so git hashes again only the files whose stat data changed since the last
    /// one, untracked files included. It starts from a copy of the real index, and again
    /// whenever the two track different ignored files (one force-added, or one taken out of
    /// git and added to `.gitignore`), which `git add --all` cannot reconcile; each copy
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
        let index = self.snapshot_dir().join("index");
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
        // Beside `index`, so the rename into place stays within one directory.
        let copy = index.with_file_name(format!(
            "{}.{}.copy",
            std::process::id(),
            COPIES.fetch_add(1, Ordering::Relaxed)
        ));
        #[cfg(test)]
        self.copies.fetch_add(1, Ordering::Relaxed);
        // Made even when there is no index to copy: git then writes ours there from nothing.
        let dir = index
            .parent()
            .expect("the snapshot index is a file in snapshot_dir()");
        if let Err(source) = fs::create_dir_all(dir) {
            return Err(Error::CopyIndex {
                from: real,
                to: index.to_owned(),
                source,
            });
        }
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
        check_object_id("snapshot", before)?;
        check_object_id("snapshot", after)?;
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

    /// The commit `HEAD` names, or `None` in a repository with no commit yet.
    ///
    /// # Errors
    ///
    /// The git errors other than an unborn `HEAD`.
    pub(crate) fn head(&self) -> Result<Option<String>> {
        match git(
            &self.root,
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        ) {
            Ok(id) => Ok(Some(id.trim_end().to_owned())),
            // With `--quiet`, a name that resolves to nothing exits 1 and prints nothing: an
            // unborn `HEAD`. Any other failure prints why.
            Err(Error::Git { stderr, .. }) if stderr.trim().is_empty() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Makes a linked worktree at `path`, on the branch `checkout` names, and locks it
    /// (`git worktree lock`), so `git worktree remove` and `prune` leave it alone. Checks
    /// every file out, running the repository's smudge filters, so it takes as long as a
    /// checkout. Leaves this working tree's files, index and branch alone.
    ///
    /// # Errors
    ///
    /// The git error, such as a new branch whose name exists already, an existing branch that
    /// is gone or checked out elsewhere, or `path` that exists and is not empty.
    pub(crate) fn add_worktree(&self, path: &Path, checkout: Checkout<'_>) -> Result<()> {
        // No hooks: the checkout runs `post-checkout`, which may install or build, and the
        // worktree is Postil's, not a checkout the engineer made.
        // Absolute pointers between the worktree and git's record of it, whatever the
        // engineer's config says (`worktree.useRelativePaths`, git 2.48): Postil finds the
        // record from the folder's path when the folder is gone.
        let mut args = vec![
            OsStr::new("-c"),
            OsStr::new("core.hooksPath=/dev/null"),
            OsStr::new("-c"),
            OsStr::new("worktree.useRelativePaths=false"),
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--quiet"),
            OsStr::new("--lock"),
            OsStr::new("--reason"),
            OsStr::new("Postil's worktree of a thread: end the thread in Postil to remove it"),
        ];
        match checkout {
            Checkout::New { branch, fork } => {
                args.extend([
                    OsStr::new("-b"),
                    OsStr::new(branch),
                    path.as_os_str(),
                    OsStr::new(fork),
                ]);
            }
            Checkout::Existing { branch } => {
                args.extend([path.as_os_str(), OsStr::new(branch)]);
            }
        }
        run_git_with(&self.root, Search::Here, None, &args, None).map(drop)
    }

    /// The folder in which git keeps its record of the linked worktree at `path`
    /// (`<common git dir>/worktrees/<name>`): the one the worktree's `.git` file names, and
    /// whose own `gitdir` file names `path` back, as git checks; or, when the worktree's
    /// folder is gone, the one whose `gitdir` file names `path`. `None`
    /// when git has none. Deleting that folder is what `git worktree prune` does for one
    /// worktree, without touching any other whose folder is missing (a disk not mounted).
    ///
    /// # Errors
    ///
    /// [`Error::NotAWorktree`] when `path` exists and has no `.git` file naming a folder in
    /// this repository's `worktrees/` (no `.git`, one that cannot be read, or one naming
    /// another place), and the git error when the common git dir cannot be read.
    pub(crate) fn worktree_record(&self, path: &Path) -> Result<Option<PathBuf>> {
        let not_ours = || Error::NotAWorktree {
            path: path.to_owned(),
        };
        let common = rev_parse_path(&self.root, Search::Here, "--git-common-dir")?;
        let records = self.root.join(common).join("worktrees");
        let pointer = match fs::read_to_string(path.join(".git")) {
            Ok(pointer) => pointer,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A folder that is there without a `.git` is no worktree: it is not freed.
                if path.symlink_metadata().is_ok() {
                    return Err(not_ours());
                }
                // `gitdir` holds the worktree's `.git` path as `git worktree add` wrote it.
                let wanted = path.join(".git");
                let named = |record: &PathBuf| {
                    fs::read_to_string(record.join("gitdir"))
                        .is_ok_and(|gitdir| Path::new(gitdir.trim_end_matches('\n')) == wanted)
                };
                let Ok(entries) = fs::read_dir(&records) else {
                    return Ok(None);
                };
                return Ok(entries
                    .flatten()
                    .map(|entry| entry.path())
                    .find(|record| named(record)));
            }
            Err(_) => return Err(not_ours()),
        };
        let record = pointer
            .strip_prefix("gitdir: ")
            .map(|record| PathBuf::from(record.trim_end_matches('\n')))
            .ok_or_else(not_ours)?;
        let records = fs::canonicalize(&records).map_err(|_| not_ours())?;
        // Relative to the worktree when `worktree.useRelativePaths` wrote it; `join` keeps an
        // absolute pointer as it is.
        let record = fs::canonicalize(path.join(record)).map_err(|_| not_ours())?;
        if record.parent() != Some(records.as_path()) {
            return Err(not_ours());
        }
        // And the record names this folder back, as git itself checks: a `.git` rewritten to
        // name another worktree's record (the engineer's own linked worktree) would otherwise
        // pass, and git would act on that worktree's index and branch.
        let back = fs::read_to_string(record.join("gitdir")).map_err(|_| not_ours())?;
        let back = record.join(back.trim_end_matches('\n'));
        let ours = fs::canonicalize(path.join(".git")).map_err(|_| not_ours())?;
        if fs::canonicalize(back).ok() != Some(ours) {
            return Err(not_ours());
        }
        Ok(Some(record))
    }

    /// The commit local branch `branch` names, or `None` when there is no such branch.
    ///
    /// # Errors
    ///
    /// The git error, such as a repository git cannot read.
    pub(crate) fn branch_commit(&self, branch: &str) -> Result<Option<String>> {
        self.ref_commit(&format!("refs/heads/{branch}"))
    }

    /// The commit `refs/remotes/<remote>/<branch>` names: the branch on `remote` as this
    /// repository last fetched or pushed it. `None` when it has none.
    ///
    /// # Errors
    ///
    /// As [`Repo::branch_commit`].
    pub(crate) fn remote_branch_commit(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>> {
        self.ref_commit(&format!("refs/remotes/{remote}/{branch}"))
    }

    /// Where local branch `branch` lives on a remote: the remote and branch it tracks
    /// (`branch.<name>.remote` and `.merge`), else the branch of the same name on `origin`
    /// when there is one. `None` when there is neither.
    ///
    /// # Errors
    ///
    /// The git error when the config or the remotes cannot be read.
    pub(crate) fn upstream(&self, branch: &str) -> Result<Option<Upstream>> {
        let remote = self.config_value(&format!("branch.{branch}.remote"))?;
        // `.` is this repository itself: a branch tracking another local branch.
        if let Some(remote) = remote.filter(|remote| remote != ".") {
            let merge = self.config_value(&format!("branch.{branch}.merge"))?;
            let name = merge
                .as_deref()
                .and_then(|merge| merge.strip_prefix("refs/heads/"))
                .unwrap_or(branch);
            return Ok(Some(Upstream {
                remote,
                branch: name.to_owned(),
            }));
        }
        let remotes = git(&self.root, &["remote"])?;
        Ok(remotes
            .lines()
            .any(|remote| remote == "origin")
            .then(|| Upstream {
                remote: "origin".to_owned(),
                branch: branch.to_owned(),
            }))
    }

    /// The value of config `key`, or `None` when it is not set or empty.
    fn config_value(&self, key: &str) -> Result<Option<String>> {
        match git(&self.root, &["config", "--get", key]) {
            Ok(value) => Ok(Some(value.trim().to_owned()).filter(|value| !value.is_empty())),
            // `git config --get` exits 1, printing nothing, for a key that is not set.
            Err(Error::Git { stderr, .. }) if stderr.trim().is_empty() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// At most `most` of the commits reachable from `to` and not from `from`, newest first,
    /// each as its short id and subject: `"a1b2c3d Fix the parser"`.
    ///
    /// # Errors
    ///
    /// The git error, such as an id that names no commit.
    pub(crate) fn commits_between(&self, from: &str, to: &str, most: usize) -> Result<Vec<String>> {
        let range = format!("{from}..{to}");
        let count = format!("--max-count={most}");
        let log = git(&self.root, &["log", &count, "--format=%h %s", &range])?;
        Ok(log.lines().map(str::to_owned).collect())
    }

    /// Pushes local branch `branch` to the branch of the same name on `remote`, with the login
    /// shell's `search_path` as `PATH`: git runs the credential helpers the engineer set up
    /// (`gh auth git-credential`, `osxkeychain`) and their `pre-push` hook by it, and the app
    /// started from the Dock has a minimal `PATH`. The hook runs, unlike for Postil's own
    /// commits: this is the engineer's push to their remote, which a hook may guard (a secret
    /// scanner). Takes as long as the push and the hook.
    ///
    /// # Errors
    ///
    /// The git error, such as a remote that refuses the push or a missing credential.
    pub(crate) fn push_branch(
        &self,
        remote: &str,
        branch: &str,
        search_path: &OsStr,
    ) -> Result<()> {
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let args = ["push", "--quiet", remote, refspec.as_str()].map(OsStr::new);
        let mut command = git_command(&self.root, Search::Here);
        command.args(args).env("PATH", search_path);
        output_of(command, &args, None).map(drop)
    }

    /// The commit the full ref name `name` resolves to, or `None` when it names nothing.
    fn ref_commit(&self, name: &str) -> Result<Option<String>> {
        let name = format!("{name}^{{commit}}");
        match git(&self.root, &["rev-parse", "--verify", "--quiet", &name]) {
            Ok(id) => Ok(Some(id.trim_end().to_owned())),
            // As in `head`: with `--quiet`, a name that resolves to nothing prints nothing.
            Err(Error::Git { stderr, .. }) if stderr.trim().is_empty() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether `commit` is `descendant` or one of its ancestors.
    ///
    /// # Errors
    ///
    /// The git error, such as an id that names no commit.
    pub(crate) fn is_ancestor(&self, commit: &str, descendant: &str) -> Result<bool> {
        match git(
            &self.root,
            &["merge-base", "--is-ancestor", commit, descendant],
        ) {
            Ok(_) => Ok(true),
            // Exit 1 with nothing on stderr is "not an ancestor"; a bad id prints why.
            Err(Error::Git { stderr, .. }) if stderr.trim().is_empty() => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The full id of the commit `commit` (a hexadecimal id, full or short) names, or `None`
    /// when the repository has no such commit: one `git gc` pruned after a rebase, say.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when `commit` is not a hexadecimal object id, and the other git
    /// errors, such as an id of an object that is not a commit, or a short id that names more
    /// than one object.
    pub(crate) fn resolve_commit(&self, commit: &str) -> Result<Option<String>> {
        check_object_id("commit", commit)?;
        self.ref_commit(commit)
    }

    /// The paths that differ between commits `from` and `to` (or two snapshot trees,
    /// [`Repo::snapshot`]), each with how it changed, in git's order. A rename is a deletion
    /// and an addition (`--no-renames`), so a file moved into a folder no node covers reads as
    /// added. A submodule is left out, as [`Repo::files`] leaves it out. Reads the two trees,
    /// not the disk.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when an id is not a hexadecimal object id, and the git error,
    /// such as a commit the repository does not have.
    pub(crate) fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<ChangedPath>> {
        check_object_id("commit", from)?;
        check_object_id("commit", to)?;
        let args = [
            "diff",
            "--name-status",
            "-z",
            "--no-renames",
            "--ignore-submodules=all",
            "--no-ext-diff",
            from,
            to,
        ];
        let listed = git(&self.root, &args)?;
        // `<status>\0<path>\0` for each path: with `-z` the path is written as it is, unquoted.
        let mut fields = listed.split_terminator('\0');
        let mut changed = Vec::new();
        while let Some(status) = fields.next() {
            let change = match status {
                "A" => Change::Added,
                "D" => Change::Deleted,
                "M" => Change::Modified,
                "T" => Change::TypeChanged,
                // Unmerged (`U`) and unknown (`X`) entries are in no diff of two commits.
                other => {
                    return Err(Error::Git {
                        command: args.join(" "),
                        stderr: format!("(no error output, and an unexpected status {other:?})"),
                    });
                }
            };
            let Some(path) = fields.next() else {
                return Err(Error::Git {
                    command: args.join(" "),
                    stderr: format!("(no error output, and status {status} has no path)"),
                });
            };
            changed.push(ChangedPath {
                change,
                path: path.to_owned(),
            });
        }
        Ok(changed)
    }

    /// Whether git ignores `path` (relative to the root) in this working tree, by its own
    /// `.gitignore` files and the repository's exclude rules. A path git cannot judge counts
    /// as not ignored: its one caller then leaves the folder out, the safe side.
    pub(crate) fn is_ignored(&self, path: &str) -> bool {
        // Exit 0 is "ignored", 1 "not ignored", and anything else an error.
        git(&self.root, &["check-ignore", "--quiet", path]).is_ok()
    }

    /// Puts the working tree back to branch `branch` at commit `commit`: the branch is checked
    /// out and moved there, tracked files are restored, and untracked files that are not
    /// ignored are deleted. Ignored files (the cloned build folders) stay. No hook runs. For a
    /// thread's worktree only: it throws work away, a commit made on another branch since
    /// included.
    ///
    /// # Errors
    ///
    /// The git errors.
    pub(crate) fn reset_to(&self, branch: &str, commit: &str) -> Result<()> {
        let no_hooks = ["-c", "core.hooksPath=/dev/null"];
        let checkout = ["checkout", "--quiet", "--force", "-B", branch, commit];
        git(&self.root, &[&no_hooks[..], &checkout[..]].concat())?;
        let clean = ["clean", "-d", "--force", "--quiet"];
        git(&self.root, &[&no_hooks[..], &clean[..]].concat()).map(drop)
    }

    /// Deletes the local branch `branch` while it names commit `expected`, and keeps it when it
    /// names another commit (one made on it since) or a working tree has it checked out (the
    /// engineer's own, or any linked worktree): either way it is someone's now. The delete
    /// itself is `update-ref -d <ref> <expected>`, which refuses a branch that moved
    /// meanwhile. A branch that does not exist (never made, or deleted by hand) is already as
    /// asked.
    ///
    /// # Errors
    ///
    /// The git errors.
    pub(crate) fn delete_branch_at(&self, branch: &str, expected: &str) -> Result<()> {
        let name = format!("refs/heads/{branch}");
        // One line per branch: its commit, then the folder that has it checked out, empty when
        // none does. No line: no such branch.
        let format = "--format=%(objectname) %(worktreepath)";
        let line = git(&self.root, &["for-each-ref", format, &name])?;
        let Some((at, checked_out)) = line.trim_end_matches('\n').split_once(' ') else {
            return Ok(());
        };
        if at != expected || !checked_out.is_empty() {
            // Kept.
            return Ok(());
        }
        match git(&self.root, &["update-ref", "-d", &name, expected]) {
            Ok(_) => Ok(()),
            // Refused: when the branch moved between the two calls, it is kept, which is the
            // point; any other failure is git's to report.
            Err(error) => {
                let now = git(&self.root, &["rev-parse", "--verify", "--quiet", &name]);
                match now {
                    Ok(now) if now.trim_end() != expected => Ok(()),
                    // Deleted meanwhile: as asked. As in `head`, nothing on stderr is a name
                    // that resolves to nothing.
                    Err(Error::Git { stderr, .. }) if stderr.trim().is_empty() => Ok(()),
                    _ => Err(error),
                }
            }
        }
    }

    /// Checks out the submodules `.gitmodules` names, when there is one. A new worktree has
    /// none checked out. May fetch over the network. Git asks for no credential
    /// (`GIT_TERMINAL_PROMPT=0`); an ssh key with a passphrase can still ask on the terminal
    /// Postil was started from.
    ///
    /// # Errors
    ///
    /// The git error, such as a submodule that cannot be fetched.
    pub(crate) fn update_submodules(&self) -> Result<()> {
        if !self.root.join(".gitmodules").exists() {
            return Ok(());
        }
        let args = [
            "-c",
            "core.hooksPath=/dev/null",
            "submodule",
            "update",
            "--init",
            "--quiet",
        ];
        git(&self.root, &args).map(drop)
    }

    /// The engineer's changes since the last commit, for the warning at send: the entries
    /// `git status` lists, each its path and a rename's source. An untracked folder is one
    /// entry (`dir/`), as `git status` shows it, not one per file in it, so a large one counts
    /// once and costs little. Takes no optional lock (`--no-optional-locks`): the engineer may
    /// be committing in a terminal meanwhile.
    ///
    /// # Errors
    ///
    /// The git error.
    pub(crate) fn uncommitted(&self) -> Result<Vec<(String, Option<String>)>> {
        let status = run_git(
            &self.root,
            None,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "-z",
                "--untracked-files=normal",
            ],
        )?;
        Ok(status_entries(&status))
    }

    /// The paths with changes git would commit, as `git status` lists them: tracked files
    /// changed or deleted, and files that are neither tracked nor ignored. Empty when the
    /// working tree is clean. Ignored files are not listed.
    ///
    /// # Errors
    ///
    /// The git error.
    pub(crate) fn changed(&self) -> Result<Vec<String>> {
        let status = run_git(
            &self.root,
            None,
            &["status", "--porcelain", "-z", "--untracked-files=all"],
        )?;
        Ok(status_entries(&status)
            .into_iter()
            .map(|(path, _)| path)
            .collect())
    }

    /// Commits every change in the working tree with `message`, and returns the commit
    /// `HEAD` names afterwards: the new one, or the one before when nothing was staged. Ignored
    /// files stay out, and so do changes inside a submodule or a repository the working tree
    /// holds, which `git add` does not stage. Runs `git add --all` and `git commit` on the
    /// working tree's own index, so it is for a thread's worktree, never the engineer's.
    ///
    /// The commit runs no hook (`core.hooksPath` emptied: `--no-verify` alone still runs
    /// `prepare-commit-msg` and `post-commit`) and keeps the message as it is
    /// (`--cleanup=verbatim`, so a note's line starting with `#` stays). It is not signed: it
    /// runs on the page's request thread or an agent's reader thread, where a signer asking for
    /// a passphrase would stall every request or the run, and it is Postil's snapshot, not the
    /// engineer's commit. The author
    /// follows the git config.
    ///
    /// # Errors
    ///
    /// The git errors, such as a missing `user.name`, or a working tree with no commit.
    pub(crate) fn commit_all(&self, message: &str) -> Result<String> {
        // No hooks: writing the index runs `post-index-change`. `--sparse` (git 2.34): in a
        // worktree with a sparse checkout, which it takes from the engineer's, a file the agent
        // writes outside the patterns is refused without it ("outside of your sparse-checkout
        // definition"), and every later commit of the thread fails with it.
        git(
            &self.root,
            &["-c", "core.hooksPath=/dev/null", "add", "--all", "--sparse"],
        )?;
        // What was staged, not what `git status` lists: a submodule or a repository inside
        // the worktree with changes of its own is listed, but `git add` stages nothing for it,
        // and a commit of nothing fails. `diff --cached --quiet` exits 1 when there is any.
        let staged = git(&self.root, &["diff", "--cached", "--quiet"]).is_err();
        if staged {
            git(
                &self.root,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgSign=false",
                    "commit",
                    "--quiet",
                    "--no-verify",
                    "--cleanup=verbatim",
                    "--message",
                    message,
                ],
            )?;
        }
        self.head()?.ok_or_else(|| Error::Git {
            command: "rev-parse HEAD".to_owned(),
            stderr: "the worktree has no commit".to_owned(),
        })
    }

    /// The changes from commit `from` to commit `to` as a patch `git apply` takes: binary
    /// files included, renames found. The options fix the format whatever the engineer's git
    /// config says, as for [`Repo::diff`].
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when `from` or `to` is not a hexadecimal object id, and the git
    /// errors, such as an id that names no commit.
    pub(crate) fn patch(&self, from: &str, to: &str) -> Result<Vec<u8>> {
        check_object_id("commit", from)?;
        check_object_id("commit", to)?;
        run_git_bytes(
            &self.root,
            None,
            &[
                "-c",
                "core.quotePath=false",
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--find-renames",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                from,
                to,
            ],
        )
    }

    /// Applies `patch` ([`Repo::patch`]) to the working tree's files, leaving the index
    /// alone. All or nothing: git refuses the whole patch when any part of it does not fit,
    /// and then nothing changes. An empty patch changes nothing.
    ///
    /// # Errors
    ///
    /// The git error, naming each file that does not fit.
    pub(crate) fn apply(&self, patch: &[u8]) -> Result<()> {
        if patch.is_empty() {
            return Ok(());
        }
        let args = ["apply", "--whitespace=nowarn", "-"].map(OsStr::new);
        run_git_with(&self.root, Search::Here, None, &args, Some(patch)).map(drop)
    }

    /// The ignored folders at the top of the working tree that are on disk, such as `target`
    /// or `node_modules`: folder names, without the trailing `/`. Files, folders below the
    /// top, and symlinks are left out.
    ///
    /// # Errors
    ///
    /// The git error.
    pub(crate) fn ignored_folders(&self) -> Result<Vec<String>> {
        let list = run_git(
            &self.root,
            None,
            &[
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
            ],
        )?;
        Ok(list
            .split_terminator('\0')
            .filter_map(|entry| entry.strip_suffix('/'))
            .filter(|name| !name.contains('/'))
            .filter(|name| {
                fs::symlink_metadata(self.root.join(name)).is_ok_and(|meta| meta.is_dir())
            })
            .map(str::to_owned)
            .collect())
    }

    /// Reads one file as text. `file` is a path relative to the root, as [`Repo::files`]
    /// returns it. A symlink inside the tree is followed, wherever it points: the reader sees
    /// what an editor would show.
    ///
    /// # Errors
    ///
    /// [`Error::OutsideRepository`] for a path not written as git writes it (one that leaves
    /// the root, is absolute, or has an empty, `.` or `..` name), [`Error::FileTooLarge`]
    /// above [`MAX_READ`] bytes, and [`Error::Read`] when the file cannot be read as UTF-8
    /// text.
    pub(crate) fn read(&self, file: &str) -> Result<String> {
        let path = self.inside(file)?;
        // At most one byte past the limit is read, so a file is refused without reading more
        // than that, whatever its size. Bytes, then text: the limit can cut a character in
        // two, and a large file must be refused as large, not as "not text".
        let mut bytes = Vec::new();
        let read =
            File::open(&path).and_then(|opened| opened.take(MAX_READ + 1).read_to_end(&mut bytes));
        match read {
            Ok(read) if read as u64 > MAX_READ => {
                // The size on disk says how far past the limit it is, and one past the limit is
                // the floor: the file may have shrunk, or vanished, since it was read.
                let bytes = fs::metadata(&path)
                    .map_or(0, |metadata| metadata.len())
                    .max(MAX_READ + 1);
                Err(Error::FileTooLarge { path, bytes })
            }
            Ok(_) => String::from_utf8(bytes).map_err(|error| Error::Read {
                path,
                source: io::Error::new(io::ErrorKind::InvalidData, error.utf8_error()),
            }),
            Err(source) => Err(Error::Read { path, source }),
        }
    }

    /// Reads one file as commit `commit` holds it, as text: `git show <commit>:<file>`, the
    /// blob as git stores it, with no textconv driver and no smudge filter. `commit` may also
    /// be a tree, as the snapshots of runs from before threads are. `file` is written as for
    /// [`Repo::read`]; a folder's path shows git's listing of it, so a caller that needs a
    /// file checks the path against [`Repo::files_at`] first.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when `commit` is not a hexadecimal object id;
    /// [`Error::OutsideRepository`] as for [`Repo::read`]; the git error when the commit or
    /// the file in it does not exist; [`Error::FileTooLarge`] above [`MAX_READ`] bytes, and
    /// [`Error::Read`] when the blob is not UTF-8 text.
    pub(crate) fn read_at(&self, commit: &str, file: &str) -> Result<String> {
        check_object_id("commit", commit)?;
        let path = self.inside(file)?;
        let object = format!("{commit}:{file}");
        let args = ["show", "--no-color", "--no-textconv", object.as_str()].map(OsStr::new);
        let mut child = git_command(&self.root, Search::Here)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(Error::SpawnGit)?;
        // At most one byte past the limit is read, as `Repo::read` reads a file, so a large
        // blob is refused without holding it. git's error output is a line or two, which the
        // pipe holds while stdout is read.
        let stdout = child.stdout.take().expect("stdout is piped above");
        let mut bytes = Vec::new();
        let read = stdout.take(MAX_READ + 1).read_to_end(&mut bytes);
        if bytes.len() as u64 > MAX_READ {
            // git is still writing the rest: stopped, as nothing more of it is read. Its exit
            // status says nothing once it is killed.
            drop(child.kill());
            drop(child.wait());
            // The blob's size is not read: one past the limit is the floor, as for `read`.
            return Err(Error::FileTooLarge {
                path,
                bytes: MAX_READ + 1,
            });
        }
        let output = child.wait_with_output().map_err(Error::SpawnGit)?;
        if !output.status.success() {
            return Err(Error::Git {
                command: args.map(OsStr::to_string_lossy).join(" "),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        if let Err(source) = read {
            return Err(Error::Read { path, source });
        }
        String::from_utf8(bytes).map_err(|error| Error::Read {
            path,
            source: io::Error::new(io::ErrorKind::InvalidData, error.utf8_error()),
        })
    }

    /// What `path` is in the working tree now: a file, a folder, or `None` when nothing is
    /// there or it is neither (a socket, a FIFO). One `lstat`; reads no file.
    ///
    /// A symlink at `path` is a file, wherever it points, and is not followed: git tracks the
    /// link itself, so [`Repo::files`] lists it and the file list shows it as a file, even when
    /// it points at a folder. A symlink earlier in `path` is followed, as for [`Repo::read`].
    ///
    /// An `lstat` that fails for another reason (no permission to look) also gives `None`: a
    /// note's caller can only say "not a file or folder" either way, and no error variant
    /// names a failed look at a path that may be a folder.
    ///
    /// # Errors
    ///
    /// [`Error::OutsideRepository`] for a path not written as git writes it, as for
    /// [`Repo::read`].
    pub(crate) fn kind(&self, path: &str) -> Result<Option<PathKind>> {
        let full = self.inside(path)?;
        Ok(match fs::symlink_metadata(full) {
            Ok(metadata) if metadata.is_file() || metadata.is_symlink() => Some(PathKind::File),
            Ok(metadata) if metadata.is_dir() => Some(PathKind::Folder),
            _ => None,
        })
    }

    /// Checks that `path` is written as git writes paths, as [`Repo::read`] does, without
    /// looking at the disk: it need not exist.
    ///
    /// # Errors
    ///
    /// [`Error::OutsideRepository`] when it is not.
    pub(crate) fn check_path(&self, path: &str) -> Result<()> {
        self.inside(path).map(drop)
    }

    /// `file` on disk, once it is known to name something inside the root.
    ///
    /// The path comes from the page. It must be written as git writes paths: names joined by
    /// single `/`, none of them `.` or `..`, with no leading or trailing `/`. So it cannot
    /// climb above the root or be absolute, and each file or folder has one spelling, the one
    /// a note stores and a prompt shows.
    fn inside(&self, file: &str) -> Result<PathBuf> {
        let plain = !file.is_empty()
            && file
                .split('/')
                .all(|name| !name.is_empty() && name != "." && name != "..");
        if !plain {
            return Err(Error::OutsideRepository {
                path: file.to_owned(),
            });
        }
        Ok(self.root.join(file))
    }
}

/// A path that differs between two commits, and how ([`Repo::changed_paths`]).
#[derive(Debug)]
pub(crate) struct ChangedPath {
    pub(crate) change: Change,
    /// Relative to the root, as git writes it.
    pub(crate) path: String,
}

/// How a path changed between two commits: git's `--name-status` letters `A`, `D`, `M`, `T`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    Added,
    Deleted,
    Modified,
    /// A file became a symlink or a submodule, or the other way round.
    TypeChanged,
}

/// What a path in the working tree is, as [`Repo::kind`] finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathKind {
    File,
    Folder,
}

impl fmt::Display for PathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::File => "file",
            Self::Folder => "folder",
        })
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

/// The repository name in the git remote `url`, read as hippius-mem reads it, so Postil's
/// recalls name the repository its agents' notes are kept under: `normalize_remote` and
/// `resolve_profile` in hippius-mem at db9261b
/// (`hippius-mem/src/resolver.rs`, `hippius-mem/src/main.rs`), which take the last segment of
/// `host/org/.../repo` once one trailing `.git` and any trailing `/` are gone. Its three
/// shapes: `https://host/org/repo`, `ssh://user@host:22/org/repo` and the scp-like
/// `user@host:org/repo`. `None` for a path with fewer than two segments (an org alone), a
/// local path, or a one-character host (a Windows drive letter).
fn memory_repo_name(url: &str) -> Option<&str> {
    let url = url.trim();
    // `://` first, so the scheme's colon is never read as the scp-like form's.
    let (authority, path) = match url.split_once("://") {
        Some((_scheme, after)) => after.split_once('/')?,
        None => url.split_once(':')?,
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split_once(':').map_or(host, |(bare, _port)| bare);
    if host.len() < 2 {
        return None;
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    // The org; the name is the last segment after it.
    segments.next()?;
    segments.next_back()
}

/// Checks that `id`, a `kind` of object ("commit", "snapshot") from a run log or a request,
/// is a hexadecimal git object id, so neither a name git would resolve some other way nor a
/// leading `-` read as an option reaches git.
///
/// # Errors
///
/// [`Error::BadRequest`] naming the id when it is not one.
fn check_object_id(kind: &str, id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::BadRequest(format!(
            "{kind} {id:?}: expected a git object id"
        )));
    }
    Ok(())
}

/// Runs `git` in `dir` and returns what it printed.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    run_git(dir, None, args)
}

/// Runs `git rev-parse <option>` in `dir` and returns the one path it prints, byte for byte:
/// a path may hold a newline, or bytes that are not UTF-8.
fn rev_parse_path(dir: &Path, search: Search, option: &str) -> Result<PathBuf> {
    let args = ["rev-parse", option].map(OsStr::new);
    let mut path = run_git_with(dir, search, None, &args, None)?;
    // git ends the path with one newline; any other newline is part of the path.
    if path.pop() != Some(b'\n') || path.is_empty() {
        return Err(Error::Git {
            command: format!("rev-parse {option}"),
            // git exited successfully and wrote nothing to stderr; this field then says what
            // was wrong with its output instead.
            stderr: "(no error output, and stdout is not one path ending in a newline)".to_owned(),
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
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    run_git_with(dir, Search::Here, index, &args, None)
}

/// Where git looks for the repository of the folder it runs in.
#[derive(Debug)]
enum Search {
    /// In the folder and every folder above it, as git does by default: only in
    /// [`Repo::open`], whose callers check the root they get where it matters
    /// ([`crate::thread::open`]).
    Up,
    /// In the folder alone. Every other call runs in a working tree's top folder, and when
    /// its `.git` is gone (an agent in a thread's worktree may delete it), searching up
    /// would act on whatever repository holds the folder, a dotfiles repository in the
    /// home folder, say. git then fails instead, except under a folder whose path holds a
    /// colon (see [`run_git_with`]), which a thread's worktree is never put in
    /// ([`crate::thread::plan`]).
    Here,
}

/// Runs `git` in `dir` with arguments that may be paths, writes `input` to its standard input
/// when given, and returns what it printed. Without `input` its standard input is empty, so
/// a command that reads a credential from it fails instead of waiting; with
/// `GIT_TERMINAL_PROMPT=0` neither does git ask on the terminal. ssh can still read a key's
/// passphrase from the terminal Postil was started from, which the app from the Dock lacks.
fn run_git_with(
    dir: &Path,
    search: Search,
    index: Option<&Path>,
    args: &[&OsStr],
    input: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut command = git_command(dir, search);
    command.args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    output_of(command, args, input)
}

/// The entries of `git status --porcelain -z` output: each one's path and, for a rename or a
/// copy, its source, which follows it as a field of its own. `XY path`: a rename shows in
/// either column, `R ` once staged and ` R` when only the working tree has it (the moved file
/// added with `git add -N`).
fn status_entries(status: &str) -> Vec<(String, Option<String>)> {
    let mut entries = Vec::new();
    let mut fields = status.split_terminator('\0');
    while let Some(field) = fields.next() {
        let (code, path) = field.split_at(field.len().min(3));
        let renamed = code
            .bytes()
            .take(2)
            .any(|column| matches!(column, b'R' | b'C'));
        let source = if renamed {
            fields.next().map(str::to_owned)
        } else {
            None
        };
        entries.push((path.to_owned(), source));
    }
    entries
}

/// `git -C <dir>`, set to find the repository at `dir` as `search` says and never to prompt
/// ([`run_git_with`]), with no arguments yet.
fn git_command(dir: &Path, search: Search) -> Command {
    let mut command = Command::new("git");
    match (search, dir.parent()) {
        // git looks in `dir`, then stops before the ceiling, its parent. It resolves links
        // in both before comparing them. The variable is a colon-separated list with no
        // escape, so a parent whose path holds a colon is read as two paths, neither of them
        // the parent, and git searches up as with `Search::Up`. Refusing instead would stop
        // Postil in every repository under such a folder; `thread::plan` keeps colons out of
        // the paths of threads' worktrees, where agents work.
        (Search::Here, Some(parent)) => drop(command.env("GIT_CEILING_DIRECTORIES", parent)),
        (Search::Here, None) | (Search::Up, _) => {}
    }
    command
        .arg("-C")
        .arg(dir)
        // Never wait on a terminal prompt: there is none, and a send would hang.
        .env("GIT_TERMINAL_PROMPT", "0")
        // Messages in English whatever the user's locale: `Repo::open` matches one of them.
        .env("LC_ALL", "C")
        // The repository is the one `-C` names. These variables, inherited from a terminal
        // that a script or a hook left them set in, would point git somewhere else, `git init`
        // included.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
    command
}

/// Runs `command`, a [`git_command`] given its `args`, writing `input` to its standard input
/// when given (empty otherwise), and returns what it printed.
fn output_of(mut command: Command, args: &[&OsStr], input: Option<&[u8]>) -> Result<Vec<u8>> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(Error::SpawnGit)?;
    let output = std::thread::scope(|scope| {
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            // On a thread of its own: git may fill its output pipe before it reads all of the
            // input, and this thread reads that output. A git that exits early closes the
            // pipe, and the write fails; its exit status and message then say why.
            scope.spawn(move || drop(stdin.write_all(input)));
        }
        child.wait_with_output()
    })
    .map_err(Error::SpawnGit)?;

    if !output.status.success() {
        let command: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        return Err(Error::Git {
            command: command.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn the_memory_name_is_the_remote_s_last_segment_as_hippius_mem_reads_it() {
        for (url, name) in [
            (
                "https://github.com/GeorgiosDelkos/postil.git",
                Some("postil"),
            ),
            ("https://github.com/GeorgiosDelkos/postil", Some("postil")),
            ("git@github.com:GeorgiosDelkos/postil.git\n", Some("postil")),
            ("ssh://git@host.example:22/team/postil", Some("postil")),
            ("https://github.com/team/postil.git/", Some("postil")),
            // A nested group: the last segment, and one `.git` only.
            (
                "https://gitlab.com/group/sub/tool.git.git",
                Some("tool.git"),
            ),
            // An org alone, a local path and a drive letter name no repository.
            ("https://github.com/team", None),
            ("/srv/git/postil.git", None),
            ("C:/work/postil", None),
            // A two-character host is a host.
            ("git@gh:team/postil", Some("postil")),
            ("", None),
        ] {
            assert_eq!(memory_repo_name(url), name, "{url:?}");
        }
    }

    #[test]
    fn the_memory_name_comes_from_origin_and_not_from_the_folder() {
        let dir = TempDir::new("repo-memory-name");
        let root = dir.path().join("agent-worktree");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet"]).unwrap();
        let repo = Repo::open(&root).unwrap();
        assert_eq!(repo.memory_name(), None, "no origin: no name");

        git(
            &root,
            &["remote", "add", "origin", "git@github.com:Owner/postil.git"],
        )
        .unwrap();
        assert_eq!(repo.memory_name().as_deref(), Some("postil"));
    }

    #[test]
    fn a_working_tree_whose_git_folder_is_gone_fails_instead_of_reaching_the_one_above() {
        let dir = TempDir::new("repo-no-search-up");
        git(dir.path(), &["init", "--quiet"]).unwrap();
        let inner = dir.path().join("inner");
        fs::create_dir(&inner).unwrap();
        git(&inner, &["init", "--quiet"]).unwrap();
        fs::write(inner.join("a.txt"), "a\n").unwrap();
        let repo = Repo::open(&inner).unwrap();
        assert_eq!(repo.changed().unwrap(), ["a.txt"]);

        fs::remove_dir_all(inner.join(".git")).unwrap();

        // The outer repository would list `inner/a.txt` as untracked.
        let error = repo.changed().unwrap_err();
        assert!(
            matches!(&error, Error::Git { stderr, .. } if stderr.contains("not a git repository")),
            "{error:?}"
        );
    }

    #[test]
    fn read_at_reads_the_blob_and_refuses_what_is_not_text_within_the_limit() {
        let dir = TempDir::new("repo-read-at");
        git(dir.path(), &["init", "--quiet"]).unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), "then\n").unwrap();
        fs::write(dir.path().join("binary"), [0xff, 0xfe]).unwrap();
        let too_large = usize::try_from(MAX_READ + 1).expect("20 MB fits in usize");
        fs::write(dir.path().join("large"), vec![b'x'; too_large]).unwrap();
        let tree = repo.snapshot().unwrap();
        fs::write(dir.path().join("a.txt"), "now\n").unwrap();

        assert_eq!(repo.read_at(&tree, "a.txt").unwrap(), "then\n");
        let binary = repo.read_at(&tree, "binary").unwrap_err();
        let not_text = matches!(
            &binary,
            Error::Read { source, .. } if source.kind() == io::ErrorKind::InvalidData
        );
        assert!(not_text, "{binary:?}");
        let large = repo.read_at(&tree, "large").unwrap_err();
        assert!(matches!(large, Error::FileTooLarge { .. }), "{large:?}");
        let missing = repo.read_at(&tree, "gone.txt").unwrap_err();
        assert!(matches!(missing, Error::Git { .. }), "{missing:?}");
        let outside = repo.read_at(&tree, "../a.txt").unwrap_err();
        assert!(
            matches!(outside, Error::OutsideRepository { .. }),
            "{outside:?}"
        );
        let named = repo.read_at("HEAD", "a.txt").unwrap_err();
        assert!(matches!(named, Error::BadRequest(_)), "{named:?}");
    }

    #[test]
    fn changed_paths_lists_added_deleted_modified_and_type_changed_files() {
        let dir = TempDir::new("repo-changed-paths");
        let root = dir.path();
        git(root, &["init", "--quiet"]).unwrap();
        git(root, &["config", "user.name", "Postil Test"]).unwrap();
        git(
            root,
            &["config", "user.email", "postil-test@example.invalid"],
        )
        .unwrap();
        let commit = || {
            git(root, &["add", "--all"]).unwrap();
            git(root, &["commit", "--quiet", "--message", "c"]).unwrap();
            Repo::open(root).unwrap().head().unwrap().unwrap()
        };
        fs::write(root.join("old.txt"), "the same text\n").unwrap();
        fs::write(root.join("edited.txt"), "before\n").unwrap();
        fs::write(root.join("link"), "a file\n").unwrap();
        let from = commit();

        // The moved file keeps its text, which git would call a rename; the new name has a
        // space and a letter that is not ASCII, which git quotes without `-z`.
        fs::remove_file(root.join("old.txt")).unwrap();
        fs::write(root.join("new é.txt"), "the same text\n").unwrap();
        fs::write(root.join("edited.txt"), "after\n").unwrap();
        fs::remove_file(root.join("link")).unwrap();
        std::os::unix::fs::symlink("edited.txt", root.join("link")).unwrap();
        let to = commit();

        let repo = Repo::open(root).unwrap();
        let changed: Vec<(Change, String)> = repo
            .changed_paths(&from, &to)
            .unwrap()
            .into_iter()
            .map(|changed| (changed.change, changed.path))
            .collect();
        assert_eq!(
            changed,
            [
                (Change::Modified, "edited.txt".to_owned()),
                (Change::TypeChanged, "link".to_owned()),
                (Change::Added, "new é.txt".to_owned()),
                (Change::Deleted, "old.txt".to_owned()),
            ]
        );
        assert!(repo.changed_paths(&to, &to).unwrap().is_empty());

        // A submodule added: a gitlink, which no file list holds.
        let gitlink = format!("160000,{to},vendor/sub");
        git(root, &["update-index", "--add", "--cacheinfo", &gitlink]).unwrap();
        git(root, &["commit", "--quiet", "--message", "sub"]).unwrap();
        let with_sub = repo.head().unwrap().unwrap();
        assert!(
            repo.changed_paths(&to, &with_sub).unwrap().is_empty(),
            "a submodule is left out"
        );
        let named = repo.changed_paths("HEAD~1", &to).unwrap_err();
        assert!(matches!(named, Error::BadRequest(_)), "{named:?}");
    }

    #[test]
    fn resolve_commit_gives_the_full_id_or_none_for_a_commit_the_repository_lacks() {
        let dir = TempDir::new("repo-resolve-commit");
        let root = dir.path();
        git(root, &["init", "--quiet"]).unwrap();
        git(root, &["config", "user.name", "Postil Test"]).unwrap();
        git(
            root,
            &["config", "user.email", "postil-test@example.invalid"],
        )
        .unwrap();
        git(
            root,
            &["commit", "--quiet", "--allow-empty", "--message", "c"],
        )
        .unwrap();
        let repo = Repo::open(root).unwrap();
        let head = repo.head().unwrap().unwrap();

        assert_eq!(repo.resolve_commit(&head[..7]).unwrap(), Some(head.clone()));
        let missing = "0".repeat(head.len());
        assert_eq!(repo.resolve_commit(&missing).unwrap(), None);
        let tree = git(root, &["rev-parse", "HEAD^{tree}"]).unwrap();
        let tree = repo.resolve_commit(tree.trim_end()).unwrap_err();
        assert!(matches!(tree, Error::Git { .. }), "not a commit: {tree:?}");
        let named = repo.resolve_commit("--all").unwrap_err();
        assert!(matches!(named, Error::BadRequest(_)), "{named:?}");
    }
}
