//! Threads: the worktree and branch each thread of runs works in (docs/design.md,
//! "Worktrees").
//!
//! A thread is a run of notes with its follow-ups and review stages. Its id is the first
//! run's (`R3`). It works in a linked git worktree of its own, on branch `postil/r3-<slug>`
//! started at the commit `HEAD` named at send, so two threads never write one working tree,
//! and the engineer's working tree is never an agent's.
//!
//! # The record
//!
//! `<git dir>/postil/threads.json` lists every thread with its worktree's path, its branch,
//! the commit it started from (`fork`) and the last commit Postil made or found on it
//! (`tip`), and whether it is open or how it ended. It is written before the worktree is
//! made, so a crash leaves a record naming whatever was made. Every change reads the file,
//! changes one thread, and writes it whole, under an exclusive lock on `threads.lock` beside
//! it, so two windows on one repository keep each other's changes. A file that cannot be read
//! or parsed is reported and never written over.
//!
//! # Places
//!
//! Worktrees live under Postil's home folder, `~/.postil`, in
//! `worktrees/<repo>-<id>/r<n>`, `<id>` a hash of the repository's path. A repository on
//! another volume gets its worktrees under `.postil` at that volume's root instead, so the
//! build folders can be cloned (`clone.rs`), which works only within one volume. An ended
//! thread's worktree is renamed into `trash/` beside `worktrees/` and deleted there on a
//! background thread; opening the repository again finishes what a quit cut short.
//!
//! Postil deletes only what it made: a folder directly in this repository's worktrees
//! folder whose `.git` names a worktree record of this repository, and branches under
//! `postil/`. The record is a file a hand edit can change.

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::clone::clone_folder;
use crate::error::{Error, Result, error_chain};
use crate::files::{save_json, version_of};
use crate::repo::Repo;

/// The format version of `threads.json`.
const THREADS_VERSION: u32 = 1;

/// The longest slug a branch name takes from a note, in characters. Arbitrary: long enough to
/// tell threads apart in `git branch`, short enough for a pull request's head.
const SLUG_LENGTH: usize = 40;

/// One thread, as `threads.json` stores it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Thread {
    /// The thread's first run, `R3`.
    pub(crate) id: String,
    /// The worktree's folder, absolute.
    pub(crate) path: PathBuf,
    /// The thread's branch, `postil/r3-fix-the-parser`.
    pub(crate) branch: String,
    /// The commit the branch started from: what `HEAD` named at send.
    pub(crate) fork: String,
    /// The branch `HEAD` was on at send, or `None` when it was detached: a pull request goes
    /// to this branch.
    pub(crate) base: Option<String>,
    /// The last commit Postil made or found on the branch, before or after a run; `fork`
    /// until then. Ending the thread checks the worktree is at it, or past it.
    pub(crate) tip: String,
    pub(crate) state: State,
    /// What the worktree was not given ([`make`]): ignored folders, or its submodules, each
    /// with the reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) skipped: Vec<String>,
}

/// Whether a thread is open, or how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum State {
    /// Its worktree is on disk (or being made), and its runs can go on.
    Open,
    /// Its changes were written into the engineer's working tree; the worktree is gone.
    Applied,
    /// It ended with nothing applied; the worktree is gone.
    Discarded,
}

/// `threads.json`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Threads {
    version: u32,
    threads: Vec<Thread>,
}

/// A new thread for run `id` of `repo`, its first note's text `title`: its place, its branch
/// and the commit it starts from, `fork`. Nothing is written or made.
///
/// # Errors
///
/// [`Error::NoHome`] when `home` is `None`, and the git error when the branch `HEAD` is on
/// cannot be read.
pub(crate) fn plan(
    repo: &Repo,
    home: Option<&Path>,
    id: &str,
    fork: &str,
    title: &str,
) -> Result<Thread> {
    // Resolved, as git resolves the repository's root: an agent reports paths as the system
    // gives them (`/private/tmp` for `/tmp` on macOS), and Postil strips the worktree's path
    // from them.
    let home = resolved(home.ok_or(Error::NoHome)?);
    let folder = id.to_lowercase();
    let base = Some(repo.branch()?).filter(|branch| !branch.is_empty());
    Ok(Thread {
        id: id.to_owned(),
        path: worktrees_dir(repo.root(), &home).join(&folder),
        branch: branch_name(&folder, title),
        fork: fork.to_owned(),
        base,
        tip: fork.to_owned(),
        state: State::Open,
        skipped: Vec::new(),
    })
}

