//! Pull requests from threads (docs/design.md, "Ending a thread"): Open pull request pushes a
//! thread's branch and opens a draft pull request with `gh`, and the page asks for its state
//! again when the window regains focus. Neither ends the thread.
//!
//! What can be checked without the network is checked on the request thread, and refused there
//! with the reason: a thread with no base branch, a base branch not on the remote, commits of
//! the base branch the remote lacks (the pull request would show them), work Postil did not
//! commit. The push and `gh` run on a thread of their own: a push runs the engineer's
//! `pre-push` hook, which can take minutes, and the one request thread answers the page's Stop
//! and questions meanwhile. What that thread is doing, or why it failed, is kept in this
//! window only ([`PullWork`]): it is not the thread's state, and a crash must not leave a
//! thread marked as opening for good. The pull request's URL and state go in the record.
//!
//! Opening holds the thread's lock (`threads/<id>.lock`) from its checks until `gh` is done,
//! as a run does: no run, ending, forgetting or eviction in any window changes the branch or
//! the record under it, and each is refused as for a run under way. The lock is taken as an
//! ending takes it: this window's slots are asked first, and nothing in this window takes the
//! lock of a thread that is not busy until this request is answered (`run.rs`, "Threads and
//! locks"). Reading the state takes no lock: it only writes the state it read.
//!
//! [`Runner::pull_work`] is a lock of its own, held for a map lookup or insert only and never
//! with another of the runner's locks.

use std::ffi::OsStr;
use std::fs::File;
use std::sync::Arc;

use serde::Serialize;

use crate::agents::find_program;
use crate::error::{Error, Result, error_chain};
use crate::github::{self, NewPullRequest};
use crate::notes;
use crate::run::log::hold_thread_lock;
use crate::run::{Runner, Slot, lock};
use crate::thread::{self, Mode, PullRequest, PullState, State, Thread};

/// The longest title a pull request takes from a note, in characters: GitHub shows about this
/// much in a list. Arbitrary.
const TITLE_LENGTH: usize = 72;

/// The longest body, in characters. GitHub refuses a description over 65,536; this leaves
/// room for the line after it.
const BODY_LENGTH: usize = 60_000;

/// How many of the base branch's commits the remote lacks a refusal lists. Arbitrary: enough
/// to recognise them.
const LISTED_COMMITS: usize = 10;

/// What this window is doing about a thread's pull request, or why it last failed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub(crate) enum PullWork {
    /// Pushing the branch and opening the pull request.
    Opening,
    /// Reading the pull request's state.
    Checking,
    /// The last of the two failed, saying why.
    Failed { error: String },
}

/// A thread's record with this window's [`PullWork`]: what `GET api/thread` answers.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadView {
    #[serde(flatten)]
    thread: Thread,
    pull_request_work: Option<PullWork>,
}

/// A pull request ready to be opened: what the checks found, for the job.
#[derive(Debug)]
struct Opening {
    thread: Thread,
    remote: String,
    /// The base branch's name on the remote.
    base: String,
    title: String,
    body: String,
    /// The thread's lock, held until the job ends.
    lock: File,
}

impl Runner {
    /// Thread `id`'s record, with what this window is doing about its pull request; `None` for
    /// a run from before threads had worktrees.
    ///
    /// # Errors
    ///
    /// As [`thread::find`].
    pub(crate) fn thread_view(&self, id: &str) -> Result<Option<ThreadView>> {
        let work = lock(&self.pull_work).get(id).cloned();
        Ok(thread::find(&self.repo, id)?.map(|thread| ThreadView {
            thread,
            pull_request_work: work,
        }))
    }

    /// Whether this window is opening a pull request: the project is busy until it is done.
    pub(crate) fn opening_pull_request(&self) -> bool {
        lock(&self.pull_work)
            .values()
            .any(|work| matches!(work, PullWork::Opening))
    }

