//! The models each agent offers, asked of the agent itself, for the model selects of the
//! settings dialog and the startup wizard (docs/design.md, "Agents").
//!
//! An agent's list changes with its releases, so it is never written down here: a probe
//! starts the agent, opens a session (`initialize`, `session/new`), reads the model select it
//! reports, and stops it. Probes run on threads of their own, since one takes seconds (Claude's
//! adapter starts through `npx`) and the thread that answers the page serves one request at a
//! time; the page asks again while a probe runs.
//!
//! # Invariants
//!
//! Reports are kept by command, so editing a custom agent's command probes it again, and an
//! agent the wizard has not saved yet can be probed. Each entry names the id of the latest
//! probe started for it, and only that probe may store its result or its [`Stopper`]. Ids are
//! never reused, so a probe that a rescan replaced cannot overwrite the newer result,
//! whichever finishes last.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
#[cfg(test)]
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::acp::{Agent, Launch, ModelOption, Stopper};
use crate::agents::Scanner;
use crate::error::error_chain;

/// How long each start-up step of a probe (`initialize`, `session/new`) may take. Arbitrary:
/// Claude's adapter took several seconds through `npx` on 2026-11-26, and a first `npx`
/// download takes longer; an agent waiting for a sign-in never answers.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a failed probe's report is served before the next query probes again. Arbitrary:
/// long enough that a dialog polling every 250 ms does not start an agent on each poll, short
/// enough that the list appears soon after the engineer signs in.
const FAILED_TTL: Duration = Duration::from_secs(5);

/// What is known of the models an agent offers.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum ModelReport {
    /// A probe runs.
    Loading,
    /// The agent's model select: the model in effect and the ones offered, in its order. No
    /// options means the agent offers no model choice.
    Ready {
        current: Option<String>,
        options: Vec<ModelOption>,
    },
    /// The probe failed; the text says why, as an error message does.
    Failed { error: String },
}

/// A probe: the agent's command and search path in, its report out. It hands the agent's
/// [`Stopper`] to the callback once the agent's process runs, so [`ModelProber::close`] can
/// stop it.
type ProbeFn = Arc<dyn Fn(&str, &OsStr, &dyn Fn(Stopper)) -> ModelReport + Send + Sync>;

/// Probes agents for their models in the background, and keeps what they reported (module
/// docs, "Invariants").
pub(crate) struct ModelProber {
    probe: ProbeFn,
    /// Where agents' programs are looked for; a probe waits for its scan.
    agents: Scanner,
    state: Arc<Mutex<State>>,
    /// How long a failed report is served. Always [`FAILED_TTL`] outside tests.
    failed_ttl: Duration,
    /// Every probe thread started, so a test can wait for the probes it started.
    #[cfg(test)]
    threads: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Debug, Default)]
struct State {
    /// The id the next probe gets.
    next_probe: u64,
    /// By the agent's command.
    entries: HashMap<String, Cached>,
}

#[derive(Debug)]
struct Cached {
    /// The id of the latest probe started for this entry.
    probe: u64,
    /// When the report was last stored.
    updated: Instant,
    /// The running probe's agent, from its start until its report is stored.
    stopper: Option<Stopper>,
    report: ModelReport,
}

impl ModelProber {
    /// A prober whose probes start real agents. Starts nothing until asked.
    pub(crate) fn live(agents: Scanner) -> Self {
        Self::new(agents, probe_models)
    }

    /// A prober whose probes call `probe`.
    fn new(
        agents: Scanner,
        probe: impl Fn(&str, &OsStr, &dyn Fn(Stopper)) -> ModelReport + Send + Sync + 'static,
    ) -> Self {
        Self {
            probe: Arc::new(probe),
            agents,
            state: Arc::new(Mutex::new(State::default())),
            failed_ttl: FAILED_TTL,
            #[cfg(test)]
            threads: Mutex::new(Vec::new()),
        }
    }

    /// The report for each of `commands`, in order. Starts a probe for a command never
    /// probed, or whose last probe failed more than [`FAILED_TTL`] ago; a probe in flight
    /// reports [`ModelReport::Loading`]. Does not wait for a probe or for the agent scan.
    pub(crate) fn query(&self, commands: &[&str]) -> Vec<ModelReport> {
        let mut to_start = Vec::new();
        let reports: Vec<ModelReport> = {
            let mut state = lock(&self.state);
            commands
                .iter()
                .map(|&command| {
                    let keep = state
                        .entries
                        .get(command)
                        .is_some_and(|entry| match entry.report {
                            ModelReport::Loading | ModelReport::Ready { .. } => true,
                            ModelReport::Failed { .. } => entry.updated.elapsed() < self.failed_ttl,
                        });
                    if keep {
                        return state.entries[command].report.clone();
                    }
                    let probe = state.next_probe;
                    state.next_probe += 1;
                    let entry = Cached {
                        probe,
                        updated: Instant::now(),
                        stopper: None,
                        report: ModelReport::Loading,
                    };
                    state.entries.insert(command.to_owned(), entry);
                    to_start.push((command.to_owned(), probe));
                    ModelReport::Loading
                })
                .collect()
        };
        for (command, probe) in &to_start {
            self.start(command, *probe);
        }
        reports
    }

