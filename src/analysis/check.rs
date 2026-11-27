//! The forced update (docs/review-assist.md, "The forced update"; docs/review-assist-plan.md,
//! 8.7): on each page refresh, [`check_head`] compares `checked`, the commit the Architecture
//! was last compared with, with `HEAD`, and says whether an Update of the Architecture must
//! start.
//!
//! - `checked` is `HEAD`: nothing to do. This is the common case: one `git rev-parse` and a
//!   read of `analysis.json`, with no lock taken and nothing written.
//! - `checked` is gone from the repository, or is no ancestor of `HEAD` (a branch switch, a
//!   rebase, a reset): diverged. Nothing starts and nothing is written; the pane offers Update,
//!   whose start moves `checked` (`Plan::Run`'s `moves_checked`, or `Plan::MoveChecked`).
//! - `HEAD` moved forward: the paths `git diff` lists between the two commits. When none of
//!   them is a path, or below a folder, that an Architecture reference cites, and none is a
//!   file added that no reference covers, `checked` moves to `HEAD` with no run. Otherwise the
//!   Architecture is placed where its code is in `HEAD`; when a reference whose path changed
//!   is outdated there, or a file was added, an Update starts, else `checked` moves with no
//!   run (lines that only shifted).
//!
//! Only the references whose paths changed can start a run: after an update that failed at
//! one commit, its nodes stay outdated, and a later commit that touches none of them must not
//! start a second update for them.
//!
//! Everything is read from the two commits (`git diff`, `git ls-tree`, `git show`), never from
//! the working tree: an Update's worktree checks out `HEAD`, so uncommitted edits never force
//! one.
//!
//! # Two windows
//!
//! [`check_head`] takes no lock: it reads `analysis.json`, which every writer replaces by a
//! rename, so a read sees one whole version, and it runs its git commands, which take
//! milliseconds on a small repository and up to the time `git diff` of the move takes on a
//! large one, with nothing held. `analysis.lock` stays an innermost lock, held only to read and
//! write the file (`run.rs`, module docs). Every write of `checked` is [`claim_check`], a
//! compare and swap under [`lock`]: it moves `checked` only from the value the check read.
//! When no run is needed, the check claims at once, and answers [`Verdict::Same`] when the claim
//! finds `checked` moved meanwhile. When a run must start, the claim happens inside the run's
//! creation (`Runner::analyze_checked`), after the cap of 3 live runs is checked and before the
//! run's log exists. Of two windows that read the same `checked`, the first to claim starts the
//! one run; the other's claim finds `checked` moved, and it starts none, takes no run number
//! and makes no log. A start refused (the cap of 3, an Analyzer that cannot start) leaves
//! `checked` where it was, so the next check tries again; a run that started and then failed
//! or was stopped has moved it, so it is not started again for this commit.
//!
//! A file of version 1 or 2 has no `checked`: the fork of the newest top-level Architecture
//! diagram's run thread stands for it, or `HEAD` when no such thread is in the record (an
//! analysis written by hand), which moves `checked` there with no run.

use std::collections::HashSet;
use std::path::Path;

use super::{
    Analysis, CodeRef, CommitId, DiagramKind, Planned, RefDetail, VerifiedState, cited_paths,
    covered, folders_above, has_architecture, load, lock, place_at, prompt, save,
};
use crate::error::Result;
use crate::repo::{Change, ChangedPath, Repo};
use crate::thread::{self, Mode};

/// What [`check_head`] found.
#[derive(Debug)]
pub(crate) enum Verdict {
    /// `checked` is `HEAD`, or there is nothing to compare: no commit, no analysis, or no
    /// Architecture diagram in it; or another window moved `checked` meanwhile.
    Same,
    /// `checked` moved to `HEAD` with no run: nothing the Architecture cites changed.
    Untouched,
    /// `checked` is gone, or no ancestor of `HEAD`. Nothing was written.
    Diverged,
    /// An Update of the Architecture must start: `planned`, which moves `checked` from
    /// `expected` to `head` as it starts ([`claim_check`]). Nothing was written yet.
    Update {
        /// `checked` as read, which the claim compares with.
        expected: Option<CommitId>,
        head: CommitId,
        planned: Planned,
    },
}