    /// Opens a draft pull request from thread `id`'s branch into the branch `HEAD` was on at
    /// send (module docs): checks what it can here, then pushes the branch and runs
    /// `gh pr create` on a thread of its own, which records the URL in the thread, or the
    /// failure in [`PullWork`], and tells the page.
    ///
    /// # Errors
    ///
    /// [`Error::NoHome`]; [`Error::TurnRunning`] while the thread is busy in this window, and
    /// [`Error::RunElsewhere`] while another window holds its lock; [`Error::BadRequest`] for
    /// no open or evicted thread `id`, one that only reads, one with a pull request already, or
    /// one being opened; [`Error::PullRequestRefused`] with the reason (module docs); as
    /// [`thread::check`], for work Postil did not commit; and the git errors.
    pub(crate) fn open_pull_request(self: &Arc<Self>, id: &str) -> Result<()> {
        // Before the thread's lock, which an opening of this window holds: taking it again
        // would wait out its patience and blame another window.
        if lock(&self.pull_work)
            .get(id)
            .is_some_and(|work| !matches!(work, PullWork::Failed { .. }))
        {
            return Err(Error::BadRequest(format!(
                "the pull request of {id} is being opened or read"
            )));
        }
        let opening = self.check_opening(id)?;
        // Requests run one at a time on the request thread, and a job only clears its work or
        // marks it failed, so the work found absent above is still absent. Checked in every
        // build: breaking it would push and open a pull request twice.
        assert!(
            self.start_pull_work(id, PullWork::Opening),
            "no work under way for {id}, checked above"
        );
        let runner = Arc::clone(self);
        let thread_id = id.to_owned();
        self.spawn_pull_work(id, move |search_path| {
            let Opening {
                thread,
                remote,
                base,
                title,
                body,
                lock: _held,
            } = opening;
            let program = find_program("gh", search_path).ok_or(Error::NoGh)?;
            runner
                .repo
                .push_branch(&remote, &thread.branch, search_path)?;
            let new = NewPullRequest {
                base: &base,
                head: &thread.branch,
                title: &title,
                body: &body,
            };
            let url = github::create(&program, runner.repo.root(), search_path, &new)?;
            let recorded = thread::update(&runner.repo, &thread_id, |stored| {
                stored.pull_request = Some(PullRequest {
                    url: url.clone(),
                    state: PullState::Open,
                });
            });
            // Opened, so the next Open would find it: the engineer needs its URL now.
            recorded.map_err(|error| Error::GitHub {
                command: "gh pr create".to_owned(),
                message: format!(
                    "opened {url}, but it could not be recorded: {}",
                    error_chain(&error)
                ),
            })
        });
        Ok(())
    }

    /// The checks of [`Runner::open_pull_request`] that need no network, under the thread's
    /// lock, which the returned opening holds.
    fn check_opening(&self, id: &str) -> Result<Opening> {
        let home = self.home.as_deref().ok_or(Error::NoHome)?;
        if self.slots().by_thread.get(id).is_some_and(Slot::busy) {
            return Err(Error::TurnRunning);
        }
        let lock = hold_thread_lock(&self.repo.runs_dir(), id)?;
        let thread = match thread::find(&self.repo, id)? {
            Some(thread) if matches!(thread.state, State::Open | State::Evicted) => thread,
            _ => return Err(Error::BadRequest(format!("there is no open thread {id}"))),
        };
        if thread.mode == Mode::Read {
            return Err(Error::BadRequest(format!(
                "thread {id} only reads: it has no changes for a pull request"
            )));
        }
        if let Some(pull) = &thread.pull_request {
            return Err(Error::BadRequest(format!(
                "{id} has a pull request already: {}",
                pull.url
            )));
        }
        // The record is a file a hand edit can change: only Postil's branches are pushed.
        if !thread.branch.starts_with("postil/") {
            return Err(Error::PullRequestRefused(format!(
                "its branch {} is not one of Postil's (postil/...)",
                thread.branch
            )));
        }
        let local = thread.base.clone().ok_or_else(|| {
            Error::PullRequestRefused(
                "the thread started from a detached HEAD, so it has no branch to merge into"
                    .to_owned(),
            )
        })?;
        let upstream = self.repo.upstream(&local)?.ok_or_else(|| {
            Error::PullRequestRefused(format!("{local} tracks no remote, and there is no origin"))
        })?;
        let (remote, base) = (upstream.remote, upstream.branch);
        let on_remote = self
            .repo
            .remote_branch_commit(&remote, &base)?
            .ok_or_else(|| {
                Error::PullRequestRefused(format!("{base} is not on {remote}: push {local} first"))
            })?;
        let unpushed = self
            .repo
            .commits_between(&on_remote, &thread.fork, LISTED_COMMITS + 1)?;
        if !unpushed.is_empty() {
            let more = if unpushed.len() > LISTED_COMMITS {
                ", and more"
            } else {
                ""
            };
            let listed = &unpushed[..unpushed.len().min(LISTED_COMMITS)];
            return Err(Error::PullRequestRefused(format!(
                "the thread starts from commits of {local} that {remote} does not have, which \
                 the pull request would show ({}{more}): push {local} first",
                listed.join("; ")
            )));
        }
        // Nothing left uncommitted, which the push would leave out.
        thread::check(&self.repo, home, &thread)?;
        let (title, body) = self.pull_request_text(id);
        Ok(Opening {
            thread,
            remote,
            base,
            title,
            body,
            lock,
        })
    }

