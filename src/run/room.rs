//! Keeping the disk in bounds (docs/design.md, "Keeping the disk in bounds"): at most
//! [`MAX_WORKTREES`] worktrees per repository, and [`FLOOR`] bytes free on their volume. Both
//! are checked on a run's start thread before a worktree is made, so the seconds a delete
//! takes keep no request waiting. Runs check apart, in one window or several: two making
//! worktrees at once can each see room for one, so the cap can be passed by the worktrees
//! being made at that moment, at most `MAX_LIVE - 1` (`log.rs`).
//!
//! Room is made by evicting idle threads, the one whose latest run is oldest first: a thread
//! with no run in any window, not busy in this one, and nothing in its worktree that Postil did
//! not commit. Short of space, a thread first loses its build folders, and its worktree only
//! if that is not enough; over the cap, whole worktrees go. An evicted thread's agent kept in
//! this window is stopped, and its worktree freed; its branch and record stay, and its next
//! run makes the worktree again from the branch. Another window's kept agent for it is not
//! reachable from here: that window starts a fresh agent for the thread's next run.
//!
//! An idle thread is taken under its lock, as an ending is (module docs of `run.rs`, "Threads
//! and locks"): the lock first, then the slots. A launch in this window or another that comes
//! meanwhile cannot take the lock and is refused, as one that comes during an ending is; a
//! review whose next stage comes then stops, saying why. The lock is held only while folders
//! are moved into the trash and the record changed, milliseconds: a held thread lock counts
//! as a live run for the cap of `MAX_LIVE`, so the delete, which takes seconds, comes after.

use std::fmt;
use std::fs::File;
use std::path::Path;

use crate::error::{Error, Result, error_chain};
#[cfg(test)]
use crate::run::lock;
use crate::run::log::try_hold_thread_lock;
use crate::run::{Held, Runner, Slot};
use crate::thread::{self, State, Thread, Trashed};

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
            .field("free_space", &self.free_space.is_some())
            .finish()
    }
}

/// What came of trying to take a thread for eviction ([`Runner::take_idle`]).
enum Taken {
    /// Idle, and held: its lock is held until this is dropped, so no run starts in it.
    Idle { thread: Thread, lock: File },
    /// No longer an open thread with a worktree on disk: another window ended, evicted or
    /// forgot it since it was listed. It no longer counts toward the cap.
    Gone,
    /// Not idle, or it could not be checked: kept, for this reason.
    Kept(String),
}

impl fmt::Debug for Taken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle { thread, .. } => f.debug_tuple("Idle").field(&thread.id).finish(),
            Self::Gone => f.write_str("Gone"),
            Self::Kept(why) => f.debug_tuple("Kept").field(why).finish(),
        }
    }
}