/// Compares the analysis's `checked` with `HEAD` (module docs) and moves `checked` when no run
/// is needed. Takes no lock but [`claim_check`]'s, for the write. Reads the record of threads
/// when the file has no `checked`.
///
/// # Errors
///
/// As [`load`] and [`claim_check`]; as [`thread::all`]; the git errors of the commands above,
/// other than a `checked` git does not have.
pub(crate) fn check_head(repo: &Repo) -> Result<Verdict> {
    let Some(head) = CommitId::of_head(repo)? else {
        return Ok(Verdict::Same);
    };
    let Some(mut analysis) = load(&repo.analysis_file())? else {
        return Ok(Verdict::Same);
    };
    // `checked` is about the Architecture: with none, nothing is compared or written. The
    // next run that draws one sets it (`merge`).
    if !has_architecture(&analysis) || analysis.checked.as_ref() == Some(&head) {
        return Ok(Verdict::Same);
    }

    let expected = analysis.checked.clone();
    let baseline = match &expected {
        Some(checked) => Some(checked.clone()),
        None => fork_of_architecture(repo, &analysis)?,
    };
    let Some(baseline) = baseline else {
        return moved(repo, expected.as_ref(), &head);
    };
    let Some(from) = repo.resolve_commit(baseline.as_str())? else {
        return Ok(Verdict::Diverged);
    };
    if from == head.as_str() {
        // A file of version 2 learns the commit it was compared with.
        moved(repo, expected.as_ref(), &head)?;
        return Ok(Verdict::Same);
    }
    if !repo.is_ancestor(&from, head.as_str())? {
        return Ok(Verdict::Diverged);
    }

    let changes = repo.changed_paths(&from, head.as_str())?;
    let touched = touched_paths(&analysis, &changes);
    let added = added_off_the_map(&analysis, &changes);
    if touched.is_empty() && added.is_empty() {
        return moved(repo, expected.as_ref(), &head);
    }

    let files = repo.files_at(head.as_str())?;
    place_at(&mut analysis, repo, head.as_str(), &files);
    let broken = architecture_anchors(&analysis).any(|anchor| {
        touched.contains(anchor.path.as_str())
            && matches!(
                anchor.detail,
                RefDetail::Verified {
                    placed: VerifiedState::Outdated,
                    ..
                }
            )
    });
    if !broken && added.is_empty() {
        return moved(repo, expected.as_ref(), &head);
    }
    let planned = prompt::plan_architecture_update(repo, &analysis, &files, &added)?;
    Ok(Verdict::Update {
        expected,
        head,
        planned,
    })
}

/// Claims `checked` from `expected` to `head` with no run: [`Verdict::Untouched`], or
/// [`Verdict::Same`] when another window moved it meanwhile.
fn moved(repo: &Repo, expected: Option<&CommitId>, head: &CommitId) -> Result<Verdict> {
    Ok(if claim_check(repo, expected, head)? {
        Verdict::Untouched
    } else {
        Verdict::Same
    })
}

/// Moves `checked` from `expected` to `head` and returns `true`, when it is still `expected`
/// and the analysis still has an Architecture; else changes nothing and returns `false`: another
/// window claimed it first. Saves the file as stored, never as placed. Holds [`lock`] from the
/// read to the save, and takes no other lock; a forced update calls it under the slots and
/// `runs/active.lock` (`Runner::analyze_checked`), as an ask saves its note.
///
/// # Errors
///
/// As [`lock`], [`load`] and [`save`].
pub(crate) fn claim_check(
    repo: &Repo,
    expected: Option<&CommitId>,
    head: &CommitId,
) -> Result<bool> {
    let file = repo.analysis_file();
    let _lock = lock(&file)?;
    let Some(mut analysis) = load(&file)? else {
        return Ok(false);
    };
    if analysis.checked.as_ref() != expected || !has_architecture(&analysis) {
        return Ok(false);
    }
    store_checked(&file, &mut analysis, head)?;
    Ok(true)
}

