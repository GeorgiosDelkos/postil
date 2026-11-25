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
//! [`Runner::pull_work`] is a lock of its own, held for a map lookup or insert only and never
//! with another of the runner's locks.

use std::ffi::OsStr;
use std::sync::Arc;

use serde::Serialize;

use crate::agents::find_program;
use crate::error::{Error, Result, error_chain};
use crate::github::{self, NewPullRequest};
use crate::run::{Runner, Slot, lock};
use crate::thread::{self, PullRequest, PullState, State, Thread};

/// The longest title a pull request takes from a note, in characters: GitHub shows about this
/// much in a list. Arbitrary.
const TITLE_LENGTH: usize = 72;

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

    /// Opens a draft pull request from thread `id`'s branch into the branch `HEAD` was on at
    /// send (module docs): checks what it can here, then pushes the branch and runs
    /// `gh pr create` on a thread of its own, which records the URL in the thread, or the
    /// failure in [`PullWork`], and tells the page.
    ///
    /// # Errors
    ///
    /// [`Error::NoHome`]; [`Error::TurnRunning`] while the thread is busy in this window;
    /// [`Error::BadRequest`] for no open or evicted thread `id`, one with a pull request
    /// already, or one being opened; [`Error::PullRequestRefused`] with the reason (module
    /// docs); as [`thread::check`], for work Postil did not commit; and the git errors.
    pub(crate) fn open_pull_request(self: &Arc<Self>, id: &str) -> Result<()> {
        let home = self.home.as_deref().ok_or(Error::NoHome)?;
        if self.slots().by_thread.get(id).is_some_and(Slot::busy) {
            return Err(Error::TurnRunning);
        }
        let thread = match thread::find(&self.repo, id)? {
            Some(thread) if matches!(thread.state, State::Open | State::Evicted) => thread,
            _ => return Err(Error::BadRequest(format!("there is no open thread {id}"))),
        };
        if let Some(pull) = &thread.pull_request {
            return Err(Error::BadRequest(format!(
                "{id} has a pull request already: {}",
                pull.url
            )));
        }
        let refused = Error::PullRequestRefused;
        let base = thread.base.clone().ok_or_else(|| {
            refused(
                "the thread started from a detached HEAD, so it has no branch to merge into"
                    .to_owned(),
            )
        })?;
        let remote = self
            .repo
            .remote_for(&base)?
            .ok_or_else(|| refused(format!("{base} tracks no remote, and there is no origin")))?;
        let on_remote = self
            .repo
            .remote_branch_commit(&remote, &base)?
            .ok_or_else(|| refused(format!("{base} is not on {remote}: push it first")))?;
        let unpushed = self.repo.commits_between(&on_remote, &thread.fork)?;
        if !unpushed.is_empty() {
            return Err(refused(format!(
                "the thread starts from commits of {base} that {remote} does not have, which \
                 the pull request would show ({}): push {base} first",
                unpushed.join("; ")
            )));
        }
        // Nothing left uncommitted, which the push would leave out.
        thread::check(&self.repo, home, &thread)?;
        let (title, body) = self.pull_request_text(id);
        if !self.start_pull_work(id, PullWork::Opening) {
            return Err(Error::BadRequest(format!(
                "the pull request of {id} is being opened or read"
            )));
        }
        let runner = Arc::clone(self);
        let id = id.to_owned();
        self.spawn_pull_work(&id.clone(), move |search_path| {
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
            thread::update(&runner.repo, &id, |stored| {
                stored.pull_request = Some(PullRequest {
                    url,
                    state: PullState::Open,
                });
            })
        });
        Ok(())
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
        let id = id.to_owned();
        self.spawn_pull_work(&id.clone(), move |search_path| {
            let program = find_program("gh", search_path).ok_or(Error::NoGh)?;
            let state = github::state(&program, runner.repo.root(), search_path, &pull.url)?;
            thread::update(&runner.repo, &id, |stored| {
                if let Some(stored) = &mut stored.pull_request {
                    stored.state = state;
                }
            })
        });
        Ok(())
    }

    /// The title and body of thread `id`'s pull request: the first line of its first note
    /// still there, and the prompt its first run sent, which holds every note as the agent
    /// read it. A thread whose first run cannot be read gets a title naming it, and a body
    /// saying where it came from only.
    fn pull_request_text(&self, id: &str) -> (String, String) {
        let (notes, prompt) = self.run_by_id(id).map_or_else(
            |_| (Vec::new(), String::new()),
            |run| {
                let state = run.state();
                (state.header.notes.clone(), state.header.prompt.clone())
            },
        );
        let title = notes
            .iter()
            .find_map(|note| crate::notes::body(&self.repo, note).ok())
            .and_then(|body| body.lines().next().map(str::trim).map(str::to_owned))
            .filter(|line| !line.is_empty())
            .map_or_else(
                || format!("Postil thread {id}"),
                |line| line.chars().take(TITLE_LENGTH).collect(),
            );
        let body = format!("{prompt}\n\nOpened from Postil thread {id}.")
            .trim_start()
            .to_owned();
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
                command: "a thread for gh".to_owned(),
                message: format!("cannot start: {error}"),
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