    /// Forgets every report, so the next query probes again (an agent's list changes with
    /// an update), and stops the probes in flight: their results would be dropped. A probe
    /// whose agent has not started yet is stopped as it starts ([`keep_stopper`]).
    pub(crate) fn rescan(&self) {
        let entries = std::mem::take(&mut lock(&self.state).entries);
        stop_all(entries);
    }

    /// Stops every probe in flight, as [`ModelProber::rescan`] does. Waits only for each
    /// `kill`, not for the probes' threads. The site calls it as it closes, and asks nothing
    /// after.
    pub(crate) fn close(&self) {
        self.rescan();
    }

    /// Starts probe number `probe` of `command` on its own thread.
    fn start(&self, command: &str, probe: u64) {
        let probe_fn = Arc::clone(&self.probe);
        let agents = self.agents.clone();
        let shared = Arc::clone(&self.state);
        let key = command.to_owned();
        let run = move || {
            // Blocks until the login shell has said where programs are, a few seconds at most
            // and only once per app.
            let search_path = agents.wait();
            let started = |stopper: Stopper| keep_stopper(&shared, &key, probe, stopper);
            let report = probe_fn(&key, &search_path, &started);
            store(&shared, &key, probe, report);
        };
        let spawned = std::thread::Builder::new()
            .name("model-probe".to_owned())
            .spawn(run);
        match spawned {
            Ok(handle) => {
                #[cfg(test)]
                lock(&self.threads).push(handle);
                // Outside tests nothing waits for a probe: it reports through `store`.
                #[cfg(not(test))]
                drop(handle);
            }
            Err(error) => store(
                &self.state,
                command,
                probe,
                ModelReport::Failed {
                    error: format!(
                        "cannot start a thread to ask the agent for its models: {error}"
                    ),
                },
            ),
        }
    }

    /// Replaces how long a failed report is served, so a test can see it kept or expired
    /// without waiting.
    #[cfg(test)]
    fn with_failed_ttl(mut self, ttl: Duration) -> Self {
        self.failed_ttl = ttl;
        self
    }

    /// Waits until every probe thread started so far has stored or dropped its result.
    #[cfg(test)]
    pub(crate) fn join_probes(&self) {
        let threads = std::mem::take(&mut *lock(&self.threads));
        for handle in threads {
            handle.join().expect("a model probe thread panicked");
        }
    }
}

impl fmt::Debug for ModelProber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("ModelProber")
            .field("probes_started", &state.next_probe)
            .field("cached", &state.entries.len())
            .finish_non_exhaustive()
    }
}

/// Stops the agents of the probes in `entries` that still run.
fn stop_all(entries: HashMap<String, Cached>) {
    for entry in entries.into_values() {
        if let Some(stopper) = entry.stopper {
            stopper.stop();
        }
    }
}

/// Keeps `stopper` with `command`'s entry while probe number `probe` is the entry's latest,
/// so a close or rescan can stop its agent; stops it at once when a rescan or a close took
/// the entry, or a newer probe replaced this one, meanwhile.
fn keep_stopper(state: &Mutex<State>, command: &str, probe: u64, stopper: Stopper) {
    let unwanted = {
        let mut state = lock(state);
        let entry = state
            .entries
            .get_mut(command)
            .filter(|entry| entry.probe == probe);
        match entry {
            Some(entry) => {
                entry.stopper = Some(stopper);
                None
            }
            None => Some(stopper),
        }
    };
    // Stopped after the lock: it runs `/bin/kill`.
    if let Some(stopper) = unwanted {
        stopper.stop();
    }
}

/// Stores `report` as `command`'s when probe number `probe` is still the entry's latest, and
/// drops it otherwise.
fn store(state: &Mutex<State>, command: &str, probe: u64, report: ModelReport) {
    let mut state = lock(state);
    if let Some(entry) = state.entries.get_mut(command)
        && entry.probe == probe
    {
        entry.report = report;
        entry.updated = Instant::now();
        entry.stopper = None;
    }
}