    /// Reads the state of thread `id`'s pull request again with `gh pr view`, on a thread of
    /// its own, and records it in the thread; does nothing while its pull request is being
    /// opened or read. The page asks for this when the window regains focus, never on a timer
    /// (docs/design.md, "Ending a thread").
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when the thread has no pull request; as [`thread::find`].
    pub(crate) fn check_pull_request(self: &Arc<Self>, id: &str) -> Result<()> {
        let pull = thread::find(&self.repo, id)?
            .and_then(|thread| thread.pull_request)
            .ok_or_else(|| Error::BadRequest(format!("{id} has no pull request")))?;
        if !self.start_pull_work(id, PullWork::Checking) {
            return Ok(());
        }
        let runner = Arc::clone(self);
        let thread_id = id.to_owned();
        self.spawn_pull_work(id, move |search_path| {
            let program = find_program("gh", search_path).ok_or(Error::NoGh)?;
            let state = github::state(&program, runner.repo.root(), search_path, &pull.url)?;
            thread::update(&runner.repo, &thread_id, |stored| {
                if let Some(stored) = &mut stored.pull_request {
                    stored.state = state;
                }
            })
        });
        Ok(())
    }

    /// The title and body of thread `id`'s pull request: the first line of its first note
    /// still there, and its notes still there ([`notes::listed`]), never the prompt, which
    /// quotes the code they are on. A thread whose first run or notes cannot be read gets a
    /// title naming it, and a body saying where it came from only.
    fn pull_request_text(&self, id: &str) -> (String, String) {
        let ids = self
            .header_by_id(id)
            .map_or_else(|_| Vec::new(), |header| header.notes);
        let title = ids
            .iter()
            .find_map(|note| notes::body(&self.repo, note).ok())
            .and_then(|body| body.lines().next().map(str::trim).map(str::to_owned))
            .filter(|line| !line.is_empty())
            .map_or_else(
                || format!("Postil thread {id}"),
                |line| line.chars().take(TITLE_LENGTH).collect(),
            );
        let listed = notes::listed(&self.repo, &ids).unwrap_or_default();
        let mut body: String = listed.chars().take(BODY_LENGTH).collect();
        if body.len() < listed.len() {
            body.push_str("\n\n(The notes go on; the rest is in Postil.)\n\n");
        }
        body.push_str(&format!("Opened from Postil thread {id}."));
        (title, body)
    }

    /// Marks `work` under way for thread `id` and tells the page, unless other work is under
    /// way there; returns whether it was marked. A failure before is replaced.
    fn start_pull_work(&self, id: &str, work: PullWork) -> bool {
        {
            let mut pulls = lock(&self.pull_work);
            if pulls
                .get(id)
                .is_some_and(|under_way| !matches!(under_way, PullWork::Failed { .. }))
            {
                return false;
            }
            pulls.insert(id.to_owned(), work);
        }
        (self.notify)();
        true
    }

    /// Runs `job` with the login shell's search path on a thread of its own, then clears
    /// thread `id`'s work, or records why it failed, and tells the page.
    fn spawn_pull_work(
        self: &Arc<Self>,
        id: &str,
        job: impl FnOnce(&OsStr) -> Result<()> + Send + 'static,
    ) {
        let runner = Arc::clone(self);
        let thread_id = id.to_owned();
        let spawned = std::thread::Builder::new()
            .name("pull-request".to_owned())
            .spawn(move || {
                // Blocks until the login shell has said where programs are, a few seconds at
                // most and only once per app.
                let search_path = runner.agents.wait();
                let done = job(&search_path);
                runner.finish_pull_work(&thread_id, done);
            });
        if let Err(error) = spawned {
            let error = Error::GitHub {
                command: "gh".to_owned(),
                message: format!("cannot start a thread to run it: {error}"),
            };
            self.finish_pull_work(id, Err(error));
        }
    }

    /// Ends thread `id`'s work as `done` says, and tells the page.
    fn finish_pull_work(&self, id: &str, done: Result<()>) {
        {
            let mut pulls = lock(&self.pull_work);
            match done {
                Ok(()) => drop(pulls.remove(id)),
                Err(error) => drop(pulls.insert(
                    id.to_owned(),
                    PullWork::Failed {
                        error: error_chain(&error),
                    },
                )),
            }
        }
        (self.notify)();
    }
}