/// Makes `thread`'s worktree on its branch, checks out its submodules, and clones the
/// engineer's ignored top-level folders (`target`, `node_modules`) into it. Returns what was
/// not given to the worktree, each with the reason: a folder that could not be cloned or
/// copied, a Python virtual environment (it names its own absolute path, so a clone would
/// install into the engineer's), or submodules that could not be checked out. Takes as long
/// as a checkout, plus seconds per large folder; checking out submodules may fetch over the
/// network ([`Repo::update_submodules`]).
///
/// # Errors
///
/// [`Error::MakeWorktree`] when the folder above the worktree cannot be made, and the git
/// error when the worktree cannot be added.
pub(crate) fn make(repo: &Repo, thread: &Thread) -> Result<Vec<String>> {
    let parent = thread
        .path
        .parent()
        .expect("a worktree's path is <home>/worktrees/<repo>/<folder>");
    fs::create_dir_all(parent).map_err(|source| Error::MakeWorktree {
        path: parent.to_owned(),
        source,
    })?;
    repo.add_worktree(&thread.path, &thread.branch, &thread.fork)?;
    // The worktree is made from here on: what fails now is something it was not given, not a
    // failed make, which would drop the thread and leave the worktree with no owner.
    let tree = match Repo::open(&thread.path) {
        Ok(tree) => tree,
        Err(error) => {
            let why = error_chain(&error);
            return Ok(vec![format!("submodules and ignored folders: {why}")]);
        }
    };

    let mut skipped = Vec::new();
    if let Err(error) = tree.update_submodules() {
        skipped.push(format!("submodules: {}", error_chain(&error)));
    }
    let folders = repo.ignored_folders().unwrap_or_else(|error| {
        skipped.push(format!("ignored folders: {}", error_chain(&error)));
        Vec::new()
    });
    for name in folders {
        let from = repo.root().join(&name);
        let to = thread.path.join(&name);
        if from.join("pyvenv.cfg").exists() {
            skipped.push(format!("{name}: a Python virtual environment"));
        } else if to.symlink_metadata().is_ok() {
            // A checkout or a submodule made it: it is the worktree's own.
        } else if !tree.is_ignored(&format!("{name}/")) {
            // Ignored by a rule the engineer has not committed: the worktree's commit
            // would take the whole folder in.
            skipped.push(format!("{name}: not ignored by the committed .gitignore"));
        } else if let Err(error) = clone_or_copy(&from, &to) {
            skipped.push(format!("{name}: {error}"));
        }
    }
    Ok(skipped)
}

/// Clones the folder `from` to `to` in one call. When that fails for another reason than a
/// volume without clones, copies it with `cp -c -R`, which clones file by file. A volume
/// that cannot clone at all (another volume, HFS+, exFAT, a network share) is refused: a full
/// copy of a build folder on every send costs the disk this is meant to save.
fn clone_or_copy(from: &Path, to: &Path) -> io::Result<()> {
    match clone_folder(from, to) {
        Ok(()) => return Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::CrossesDevices
            ) =>
        {
            return Err(io::Error::new(
                error.kind(),
                "this volume cannot clone it, and a full copy would cost its size",
            ));
        }
        Err(_) => {}
    }
    // A failed clone can leave a part behind; `cp` would copy into it.
    drop(fs::remove_dir_all(to));
    let copied = Command::new("cp")
        .arg("-c")
        .arg("-R")
        .arg(from)
        .arg(to)
        .output()?;
    if copied.status.success() {
        return Ok(());
    }
    drop(fs::remove_dir_all(to));
    Err(io::Error::other(
        String::from_utf8_lossy(&copied.stderr).trim().to_owned(),
    ))
}

