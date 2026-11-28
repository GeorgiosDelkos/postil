// The runs the page has read, shared by the Run tab (run-tab.js), the run dialog
// (run-dialog.js) and the notes (`byId`).
//
// The store follows the latest run, and this window's other live runs: threads run side by
// side (src/run.rs). The Rust process calls `window.postil.runChanged()` whenever one changes,
// and `sync` fetches the entries it has not seen. Fetches never overlap: a change that arrives
// during one is fetched right after it. Older runs are read once each, by id, and kept: a
// finished run does not change.

import { appendEntries, isLive } from "./run.js";

// The most runs live at once in one repository (src/run/log.rs, `MAX_LIVE`): a send beyond
// them is refused, so the page offers none.
export const MAX_LIVE = 3;

// `render()` is called after anything the store holds changed; `onChange(run, previous,
// { liveEnded })` after every fetch that changed the latest run, `liveEnded` whether a live run
// other than the latest ended in it (an analysis, or a run that frees one of the 3 places);
// `onLoaded()` after an older run was read; `onError(text)` with the reason a request failed.
export function runStore({ render, onChange, onLoaded, onError }) {
  // The latest run, followed as it changes.
  let run = null;
  // This window's live runs other than the latest, by id, followed as they change; and the
  // thread of each of its live runs, by run id, read with the list (`GET api/runs`).
  const liveViews = new Map();
  let liveThreads = new Map();
  let fetching = false;
  let again = false;
  // How long to wait before fetching again after a failed fetch: doubled each failure.
  let retryIn = 1000;
  // The one retry waiting, or null.
  let retryTimer = null;
  // Every run's id, newest first (`api/runs`), read again when the latest run changes or a
  // run may have been deleted (`reload`).
  let ids = [];
  let listStale = true;
  // Older runs by id (LRU-capped so long sessions do not retain every historical log in RAM);
  // the ids asked for and not answered yet; and why a run could not be read, by id.
  const MAX_OTHERS = 40;
  const others = new Map();
  const asked = new Set();
  const failed = new Map();
  let notifyScheduled = false;
  let notifyLoaded = false;
  function scheduleNotify(loaded) {
    if (loaded) notifyLoaded = true;
    if (notifyScheduled) return;
    notifyScheduled = true;
    queueMicrotask(() => {
      notifyScheduled = false;
      const hadLoaded = notifyLoaded;
      notifyLoaded = false;
      render();
      if (hadLoaded) onLoaded();
    });
  }
  // Coalesces rapid intra-run streaming `sync()` UI notifications into one animation frame in the
  // browser, while flushing run-ID/status transitions and `liveEnded` synchronously (and keeping
  // synchronous delivery in Node tests where `requestAnimationFrame` is absent).
  let syncRaf = null;
  let syncToken = 0;
  let hasPendingSync = false;
  let pendingPrevious = undefined;
  let pendingLiveEnded = false;
  function notifySync(previous, liveEnded) {
    const coalescable =
      typeof requestAnimationFrame === "function" &&
      previous?.id === run?.id &&
      previous?.status === run?.status &&
      !liveEnded;
    if (!coalescable) {
      syncToken += 1;
      if (syncRaf !== null) {
        if (typeof cancelAnimationFrame === "function") cancelAnimationFrame(syncRaf);
        syncRaf = null;
      }
      const prev = hasPendingSync ? pendingPrevious : previous;
      const ended = pendingLiveEnded || liveEnded;
      pendingPrevious = undefined;
      pendingLiveEnded = false;
      hasPendingSync = false;
      render();
      onChange(run, prev, { liveEnded: ended });
      return;
    }
    if (!hasPendingSync) {
      pendingPrevious = previous;
      hasPendingSync = true;
    }
    if (syncRaf !== null) return;
    const token = ++syncToken;
    syncRaf = requestAnimationFrame(() => {
      if (token !== syncToken) return;
      syncRaf = null;
      const prev = pendingPrevious;
      const ended = pendingLiveEnded;
      pendingPrevious = undefined;
      pendingLiveEnded = false;
      hasPendingSync = false;
      render();
      onChange(run, prev, { liveEnded: ended });
    });
  }
  function rememberOther(id, view) {
    others.delete(id);
    others.set(id, view);
    if (others.size > MAX_OTHERS) {
      const oldest = others.keys().next().value;
      others.delete(oldest);
      asked.delete(oldest);
    }
  }
  // Threads by their first run's id (`api/thread`): the worktree, branch and state, or null
  // for a run from before threads had worktrees. Read once each, like older runs, and read
  // again after a thread ends here or the page refreshes.
  const threads = new Map();

  async function fetchRun() {
    // Only the entries not seen yet, unless the run is another one: then all of them.
    const after = run?.total ?? 0;
    const response = await fetch(`api/run?after=${after}`);
    if (!response.ok) throw new Error(await response.text());
    const reply = await response.json();
    if (reply === null) return null;
    if (run && reply.id === run.id) {
      // A reply from a later entry leaves out the notes and the prompt, which never change.
      const entries = appendEntries(run.entries, reply.entries);
      return { ...reply, notes: run.notes, prompt: run.prompt, entries };
    }
    if (after === 0) return reply;
    const whole = await fetch("api/run");
    if (!whole.ok) throw new Error(await whole.text());
    return whole.json();
  }

  async function fetchIds() {
    // Cleared before the request: a `reload` while it is in flight marks the list stale
    // again, and the next sync reads it again.
    listStale = false;
    const response = await fetch("api/runs");
    if (!response.ok) {
      listStale = true;
      throw new Error(await response.text());
    }
    const reply = await response.json();
    ids = reply.runs;
    const valid = new Set(ids);
    for (const id of others.keys()) {
      if (!valid.has(id)) {
        others.delete(id);
        asked.delete(id);
      }
    }
    for (const id of failed.keys()) {
      if (!valid.has(id)) {
        failed.delete(id);
        asked.delete(id);
      }
    }
    liveThreads = new Map((reply.live ?? []).map((live) => [live.id, live.thread]));
  }

  // Live run `id` again, with only the entries after those of `known`, its view so far.
  async function fetchLive(id, known) {
    const after = known?.total ?? 0;
    const response = await fetch(`api/run?id=${encodeURIComponent(id)}&after=${after}`);
    if (!response.ok) throw new Error(await response.text());
    const reply = await response.json();
    if (!known) return reply;
    const entries = appendEntries(known.entries, reply.entries);
    return { ...reply, notes: known.notes, prompt: known.prompt, entries };
  }

  // Follows the live runs other than the latest, and resolves to whether one of them ended. One
  // that ended is kept with the older runs, final, and the list is read again: its thread
  // changed, and it is live no more.
  async function followLive(previous) {
    let ended = false;
    // The run the latest replaced may still run: a send started another thread.
    if (previous && previous.id !== run?.id && isLive(previous)) {
      liveViews.set(previous.id, previous);
    }
    // Fetch all other live runs concurrently instead of sequentially.
    const liveIds = [...new Set([...liveThreads.keys(), ...liveViews.keys()])].filter(
      (id) => id !== run?.id,
    );
    const fetched = await Promise.all(
      liveIds.map(async (id) => [id, await fetchLive(id, liveViews.get(id))]),
    );
    for (const [id, view] of fetched) {
      if (isLive(view)) {
        liveViews.set(id, view);
        continue;
      }
      liveViews.delete(id);
      rememberOther(id, view);
      asked.add(id);
      threads.clear();
      listStale = true;
      ended = true;
    }
    for (const id of liveViews.keys()) {
      if (id === run?.id || !liveThreads.has(id)) liveViews.delete(id);
    }
    return ended;
  }

  async function sync() {
    if (fetching) {
      again = true;
      return;
    }
    fetching = true;
    // This fetch is the retry: one waiting would start a second chain of retries beside it.
    clearTimeout(retryTimer);
    retryTimer = null;
    try {
      do {
        again = false;
        const previous = run;
        const next = await fetchRun();
        // With no latest run there is nothing to read, unless a live run of another thread is
        // followed: an analysis is never the latest run (src/run.rs, `Runner::analyze`).
        if (next === null && previous === null && !listStale && liveThreads.size === 0) continue;
        run = next;
        if (listStale || previous?.id !== run?.id || previous?.status !== run?.status) {
          await fetchIds();
        }
        const liveEnded = await followLive(previous);
        // A run that started or ended changed its thread: the worktree was made (`skipped`),
        // its tip moved, or a run elsewhere ended it.
        if (previous?.id !== run?.id || previous?.status !== run?.status) threads.clear();
        // A pull request being opened or read is done when the server nudges: read again.
        for (const [id, record] of threads) {
          const work = record?.pullRequestWork;
          if (work && work.kind !== "failed") threads.delete(id);
        }
        notifySync(previous, liveEnded);
      } while (again);
      retryIn = 1000;
    } catch (error) {
      onError(String(error.message ?? error));
      // The nudge behind the failed fetch may have been the run's last, so the fetch is
      // tried again, less often each time it fails.
      retryTimer = setTimeout(sync, retryIn);
      retryIn = Math.min(retryIn * 2, 30000);
    } finally {
      fetching = false;
    }
  }

  // The run `id`: the latest, or an older one once read; null until then, and a read is
  // started. Each id is asked for once until `forget` or `reload`: asking again on every draw
  // would fetch it in a loop.
  function byId(id) {
    if (run?.id === id) return run;
    if (liveViews.has(id)) return liveViews.get(id);
    const known = others.get(id);
    if (known) {
      // Refresh LRU position on hit.
      others.delete(id);
      others.set(id, known);
      return known;
    }
    if (asked.has(id)) return null;
    asked.add(id);
    fetch(`api/run?id=${encodeURIComponent(id)}`)
      .then(async (response) => {
        if (!response.ok) throw new Error(await response.text());
        rememberOther(id, await response.json());
        scheduleNotify(true);
      })
      // The run's card says why, and its notes say "Sent in R2": the tray stays usable.
      .catch((error) => {
        failed.set(id, String(error.message ?? error));
        scheduleNotify(false);
      });
    return null;
  }

  // Drops the older runs that may still change, and reads the list again: the page's
  // refresh, when another window may have moved on. Only an ended or failed run is final; a
  // run another window is running reads from its log as unfinished.
  function forget() {
    for (const [id, view] of others) {
      if (view.status === "ended" || view.status === "failed") continue;
      others.delete(id);
      asked.delete(id);
    }
    for (const id of failed.keys()) asked.delete(id);
    failed.clear();
    threads.clear();
    listStale = true;
  }

  // Drops every run read, after a note's delete may have deleted runs, and fetches again.
  function reload() {
    others.clear();
    asked.clear();
    failed.clear();
    threads.clear();
    listStale = true;
    return sync();
  }

  // The thread whose first run is `id`, once read: undefined until then (a read is started),
  // null when the run has none. Asked for once until it changes here or the page refreshes.
  function thread(id) {
    if (threads.has(id)) return threads.get(id);
    threads.set(id, undefined);
    fetch(`api/thread?id=${encodeURIComponent(id)}`)
      .then(async (response) => {
        if (!response.ok) throw new Error(await response.text());
        threads.set(id, await response.json());
        render();
      })
      .catch((error) => {
        // Asked again at the next draw that needs it.
        threads.delete(id);
        onError(String(error.message ?? error));
      });
    return undefined;
  }

  // Ends thread `id`: `"apply"` writes its changes into the working tree, `"discard"` drops
  // them; either frees its worktree. Resolves to whether it ended.
  function endThread(id, ending) {
    return post(`api/thread/${ending}`, { thread: id }).then(
      () => {
        threads.delete(id);
        render();
        return true;
      },
      (error) => {
        onError(error.message);
        return false;
      },
    );
  }

  // Starts opening thread `id`'s pull request: the branch is pushed and a draft opened after
  // the reply, which the thread's record then shows. Resolves to whether it started.
  function openPullRequest(id) {
    return post("api/thread/pr", { thread: id }).then(
      () => {
        threads.delete(id);
        render();
        return true;
      },
      (error) => {
        onError(error.message);
        return false;
      },
    );
  }

  // Asks GitHub again for the state of every pull request not merged of a thread this window
  // has read: the window regained focus, after the engineer may have merged one, or reopened
  // one closed. A merged one stays merged. Never on a timer.
  function checkPullRequests() {
    for (const [id, record] of threads) {
      const state = record?.pullRequest?.state;
      if (!state || state === "merged") continue;
      post("api/thread/pr/state", { thread: id }).then(
        () => threads.delete(id),
        // A failure shows in the thread as `pullRequestWork`; nothing more to say here.
        () => {},
      );
    }
  }

  async function post(route, body) {
    const response = await fetch(route, { method: "POST", body: JSON.stringify(body ?? {}) });
    if (!response.ok) throw new Error(await response.text());
    await sync();
  }

  // Answers run `id`'s open question `request` with the form's values, or `null` to skip
  // it. Resolves to whether the answer was taken.
  function answerQuestion(id, request, content) {
    return post("api/run/question", { run: id, request, content }).then(
      () => true,
      (error) => {
        onError(error.message);
        return false;
      },
    );
  }

  // Answers run `id`'s permission request `request` (its number) with the option `option`.
  function answer(id, request, option) {
    return post("api/run/permission", { run: id, request, option }).catch((error) =>
      onError(error.message),
    );
  }

  // Stops run `id`: asks its agent to stop, or ends it once the Rust process would force it
  // (src/run.rs, "Stopping").
  function stop(id) {
    return post("api/run/cancel", { run: id }).catch((error) => onError(error.message));
  }

  // A run started that never becomes the latest (a memory draft, src/run.rs `Runner::draft`):
  // the list is read again at once, so its thread shows busy before the run sends a nudge.
  function started() {
    listStale = true;
    return sync();
  }

  // This window's live runs, oldest first.
  function liveRuns() {
    const live = [...liveViews.values()];
    if (isLive(run)) live.push(run);
    return live.sort((a, b) => Number(a.id.slice(1)) - Number(b.id.slice(1)));
  }

  // Follows a run this page started outside the Run tab, an analysis (`POST api/analysis`):
  // it is in no list the store has read yet, so the list is read again.
  function track() {
    listStale = true;
    return sync();
  }

  // Starts the settings' reviewers on run `of`.
  function startReview(of) {
    return post("api/run/review", { of }).catch((error) => onError(error.message));
  }

  // Every run read so far, to find a run's reviews among them. Read-only: must not trigger
  // `byId` fetches (which would thrash `MAX_OTHERS` when a repository has > 40 runs).
  function knownViews() {
    const byIdMap = new Map();
    if (run) byIdMap.set(run.id, run);
    for (const [id, view] of liveViews) byIdMap.set(id, view);
    for (const [id, view] of others) if (!byIdMap.has(id)) byIdMap.set(id, view);
    return [...byIdMap.values()];
  }

  return {
    sync,
    byId,
    forget,
    reload,
    post,
    answerQuestion,
    answer,
    stop,
    liveRuns,
    track,
    // The thread live run `id` works in, by its first run's id; undefined for a run not live.
    liveThread: (id) => liveThreads.get(id),
    // Whether a run of the thread that starts at run `root` is under way in this window.
    // From the live runs followed, as the run dialog's `liveOf` does: `liveThreads` keeps an ended
    // run until the list is read again, which a run that is never the latest does not cause.
    threadLive: (root) => liveRuns().some((view) => liveThreads.get(view.id) === root),
    started,
    startReview,
    thread,
    endThread,
    openPullRequest,
    checkPullRequests,
    knownViews,
    run: () => run,
    ids: () => ids,
    // Why run `id` could not be read, or undefined.
    failure: (id) => failed.get(id),
  };
}
