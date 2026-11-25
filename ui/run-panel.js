// The run panel: the tray's Run tab and the agent status at the foot of the sidebar
// (run-tab.js), the dialog a run's card opens (run-dialog.js), and the runs both draw
// (run-store.js). The tab lists the runs, one small card each, newest first; a card opens the
// run's dialog, which holds everything about it: its plan, its notes, every step, and the
// agent's reply (docs/design.md, "The Run tab"). ui/run.js has the rules.
//
// The page's notes read their runs through the same store (`byId`).

import { runDialog } from "./run-dialog.js";
import { MAX_LIVE, runStore } from "./run-store.js";
import { runTab } from "./run-tab.js";
import { foldRun, isLive, openPermission, permissionKey, permissionKeys } from "./run.js";

// `panelEl` is the Run tab's body, `headerEl` the tray header's run part, `statusEl` the
// sidebar's agent status, `detailEl` the `<dialog>` a card opens. `onChange(run, previous)` is
// called after every fetch that changed the latest run, so the page can follow it (switch
// tabs, re-read files); `onLoaded()` after an older run was read. `noteOf(id)` gives a note
// by id, or null once it is deleted; `openNote(id)` opens its detail; `openFile(path)` shows a
// file in the code pane; `reviewers()` names the settings' reviewers, in order;
// `showChanges(from, to)` shows what runs `from` to `to` changed in the code pane.
export function runPanel({
  panelEl,
  headerEl,
  statusEl,
  detailEl,
  onChange,
  onLoaded,
  onError,
  noteOf,
  openNote,
  openFile,
  reviewers,
  showChanges,
}) {
  // `tab` is built below: the store draws only after a fetch, never while it is built.
  const store = runStore({ render: () => tab.render(), onChange, onLoaded, onError });
  const dialog = runDialog({
    store,
    detailEl,
    noteOf,
    openNote,
    openFile,
    reviewers,
    showChanges,
    onError,
  });
  const tab = runTab({ store, dialog, panelEl, headerEl, statusEl });

  // `a`, Shift-`a` and `r` answer the open permission request (`permissionKey`). Returns
  // whether the key was used.
  // The request answered is the oldest live run's first open one, the one the Run tab shows
  // first.
  function keydown(event) {
    const key = permissionKey(event);
    if (!key) return false;
    for (const run of store.liveRuns()) {
      const request = openPermission(foldRun(run.entries).items);
      if (!request) continue;
      const option = permissionKeys(request.options).get(key);
      if (option === undefined) return false;
      event.preventDefault();
      store.answer(run.id, request.request, option);
      return true;
    }
    return false;
  }

  return {
    sync: store.sync,
    stop: tab.stop,
    keydown,
    byId: store.byId,
    knownViews: store.knownViews,
    startReview: store.startReview,
    answerQuestion: store.answerQuestion,
    holdDraft: dialog.holdDraft,
    forget: store.forget,
    checkPullRequests: store.checkPullRequests,
    reload: store.reload,
    run: store.run,
    liveRuns: store.liveRuns,
    // Whether the latest run, the one the header shows, is under way: Escape stops it.
    live: () => isLive(store.run()),
    // Whether this window runs as many as a repository may: a send would be refused.
    full: () => store.liveRuns().length >= MAX_LIVE,
    busyThread: dialog.busyThread,
  };
}