/// Every thread of `repo`, in the order they were added.
///
/// # Errors
///
/// [`Error::ReadThreads`] when the record cannot be read or parsed.
pub(crate) fn all(repo: &Repo) -> Result<Vec<Thread>> {
    Ok(read(&threads_file(repo))?.threads)
}

/// The thread `id` of `repo`, or `None` when the record has none (a run from before threads
/// had worktrees).
///
/// # Errors
///
/// As [`all`].
pub(crate) fn find(repo: &Repo, id: &str) -> Result<Option<Thread>> {
    Ok(all(repo)?.into_iter().find(|t| t.id == id))
}

/// Adds the new `thread` to the record.
///
/// # Errors
///
/// [`Error::ReadThreads`] when the record cannot be read or parsed, or has a thread of that
/// id already (a crash can give a deleted run's number again, and two entries of one id
/// would be removed together); [`Error::WriteThreads`] when it cannot be locked or written.
pub(crate) fn add(repo: &Repo, thread: &Thread) -> Result<()> {
    change(repo, |threads| {
        if threads.iter().any(|t| t.id == thread.id) {
            return Err(Error::ReadThreads {
                path: threads_file(repo),
                problem: format!(
                    "it has a thread {} already (remove that entry: its run log is gone)",
                    thread.id
                ),
            });
        }
        threads.push(thread.clone());
        Ok(())
    })
}

/// Changes the stored thread `id` with `edit`, under the record's lock, so a change made
/// meanwhile by another call or window to another field is kept. A thread the record no
/// longer has is left so.
///
/// # Errors
///
/// As [`add`].
pub(crate) fn update(repo: &Repo, id: &str, edit: impl FnOnce(&mut Thread)) -> Result<()> {
    change(repo, |threads| {
        if let Some(stored) = threads.iter_mut().find(|t| t.id == id) {
            edit(stored);
        }
        Ok(())
    })
}

/// Removes the threads whose ids are in `ids` from the record. Writes nothing when `ids` is
/// empty.
///
/// # Errors
///
/// As [`add`].
pub(crate) fn remove(repo: &Repo, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    change(repo, |threads| {
        threads.retain(|t| !ids.contains(&t.id));
        Ok(())
    })
}

/// Frees `thread`'s worktree: renames its folder into the trash beside the worktrees, drops
/// git's record of that one worktree, and deletes the folder on a background thread. A
/// worktree that is not on disk (never made, freed before, or deleted by hand) only has git's
/// record dropped, if git has one. Does not touch the branch or the thread's record.
///
/// The path comes from `threads.json`, which a hand edit can point anywhere, so a folder is
/// freed only when it is directly in this repository's worktrees folder under Postil's home
/// `home` ([`check_place`]), and the trash is found from that folder, never from the path.
/// And only when it holds nothing freeing would lose ([`check`]), so no caller can forget to
/// ask.
///
/// # Errors
///
/// [`Error::NotAWorktree`] for a folder anywhere else, or whose `.git` names no worktree
/// record of this repository; [`Error::ThreadChanged`] as [`check`]; [`Error::MakeWorktree`]
/// when the folder cannot be moved into the trash or git's record removed; and the git
/// errors.
pub(crate) fn free(repo: &Repo, home: &Path, thread: &Thread) -> Result<()> {
    // A unique name in the trash only: nothing is ordered by it.
    static FREED: AtomicU64 = AtomicU64::new(0);
    check_place(repo, home, thread)?;
    check(thread)?;
    let record = repo.worktree_record(&thread.path)?;
    if thread.path.symlink_metadata().is_ok() {
        let path = fs::canonicalize(&thread.path).unwrap_or_else(|_| thread.path.clone());
        let trash = trash_for(repo.root(), home);
        // From the folder's own name, which `check_place` checked, never from the record's
        // id: a hand-edited id could name a place outside the trash.
        let folder = path
            .file_name()
            .expect("check_place passed, so the path ends in a folder's name");
        let name = format!(
            "{}-{}-{}",
            folder.to_string_lossy(),
            std::process::id(),
            FREED.fetch_add(1, Ordering::Relaxed)
        );
        let moved = fs::create_dir_all(&trash).and_then(|()| fs::rename(&path, trash.join(&name)));
        moved.map_err(|source| Error::MakeWorktree { path, source })?;
        // Runs on: the delete takes seconds, and nothing waits for it.
        empty_trash(&trash);
    }
    if let Some(record) = record {
        fs::remove_dir_all(&record).map_err(|source| Error::MakeWorktree {
            path: record,
            source,
        })?;
    }
    Ok(())
}

