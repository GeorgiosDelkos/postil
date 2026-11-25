//! Keeping the disk in bounds (docs/design.md, "Keeping the disk in bounds"): at most
//! [`MAX_WORKTREES`] worktrees per repository, and [`FLOOR`] bytes free on their volume. Both
//! are checked on a run's start thread, before its worktree is made and its agent starts, so
//! the seconds a delete takes keep no request waiting.
//!
//! Room is made by evicting idle threads, the one whose latest run is oldest first: a thread
//! with no run in any window, not busy in this one, and nothing in its worktree that Postil did
//! not commit. Its kept agent is stopped and its worktree freed; its branch and record stay,
//! and its next run makes the worktree again from the branch. Short of space, a thread first
//! loses its build folders only, and its worktree only if that is not enough; over the cap,
//! whole worktrees go.
//!
//! An idle thread is taken under its lock, as an ending is (module docs of `run.rs`, "Threads
//! and locks"): the lock first, then the slots. A launch in this window or another that comes
//! meanwhile cannot take the lock and is refused, as one that comes during an ending is; a
//! review whose next stage comes then stops, saying why.

use std::fmt;
use std::fs::File;
use std::path::Path;

use crate::error::{Error, Result};
#[cfg(test)]
use crate::run::lock;
use crate::run::log::hold_thread_lock;
use crate::run::{Held, Runner, Slot, Worktree};
use crate::thread::{self, Delete, State, Thread};

/// The most worktrees Postil keeps for one repository: Codex's default (Cursor keeps 20). One
/// Codex report found 61 worktrees holding about 100 GB.
pub(crate) const MAX_WORKTREES: usize = 15;

/// The bytes Postil keeps free on the worktrees' volume: 20 GB. Arbitrary: a few release builds
/// of a Tauri app.
pub(crate) const FLOOR: u64 = 20_000_000_000;

/// The bounds a test sets in place of the real ones: the disk a test runs on has hundreds of GB
/// free, and fifteen worktrees take long to make.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestDisk {
    /// The most worktrees, in place of [`MAX_WORKTREES`].
    pub(crate) max_worktrees: Option<usize>,
    /// Measures the free space in place of the volume.
    pub(crate) free_space: Option<Box<dyn Fn() -> u64 + Send>>,
}

#[cfg(test)]
impl fmt::Debug for TestDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestDisk")
            .field("max_worktrees", &self.max_worktrees)
            .field(
                "free_space",
                &self.free_space.as_ref().map(|measure| measure()),
            )
            .finish()
    }
}

/// An idle thread taken for eviction, read again under its lock, which is held until this is
/// dropped: no run starts in it meanwhile.
struct Idle {
    thread: Thread,
    _lock: File,
}

impl fmt::Debug for Idle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Idle")
            .field("thread", &self.thread.id)
            .finish_non_exhaustive()
    }
}