/// Moves `checked` to `head`, whatever it was: an Update of every diagram the engineer
/// asked for starts there. Holds [`lock`] from the read to the save; does nothing when there is
/// no analysis.
///
/// # Errors
///
/// As [`lock`], [`load`] and [`save`].
pub(crate) fn move_checked(repo: &Repo, head: &CommitId) -> Result<()> {
    let file = repo.analysis_file();
    let _lock = lock(&file)?;
    if let Some(mut analysis) = load(&file)? {
        store_checked(&file, &mut analysis, head)?;
    }
    Ok(())
}

/// Whether `checked` is `head`: after a start refused for a reason another window's start
/// would also cause (the cap of 3), whether that window started the update. Takes no lock.
///
/// # Errors
///
/// As [`load`].
pub(crate) fn is_checked_at(repo: &Repo, head: &CommitId) -> Result<bool> {
    Ok(
        load(&repo.analysis_file())?
            .is_some_and(|analysis| analysis.checked.as_ref() == Some(head)),
    )
}

/// Sets `analysis.checked` to `head` and saves `analysis` to `file`, unless it is there
/// already. The caller holds [`lock`].
fn store_checked(file: &Path, analysis: &mut Analysis, head: &CommitId) -> Result<()> {
    if analysis.checked.as_ref() == Some(head) {
        return Ok(());
    }
    analysis.checked = Some(head.clone());
    save(file, analysis)
}

/// The commit a file with no `checked` was last compared with: the fork of the thread of the
/// newest top-level Architecture diagram's run, when that thread only read (an analysis run).
fn fork_of_architecture(repo: &Repo, analysis: &Analysis) -> Result<Option<CommitId>> {
    let mut tops = analysis
        .diagrams
        .iter()
        .filter(|diagram| diagram.kind == DiagramKind::Architecture && diagram.parent.is_none())
        .peekable();
    if tops.peek().is_none() {
        return Ok(None);
    }
    let threads = thread::all(repo)?;
    // Run numbers grow, so the highest is the newest.
    Ok(tops
        .filter_map(|diagram| {
            let run = diagram.run.to_string();
            let thread = threads
                .iter()
                .find(|thread| thread.id == run && thread.mode == Mode::Read)?;
            Some((diagram.run.0, &thread.fork))
        })
        .max_by_key(|(number, _)| *number)
        .and_then(|(_, fork)| CommitId::parse(fork)))
}

/// Every reference of the Architecture diagrams, of their nodes and their edges.
fn architecture_anchors(analysis: &Analysis) -> impl Iterator<Item = &CodeRef> {
    analysis
        .diagrams
        .iter()
        .filter(|diagram| diagram.kind == DiagramKind::Architecture)
        .flat_map(|diagram| {
            let nodes = diagram.nodes.iter().flat_map(|node| &node.anchors);
            nodes.chain(diagram.edges.iter().filter_map(|edge| edge.anchor.as_ref()))
        })
}

/// The paths of the verified Architecture references that `changes` touch: a changed path,
/// or a folder above one.
fn touched_paths(analysis: &Analysis, changes: &[ChangedPath]) -> HashSet<String> {
    let changed: HashSet<&str> = changes
        .iter()
        .map(|changed| changed.path.as_str())
        .collect();
    let folders = folders_above(changed.iter().copied());
    architecture_anchors(analysis)
        .filter(|anchor| matches!(anchor.detail, RefDetail::Verified { .. }))
        .map(|anchor| anchor.path.as_str())
        .filter(|path| changed.contains(path) || folders.contains(path))
        .map(str::to_owned)
        .collect()
}

/// The files `changes` adds that no verified reference covers, when there is a top-level
/// Architecture diagram, which places them; none otherwise.
fn added_off_the_map(analysis: &Analysis, changes: &[ChangedPath]) -> Vec<String> {
    let maps_files = analysis
        .diagrams
        .iter()
        .any(|diagram| diagram.kind == DiagramKind::Architecture && diagram.parent.is_none());
    if !maps_files {
        return Vec::new();
    }
    let cited = cited_paths(analysis);
    changes
        .iter()
        .filter(|changed| changed.change == Change::Added && !covered(&cited, &changed.path))
        .map(|changed| changed.path.clone())
        .collect()
}