/// Frees `thread`'s worktree ([`free`], with its checks) and deletes its branch while that
/// still names the thread's tip and no working tree has it checked out
/// ([`Repo::delete_branch_at`]): what goes with a thread's last note. A branch past the tip
/// stays, as the engineer's work, and only a branch under `postil/` is deleted at all, since
/// the name comes from the record, which a hand edit can change. Does not touch the record.
///
/// # Errors
///
/// As [`free`] and [`Repo::delete_branch_at`].
pub(crate) fn forget(repo: &Repo, home: &Path, thread: &Thread) -> Result<()> {
    free(repo, home, thread)?;
    if thread.branch.starts_with("postil/") {
        repo.delete_branch_at(&thread.branch, &thread.tip)?;
    }
    Ok(())
}

/// Checks that `thread`'s worktree holds nothing that ending the thread would lose: no change
/// git would commit (ignored files, such as the cloned build folders, do not count), the
/// thread's branch checked out, and that branch at the commit Postil left or past it. Past it
/// is a commit the engineer made there, as the refusal tells them to, and it is theirs to
/// keep: it is on the branch, which freeing keeps. Returns the commit the thread ends at: the
/// worktree's, or the recorded tip when the worktree is not on disk.
///
/// # Errors
///
/// [`Error::ThreadChanged`] naming what is there, and the git errors.
pub(crate) fn check(thread: &Thread) -> Result<String> {
    if !thread.path.is_dir() {
        return Ok(thread.tip.clone());
    }
    let tree = Repo::open(&thread.path)?;
    let changed = tree.changed()?;
    if !changed.is_empty() {
        return Err(Error::ThreadChanged {
            path: thread.path.clone(),
            found: format!("changed files {}", changed.join(", ")),
        });
    }
    // On the thread's branch: a commit made with `HEAD` detached, or on another branch the
    // worktree has checked out, is in no ref freeing keeps, and would be lost with it.
    let current = tree.branch()?;
    if current != thread.branch {
        let on = if current.is_empty() {
            "no branch (HEAD is detached)".to_owned()
        } else {
            format!("branch {current}")
        };
        return Err(Error::ThreadChanged {
            path: thread.path.clone(),
            found: format!("{on}, not the thread's {}", thread.branch),
        });
    }
    match tree.head()? {
        Some(head) if tree.is_ancestor(&thread.tip, &head)? => Ok(head),
        head => Err(Error::ThreadChanged {
            path: thread.path.clone(),
            found: format!(
                "the branch is at {}, which does not follow {}, where Postil left it",
                head.as_deref().unwrap_or("no commit"),
                thread.tip
            ),
        }),
    }
}