impl Runner {
    /// Makes room on the disk for a run of thread `own`, whose worktree `worktree` says is to
    /// be made, made again, or made already: evicts idle threads (module docs) until the
    /// repository's worktrees, `own`'s counted, fit [`MAX_WORKTREES`], and the volume has
    /// [`FLOOR`] free. Space that cannot be measured counts as enough. Runs on the run's start
    /// thread: freeing deletes before it returns, which takes seconds for a large build folder.
    ///
    /// # Errors
    ///
    /// [`Error::TooManyWorktrees`] and [`Error::DiskFull`] when evicting every idle thread is
    /// not enough, naming the threads that could not be evicted; as [`thread::all`].
    pub(super) fn free_disk(&self, own: &str, worktree: &Worktree) -> Result<()> {
        let Some(home) = self.home.as_deref() else {
            return Ok(());
        };
        let mut others: Vec<Thread> = thread::all(&self.repo)?
            .into_iter()
            .filter(|t| t.id != own && t.state == State::Open && t.path.is_dir())
            .collect();
        // Oldest run first; a thread from version 1 of the record, with no time, before all.
        others.sort_by_key(|t| t.last_run);
        let cap = self.max_worktrees();
        // A run in a worktree on disk adds none.
        let mut over = match worktree {
            Worktree::Make | Worktree::Remake { .. } => (others.len() + 1).saturating_sub(cap),
            Worktree::Made => 0,
        };
        let short = || self.free_space(home).is_some_and(|free| free < FLOOR);
        let mut kept = Vec::new();
        for other in others {
            if over == 0 && !short() {
                return Ok(());
            }
            let Some(idle) = self.take_idle(home, &other) else {
                kept.push(other.id);
                continue;
            };
            if over == 0 {
                // Short of space only: its build folders first, most of a worktree's size,
                // which a build makes again.
                drop(thread::drop_build_folders(&self.repo, home, &idle.thread));
                if !short() {
                    return Ok(());
                }
            }
            match self.evict(home, &idle.thread) {
                Ok(()) => over = over.saturating_sub(1),
                Err(_) => kept.push(idle.thread.id),
            }
        }
        if over > 0 {
            return Err(Error::TooManyWorktrees { cap, kept });
        }
        match self.free_space(home) {
            Some(free) if free < FLOOR => Err(Error::DiskFull {
                free,
                floor: FLOOR,
                kept,
            }),
            _ => Ok(()),
        }
    }

    /// Thread `other`, taken for eviction when it is idle: its lock is free (no run in any
    /// window), it is not busy in this window (a review between two stages holds no lock), and
    /// its worktree holds nothing freeing would lose ([`thread::check`]). Read again under the
    /// lock: another window may have ended, evicted or forgotten it since it was listed.
    fn take_idle(&self, home: &Path, other: &Thread) -> Option<Idle> {
        let lock = hold_thread_lock(&self.repo.runs_dir(), &other.id).ok()?;
        if self
            .slots()
            .by_thread
            .get(&other.id)
            .is_some_and(Slot::busy)
        {
            return None;
        }
        let thread = thread::find(&self.repo, &other.id).ok().flatten()?;
        if thread.state != State::Open || !thread.path.is_dir() {
            return None;
        }
        thread::check(&self.repo, home, &thread).ok()?;
        Some(Idle {
            thread,
            _lock: lock,
        })
    }

    /// Stops the agent this window keeps in idle `thread`, whose working directory is its
    /// worktree, frees the worktree before returning, and marks the thread evicted. Freed
    /// first: a thread marked evicted whose worktree is still there could not be made again.
    ///
    /// # Errors
    ///
    /// As [`thread::free`], the thread then left open, and as [`thread::update`], the thread
    /// then open with its worktree gone, which its next run says.
    fn evict(&self, home: &Path, thread: &Thread) -> Result<()> {
        let released = self
            .slots()
            .by_thread
            .remove(&thread.id)
            .map_or(Held::None, |mut slot| slot.release());
        // Dropping the agent waits for its process, so after the slots' lock.
        drop(released);
        thread::free(&self.repo, home, thread, Delete::Now)?;
        thread::update(&self.repo, &thread.id, |stored| {
            stored.state = State::Evicted;
        })
    }

    /// [`MAX_WORKTREES`], or what a test set.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "a test sets the cap on the runner")
    )]
    fn max_worktrees(&self) -> usize {
        #[cfg(test)]
        if let Some(cap) = lock(&self.test_disk).max_worktrees {
            return cap;
        }
        MAX_WORKTREES
    }

    /// The free space on the worktrees' volume ([`thread::available_space`]), or what a test
    /// measures.
    fn free_space(&self, home: &Path) -> Option<u64> {
        #[cfg(test)]
        if let Some(measure) = &lock(&self.test_disk).free_space {
            return Some(measure());
        }
        thread::available_space(&self.repo, home)
    }

    /// Sets the bounds the next runs keep, in place of the real ones.
    #[cfg(test)]
    pub(crate) fn set_test_disk(&self, disk: TestDisk) {
        *lock(&self.test_disk) = disk;
    }
}