/// Locks the prober's state. Poisoning is ignored: every change under the lock is a map
/// operation or an assignment, so a panic cannot leave an entry half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts the agent `command` in `$TMPDIR/postil-models`, opens a session, and reports the
/// model select it offers. The agent is stopped when the report is made, as dropping it does.
fn probe_models(command: &str, search_path: &OsStr, started: &dyn Fn(Stopper)) -> ModelReport {
    let failed = |error: String| ModelReport::Failed { error };
    // A folder of Postil's own outside any project, shared by every probe, so the probe's
    // session sees no project: nothing the agent reads or writes at start-up lands in one.
    let cwd = std::env::temp_dir().join("postil-models");
    if let Err(error) = fs::create_dir_all(&cwd) {
        return failed(format!("cannot make the folder {}: {error}", cwd.display()));
    }
    let launch = Launch {
        command,
        cwd: &cwd,
        search_path,
        timeout: PROBE_TIMEOUT,
    };
    // The session's events (an agent may announce its commands) are not needed.
    let starting = match Agent::spawn(&launch, drop) {
        Ok(starting) => starting,
        Err(error) => return failed(error_chain(&error)),
    };
    started(starting.stopper());
    match starting.open_session(None) {
        Ok(agent) => ModelReport::Ready {
            current: agent.model().map(str::to_owned),
            options: agent.offered_models().to_vec(),
        },
        Err(error) => failed(error_chain(&error)),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::mpsc;

    use super::*;

    fn ready(current: &str) -> ModelReport {
        ModelReport::Ready {
            current: Some(current.to_owned()),
            options: Vec::new(),
        }
    }

    fn failed() -> ModelReport {
        ModelReport::Failed {
            error: "not signed in".to_owned(),
        }
    }

    /// A prober whose probes report what `probe` returns for the command, and count calls.
    fn counting(
        probe: impl Fn(&str) -> ModelReport + Send + Sync + 'static,
    ) -> (ModelProber, Arc<Mutex<u32>>) {
        let calls = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&calls);
        let prober = ModelProber::new(Scanner::new(OsString::new), move |command, _, _| {
            *lock(&counter) += 1;
            probe(command)
        });
        (prober, calls)
    }

    #[test]
    fn a_ready_report_is_kept_across_queries_and_a_rescan_probes_again() {
        let (prober, calls) = counting(ready);
        assert_eq!(
            prober.query(&["a", "b"]),
            [ModelReport::Loading, ModelReport::Loading]
        );
        prober.join_probes();
        assert_eq!(prober.query(&["a", "b"]), [ready("a"), ready("b")]);
        prober.join_probes();
        assert_eq!(
            *lock(&calls),
            2,
            "a ready report is served, not probed again"
        );

        prober.rescan();
        assert_eq!(prober.query(&["a"]), [ModelReport::Loading]);
        prober.join_probes();
        assert_eq!(*lock(&calls), 3);
    }

    #[test]
    fn a_failed_report_is_served_until_it_expires() {
        let (kept, calls) = counting(|_| failed());
        kept.query(&["a"]);
        kept.join_probes();
        assert_eq!(kept.query(&["a"]), [failed()]);
        assert_eq!(*lock(&calls), 1, "within the time a failure is kept");

        let (expiring, calls) = counting(|_| failed());
        let expiring = expiring.with_failed_ttl(Duration::ZERO);
        expiring.query(&["a"]);
        expiring.join_probes();
        assert_eq!(expiring.query(&["a"]), [ModelReport::Loading]);
        expiring.join_probes();
        assert_eq!(*lock(&calls), 2, "an expired failure is probed again");
    }

    #[test]
    fn a_probe_replaced_by_a_rescan_does_not_overwrite_the_newer_result() {
        // The first probe to run says so on `entered` and waits for `release`, then reports
        // "old"; any later probe reports "new" at once.
        let (release, gate) = mpsc::channel::<()>();
        let (entered, first_running) = mpsc::channel::<()>();
        let gate = Mutex::new(Some((gate, entered)));
        let prober = ModelProber::new(Scanner::new(OsString::new), move |_, _, _| {
            // Taken in its own statement, so the lock is not held while the first probe waits.
            let first = lock(&gate).take();
            match first {
                Some((gate, entered)) => {
                    entered.send(()).unwrap();
                    gate.recv().expect("the test releases the first probe");
                    ready("old")
                }
                None => ready("new"),
            }
        });
        prober.query(&["a"]);
        first_running.recv().unwrap();
        let first = lock(&prober.threads).pop().unwrap();

        prober.rescan();
        prober.query(&["a"]);
        prober.join_probes();
        release.send(()).unwrap();
        first.join().unwrap();
        assert_eq!(prober.query(&["a"]), [ready("new")]);
    }

    #[test]
    fn close_stops_a_probe_whose_agent_does_not_answer() {
        // The fake agent in scenario "silent" never answers `initialize`, so only a stop
        // ends the probe before its timeout.
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-agent.mjs");
        let command = format!("node {} silent", script.display());
        let path = std::env::var_os("PATH").expect("PATH is set");
        let (started, agent_running) = mpsc::channel::<()>();
        let started = Mutex::new(started);
        // The probe's report, which `close` drops from the prober along with its entry.
        let (reported, report) = mpsc::channel::<ModelReport>();
        let reported = Mutex::new(reported);
        let prober = ModelProber::new(Scanner::new(move || path.clone()), move |c, p, keep| {
            let tell = |stopper: Stopper| {
                keep(stopper);
                lock(&started).send(()).unwrap();
            };
            let report = probe_models(c, p, &tell);
            lock(&reported).send(report.clone()).unwrap();
            report
        });

        prober.query(&[&command]);
        agent_running
            .recv_timeout(PROBE_TIMEOUT)
            .expect("the agent starts");
        prober.close();
        prober.join_probes();
        let ModelReport::Failed { error } = report.recv().unwrap() else {
            panic!("a stopped probe fails");
        };
        // Stopped, it exited: not "did not answer initialize within 30 s", its timeout.
        assert_eq!(error, "the agent exited without a message");
    }
}