/// Checks that `thread`'s folder, on disk or not, is where Postil puts this repository's
/// worktrees, under Postil's home `home`: `<base>/worktrees/<repo>-<id>/<folder>`. Only there
/// does Postil free a folder, or git's record of one: the path comes from `threads.json`,
/// which a hand edit can point anywhere, a worktree of the engineer's on a disk not mounted
/// included. The `<id>` is not checked against the hash of [`worktrees_dir`]: a Rust release
/// may hash differently, and every thread made before would then fail this check.
///
/// # Errors
///
/// [`Error::NotAWorktree`] for a folder anywhere else.
pub(crate) fn check_place(repo: &Repo, home: &Path, thread: &Thread) -> Result<()> {
    let path = resolved(&thread.path);
    let worktrees = base_for(repo.root(), &resolved(home)).join("worktrees");
    let name = repo_name(repo.root());
    // `<repo>-` and eight hex digits, so `app` does not take `app-web-1a2b3c4d`.
    let named = |folder: &std::ffi::OsStr| {
        folder
            .to_str()
            .and_then(|folder| folder.strip_prefix(&format!("{name}-")))
            .is_some_and(|id| id.len() == 8 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
    };
    let ours = path.parent().is_some_and(|parent| {
        parent.parent() == Some(worktrees.as_path()) && parent.file_name().is_some_and(named)
    });
    if ours {
        Ok(())
    } else {
        Err(Error::NotAWorktree { path })
    }
}

/// Deletes everything in the trash folder `trash` ([`empty`]) on a background thread: freed
/// worktrees can hold hundreds of thousands of files. A delete cut short by a quit is
/// finished by the next call.
pub(crate) fn empty_trash(trash: &Path) {
    let trash = trash.to_owned();
    let spawned = std::thread::Builder::new()
        .name("empty-trash".to_owned())
        .spawn(move || empty(&trash));
    // No thread: the folder stays until the next call, which is no harm.
    drop(spawned);
}

/// Deletes everything in the folder `trash`, keeping the folder. Errors are ignored: what is
/// left is tried again at the next call.
fn empty(trash: &Path) {
    let Ok(entries) = fs::read_dir(trash) else {
        return;
    };
    for entry in entries.flatten() {
        drop(fs::remove_dir_all(entry.path()));
    }
}

/// The trash folder where [`free`] moves the worktrees of the repository at `root`, given
/// Postil's home `home`: `trash/` beside `worktrees/`, on the same volume, so the move is a
/// rename.
pub(crate) fn trash_for(root: &Path, home: &Path) -> PathBuf {
    base_for(root, &resolved(home)).join("trash")
}

/// The folder that holds Postil's folders for the repository at `root`: Postil's home `home`
/// when both are on one volume, else `.postil` at the root of the repository's volume, so
/// clones stay on one volume. A repository that is its volume's root uses the home folder all
/// the same: a folder there would be inside the repository.
fn base_for(root: &Path, home: &Path) -> PathBuf {
    let volume = volume_root(root);
    if device(home) == device(root) || volume == root {
        home.to_owned()
    } else {
        volume.join(".postil")
    }
}

/// The folder that holds the worktrees of the repository at `root`, given Postil's home
/// folder `home`, resolved.
fn worktrees_dir(root: &Path, home: &Path) -> PathBuf {
    let base = base_for(root, home);
    let name = repo_name(root);
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "eight hex digits tell repositories of one name apart; the rest is not needed"
    )]
    let id = hasher.finish() as u32;
    base.join("worktrees").join(format!("{name}-{id:08x}"))
}

/// The name that starts the folder of the repository at `root`'s worktrees: its folder's.
fn repo_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || "repository".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// `path` with its symbolic links resolved: its nearest ancestor that exists, resolved, and
/// the rest of `path` after it. Postil's home may not be made yet.
fn resolved(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(real) = ancestor.canonicalize() {
            let rest = path
                .strip_prefix(ancestor)
                .expect("an ancestor of `path` is a prefix of it");
            return real.join(rest);
        }
    }
    path.to_owned()
}

/// The device of `path`, or of its nearest ancestor that exists: Postil's home may not be
/// made yet.
fn device(path: &Path) -> Option<u64> {
    path.ancestors()
        .find_map(|ancestor| fs::metadata(ancestor).ok())
        .map(|metadata| metadata.dev())
}

/// The topmost ancestor of `path` on the same device: its volume's mount point.
fn volume_root(path: &Path) -> PathBuf {
    let own = device(path);
    path.ancestors()
        .take_while(|ancestor| device(ancestor) == own)
        .last()
        .unwrap_or(path)
        .to_owned()
}