impl Runner {
    /// Makes room on the disk for the worktree of thread `own`, new or evicted, about to be
    /// made: evicts idle threads (module docs) until the repository's worktrees, `own`'s
    /// counted, fit [`MAX_WORKTREES`], and the volume has [`FLOOR`] free. Space that cannot be
    /// measured counts as enough. Runs on the run's start thread: what it frees is deleted
    /// before it is measured, which takes seconds for a large build folder.
    ///
    /// A run in a worktree already on disk makes no room: it adds no worktree, and a follow-up
    /// that reuses its thread's agent starts on the request thread, where no delete may wait.
    ///
    /// # Errors
    ///
    /// [`Error::TooManyWorktrees`] and [`Error::DiskFull`] when evicting every idle thread is
    /// not enough, naming each thread that was kept and why; as [`thread::all`].
    pub(super) fn free_disk(&self, own: &str) -> Result<()> {
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
        // The others' worktrees and `own`'s.
        let mut over = (others.len() + 1).saturating_sub(cap);
        let short = || self.free_space(home).is_some_and(|free| free < FLOOR);
        let mut kept = Vec::new();
        for other in others {
            if over == 0 && !short() {
                return Ok(());
            }
            // Short of space only: its build folders first, most of a worktree's size, which a
            // build makes again.
            if over == 0 {
                match self.take_idle(home, &other) {
                    Taken::Idle { thread, lock } => {
                        let trashed = thread::trash_build_folders(&self.repo, home, &thread);
                        drop(lock);
                        // An error here is met again by the eviction below, whose check names
                        // it as the reason the thread is kept.
                        trashed.into_iter().flatten().for_each(Trashed::delete_now);
                        if !short() {
                            return Ok(());
                        }
                    }
                    Taken::Gone => continue,
                    Taken::Kept(why) => {
                        kept.push(format!("{} ({why})", other.id));
                        continue;
                    }
                }
            }
            // Taken again: the lock was let go for the delete.
            match self.take_idle(home, &other) {
                Taken::Idle { thread, lock } => {
                    let evicted = self.evict(home, &thread);
                    drop(lock);
                    match evicted {
                        Ok(trashed) => {
                            if let Some(trashed) = trashed {
                                trashed.delete_now();
                            }
                            over = over.saturating_sub(1);
                        }
                        Err(error) => kept.push(format!("{} ({})", other.id, error_chain(&error))),
                    }
                }
                Taken::Gone => over = over.saturating_sub(1),
                Taken::Kept(why) => kept.push(format!("{} ({why})", other.id)),
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

    /// Thread `other`, taken for eviction when it is idle: not busy in this window (a review
    /// between two stages holds no lock), its lock free (no run in any window), and its
    /// worktree holding nothing freeing would lose ([`thread::check`]). Read again under the
    /// lock: another window may have ended, evicted or forgotten it since it was listed. This
    /// window's slots are asked before the lock too, which would otherwise keep a thread
    /// running here waiting out the lock's patience.
    fn take_idle(&self, home: &Path, other: &Thread) -> Taken {
        let busy_here = || {
            self.slots()
                .by_thread
                .get(&other.id)
                .is_some_and(Slot::busy)
        };
        if busy_here() {
            return Taken::Kept("at work in this window".to_owned());
        }
        let lock = match try_hold_thread_lock(&self.repo.runs_dir(), &other.id) {
            Ok(lock) => lock,
            // Another window's run, or another run of this window making room at once.
            Err(Error::RunElsewhere) => return Taken::Kept("at work in another run".to_owned()),
            Err(error) => return Taken::Kept(error_chain(&error)),
        };
        if busy_here() {
            return Taken::Kept("at work in this window".to_owned());
        }
        let thread = match thread::find(&self.repo, &other.id) {
            Ok(Some(thread)) if thread.state == State::Open && thread.path.is_dir() => thread,
            Ok(_) => return Taken::Gone,
            Err(error) => return Taken::Kept(error_chain(&error)),
        };
        if let Err(error) = thread::check(&self.repo, home, &thread) {
            return Taken::Kept(error_chain(&error));
        }
        Taken::Idle { thread, lock }
    }

    /// Stops the agent this window keeps in idle `thread`, whose working directory is its
    /// worktree, moves the worktree into the trash, drops git's record of it, and marks the
    /// thread evicted, all while the caller holds its lock. Returns the folder in the trash,
    /// for the caller to delete once the lock is let go. Marked only once freed: a thread
    /// marked evicted whose worktree is still there could not be made again.
    ///
    /// # Errors
    ///
    /// As [`thread::free`], the thread then left open, and as [`thread::update`], the thread
    /// then open with its worktree gone, which its next run says.
    fn evict(&self, home: &Path, thread: &Thread) -> Result<Option<Trashed>> {
        let released = self
            .slots()
            .by_thread
            .remove(&thread.id)
            .map_or(Held::None, |mut slot| slot.release());
        // Dropping the agent waits for its process, so after the slots' lock.
        drop(released);
        let trashed = thread::free(&self.repo, home, thread)?;
        thread::update(&self.repo, &thread.id, |stored| {
            stored.state = State::Evicted;
        })?;
        Ok(trashed)
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