/// The branch for thread folder `folder` (`r3`), named from `title`: `postil/r3-` and up to
/// [`SLUG_LENGTH`] lowercase letters, digits and dashes from it, or `postil/r3` when it has
/// none. Always a valid branch name: it holds no character `git check-ref-format` refuses.
fn branch_name(folder: &str, title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars().map(|c| c.to_ascii_lowercase()) {
        if slug.len() == SLUG_LENGTH {
            break;
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        format!("postil/{folder}")
    } else {
        format!("postil/{folder}-{slug}")
    }
}

/// `threads.json` of `repo`.
fn threads_file(repo: &Repo) -> PathBuf {
    repo.runs_dir().with_file_name("threads.json")
}

/// Reads the record at `file`: no file is no threads.
fn read(file: &Path) -> Result<Threads> {
    let problem = |problem: String| Error::ReadThreads {
        path: file.to_owned(),
        problem,
    };
    let json = match fs::read(file) {
        Ok(json) => json,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Threads {
                version: THREADS_VERSION,
                threads: Vec::new(),
            });
        }
        Err(error) => return Err(problem(error.to_string())),
    };
    match version_of(&json) {
        Ok(THREADS_VERSION) => serde_json::from_slice(&json).map_err(|e| problem(e.to_string())),
        Ok(version) => Err(problem(format!(
            "version {version}, and this Postil reads version {THREADS_VERSION}"
        ))),
        Err(error) => Err(problem(error.to_string())),
    }
}

/// Reads the record, lets `update` change its threads, and writes it, under the lock. An
/// `update` that fails writes nothing.
fn change(repo: &Repo, update: impl FnOnce(&mut Vec<Thread>) -> Result<()>) -> Result<()> {
    let file = threads_file(repo);
    let lock_path = file.with_extension("lock");
    let error = |source| Error::WriteThreads {
        path: lock_path.clone(),
        source,
    };
    let dir = file
        .parent()
        .expect("threads.json is in <git dir>/postil, which has a parent");
    fs::create_dir_all(dir).map_err(error)?;
    let lock = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(error)?;
    // Held for one read and one write, milliseconds, so a wait for another window's change
    // is that short. Released when `lock` drops, after the write.
    lock.lock().map_err(error)?;
    let mut threads = read(&file)?;
    update(&mut threads.threads)?;
    save_json(&file, &threads).map_err(|source| Error::WriteThreads { path: file, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn a_repository_on_another_volume_keeps_its_folders_at_that_volume_s_root() {
        let dir = TempDir::new("thread-places");
        let home = resolved(&dir.path().join(".postil"));
        let on_home = dir.path().join("repo");
        assert_eq!(
            base_for(&on_home, &home),
            home,
            "one volume: the home folder"
        );
        // `/dev` is its own file system (devfs) on macOS and Linux, apart from the home
        // folder's, and a repository below it is on that volume.
        let elsewhere = Path::new("/dev/fd");
        assert_ne!(device(elsewhere), device(&home));
        assert_eq!(base_for(elsewhere, &home), Path::new("/dev/.postil"));
        // A repository that is its volume's root: the folder would be inside it.
        assert_eq!(base_for(Path::new("/dev"), &home), home);
        assert_eq!(trash_for(elsewhere, &home), Path::new("/dev/.postil/trash"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_errors_of_a_volume_that_cannot_clone_read_as_the_kinds_clone_or_copy_refuses() {
        // `clone_or_copy` refuses a full copy by these kinds, and std does not document the
        // mapping from errno: ENOTSUP and EXDEV on macOS.
        assert_eq!(
            io::Error::from_raw_os_error(45).kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            io::Error::from_raw_os_error(18).kind(),
            io::ErrorKind::CrossesDevices
        );
    }

    #[test]
    fn emptying_the_trash_deletes_everything_in_it_and_keeps_the_folder() {
        let dir = TempDir::new("thread-trash");
        let trash = dir.path().join("trash");
        fs::create_dir_all(trash.join("r1-1-0/target/debug")).unwrap();
        fs::write(trash.join("r1-1-0/target/debug/app"), "built\n").unwrap();
        fs::create_dir_all(trash.join("r2-1-1")).unwrap();

        empty(&trash);

        assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
    }

    #[test]
    fn a_branch_takes_a_clean_slug_from_the_note_and_none_when_nothing_is_left() {
        assert_eq!(
            branch_name("r3", "Fix the parser: it drops `..` paths!"),
            "postil/r3-fix-the-parser-it-drops-paths"
        );
        assert_eq!(branch_name("r12", "  ~^:?*[\\ "), "postil/r12");
        let long = branch_name("r1", &"word ".repeat(20));
        assert!(long.len() <= "postil/r1-".len() + SLUG_LENGTH, "{long}");
        assert!(!long.ends_with('-'), "{long}");
    }
}
