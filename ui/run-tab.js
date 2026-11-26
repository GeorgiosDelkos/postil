// The tray's Run tab, its header, and the agent status at the foot of the sidebar. The tab
// lists the runs, one small card each, newest first; a card opens the run's dialog
// (run-dialog.js). What needs an answer now shows above the runs.

import { agentLogoIcon, el, statusIcon } from "./dom.js";
import {
  elapsed,
  evidence,
  foldRun,
  iconKind,
  isLive,
  keyFor,
  openQuestion,
  permissionKeys,
  statusLine,
  statusWord,
  stopKey,
  worstEvidence,
} from "./run.js";
import { latestOfThreads, reportOf } from "./status.js";

// The mark a run's card shows for the worst of its latest turn's checks, and its tooltip.
const CHECK_MARKS = { failed: "×", "not-seen": "?", stale: "~", unfinished: "…", passed: "✓" };
const CHECK_TITLES = {
  failed: "A check the agent claims failed in the log",
  "not-seen": "A check the agent claims is not in the log",
  stale: "A check passed, then a file was edited after it",
  unfinished: "A check the agent claims had not ended",
  passed: "Every check the agent claims passed in the log",
};

// The worst state of the checks `turn`'s report claims (`worstEvidence`), or null when it
// claims none. The latest turn's: a later turn's checks are what the work stands on now.
function checksOf(turn) {
  const report = reportOf(turn)?.report;
  return report ? worstEvidence(evidence(report, foldRun(turn.entries).items)) : null;
}

// `store` holds the runs (run-store.js) and `dialog` is the run dialog (run-dialog.js).
// `panelEl` is the Run tab's body, `headerEl` the tray header's run part, `statusEl` the
// sidebar's agent status.
export function runTab({ store, dialog, panelEl, headerEl, statusEl }) {
  let clock = null;
  // The tray header's nodes, `{ key, title, stop }`, built by `renderHeader`.
  let header = null;
  // When Stop starts to end the agent, by this page's clock; null unless it is stopping.
  let forceAt = null;
  // The drawn cards by key, each with the signature it was drawn from: a card whose
  // signature is unchanged keeps its node.
  let views = new Map();
  let drawnRun = null;

  // The header's Stop, and Escape: stops the latest run, the one the header shows, never one
  // it does not (another thread's stops from its dialog). It asks the agent to stop. Once the
  // Rust process would force it (forceAt), Stop ends it (src/run.rs, "Stopping"); sooner, the
  // Rust process takes it as the same request.
  function stop() {
    const run = store.run();
    if (!isLive(run)) return Promise.resolve();
    if (run.status === "cancelling" && !forceable()) return Promise.resolve();
    return store.stop(run.id);
  }

  function forceable() {
    return forceAt !== null && Date.now() >= forceAt;
  }

  function render() {
    const run = store.run();
    if (drawnRun !== (run?.id ?? null)) {
      drawnRun = run?.id ?? null;
      forceAt = null;
    }
    renderStatus();
    renderHeader();
    const ids = store.ids();
    const list = ids.length ? ids : run ? [run.id] : [];
    if (list.length === 0) {
      views = new Map();
      const empty = "No run yet. Send notes to an agent to start one.";
      panelEl.replaceChildren(el("div", "empty", empty));
      dialog.closeIfGone(list);
      return;
    }
    const wanted = [];
    // What needs an answer now shows above the runs, for each live run: a question, as a card
    // that opens the run's dialog where it is answered, and a permission request Postil could
    // not answer.
    for (const live of store.liveRuns()) {
      const items = foldRun(live.entries).items;
      const question = openQuestion(items);
      if (question) {
        const key = `question:${live.id}:${question.request}`;
        wanted.push([key, question.message, () => needsYouCard(live, question)]);
      }
      for (const item of items) {
        if (item.type !== "permission" || item.answer !== undefined) continue;
        const key = `permission:${live.id}:${item.request}`;
        wanted.push([key, item.title, () => permissionView(live, item)]);
      }
    }
    // Reviewers' runs show inside the run they review, and follow-ups inside the run they
    // follow, not here.
    const shown = list.filter((id) => !store.byId(id)?.review && !store.byId(id)?.followUp);
    const label = shown.length === 1 ? "Run" : `Runs · ${shown.length}`;
    wanted.push(["label", label, () => el("div", "run-label", label)]);
    const latests = latestOfThreads(store.knownViews());
    for (const id of shown) {
      const view = store.byId(id);
      const latest = view ? (latests.get(id) ?? view) : null;
      const checks = view ? checksOf(latest) : null;
      const sig = view
        ? JSON.stringify([
            latest.id,
            latest.status,
            view.agent,
            view.notes,
            statusWord(latest),
            checks,
          ])
        : (store.failure(id) ?? "loading");
      wanted.push([`run:${id}`, sig, () => runCard(id, view, latest, checks)]);
    }

    const next = new Map();
    const nodes = wanted.map(([key, sig, build]) => {
      const drawn = views.get(key);
      const node = drawn?.sig === sig ? drawn.node : build();
      next.set(key, { sig, node });
      return node;
    });
    views = next;
    // Only nodes out of place move.
    nodes.forEach((node, index) => {
      if (panelEl.childNodes[index] !== node) {
        panelEl.insertBefore(node, panelEl.childNodes[index] ?? null);
      }
    });
    while (panelEl.childNodes.length > nodes.length) panelEl.lastChild.remove();
    dialog.closeIfGone(list);
    dialog.draw();
  }

  // The card of a question waiting for the engineer: it opens the run's dialog.
  function needsYouCard(view, question) {
    const button = el("button", "run-card needs");
    button.type = "button";
    button.append(statusIcon("needs"));
    const logo = agentLogoIcon(view.agent);
    if (logo) button.append(logo);
    button.append(
      el("span", "run-card-text", `${view.agent} has a question: ${question.message}`),
      el("span", "run-id", view.id),
      el("span", "run-chevron", "›"),
    );
    button.addEventListener("click", () => dialog.open({ kind: "run", id: view.id }));
    return button;
  }

  // A run's card: its status, id, agent and notes, in one line. The status is its thread's
  // latest run's, `latest`: the run itself, or its last follow-up. `checks` is `latest`'s
  // worst claimed check (`checksOf`), or null.
  function runCard(id, view, latest, checks) {
    const button = el("button", "run-card run-entry");
    button.type = "button";
    if (!view) {
      button.disabled = true;
      const problem = store.failure(id);
      button.append(statusIcon(problem ? "failed" : "stopped"), el("span", "run-id", id));
      const text = problem ? `Cannot be shown: ${problem}` : "Loading…";
      button.append(el("span", "run-card-text", text));
      if (problem) button.title = problem;
      return button;
    }
    const more = latest.id === view.id ? "" : ` · +${latest.id}`;
    button.append(statusIcon(iconKind(latest)), el("span", "run-id", view.id));
    const logo = agentLogoIcon(view.agent);
    if (logo) button.append(logo);
    button.append(el("span", "run-card-text", `${view.agent} · ${view.notes.join(" ")}${more}`));
    if (checks) {
      const mark = el("span", `run-evidence tool-state ${checks}`, CHECK_MARKS[checks]);
      mark.title = CHECK_TITLES[checks];
      button.append(mark);
    }
    button.append(
      el("span", `run-entry-status tone-${statusLine(latest).tone}`, statusWord(latest)),
      el("span", "run-chevron", "›"),
    );
    button.title = `${latest.id} · ${view.agent} · ${statusLine(latest).text}`;
    button.addEventListener("click", () => dialog.open({ kind: "run", id }));
    return button;
  }

  // The sidebar's agent status. It is a button (it shows the Run tab), so its children change
  // only when what they say changes, not on every update of the run.
  function renderStatus() {
    const run = store.run();
    statusEl.hidden = !run;
    if (!run) return;
    const { text, tone } = statusLine(run);
    const shown = JSON.stringify([run.agent, text, tone]);
    if (statusEl.dataset.shown === shown) return;
    statusEl.dataset.shown = shown;
    const dot = el("span", `status-dot tone-${tone}`);
    dot.setAttribute("aria-hidden", "true");
    statusEl.replaceChildren(dot, el("span", "status-text", `${run.agent} · ${text}`));
  }

  // The tray header: the latest run's status and, while it runs, its time and Stop. Its
  // nodes are kept while `stopKey` stays the same and changed in place, so Stop takes a
  // click while the run streams.
  function renderHeader() {
    const run = store.run();
    clearInterval(clock);
    clock = null;
    if (!run) {
      header = null;
      headerEl.replaceChildren();
      return;
    }
    const key = stopKey(run);
    if (!header || header.key !== key) {
      header = { key, title: el("span", "run-title"), stop: key === null ? null : stopButton() };
      headerEl.replaceChildren(header.title, ...(header.stop ? [header.stop.button] : []));
    }
    const { text, tone } = statusLine(run);
    header.title.title = `${run.id} · ${run.agent} · ${text}`;
    const time = el("span", "run-time");
    // Reads the store on each tick, not `run`: a sync replaces the run before it draws again.
    const tick = () => {
      const now = store.run();
      if (isLive(now)) time.textContent = ` · ${elapsed(Date.now() / 1000 - now.startedAt)}`;
    };
    tick();
    const word = el("strong", `run-word tone-${tone}`, statusWord(run));
    header.title.replaceChildren(statusIcon(iconKind(run)), word, time);
    const stopping = isLive(run) && run.status === "cancelling";
    if (!stopping) forceAt = null;
    // The Rust process says how long until it would force the agent, so a reloaded page
    // offers Force stop as soon as the process would act on it.
    else forceAt ??= Date.now() + (run.forceIn ?? 0);
    const { stop: button } = header;
    if (!button) return;
    button.label();
    clock = setInterval(() => {
      tick();
      button.label();
    }, 1000);
  }

  // The header's Stop, for the live run: `label()` sets its word and whether it can be
  // pressed, in place and only when they differ, so the node under the pointer stays.
  function stopButton() {
    const button = el("button", "button stop");
    button.type = "button";
    const word = document.createTextNode("Stop");
    button.append(word, el("kbd", "", "esc"));
    button.addEventListener("click", stop);
    const label = () => {
      const stopping = store.run().status === "cancelling";
      const force = stopping && forceable();
      button.disabled = stopping && !force;
      const text = force ? "Force stop" : "Stop";
      if (word.data !== text) word.data = text;
    };
    return { button, label };
  }

  // Run `run`'s permission request `request`, with a button per option.
  function permissionView(run, request) {
    const card = el("div", "permission open");
    card.append(
      el("div", "permission-label", `Permission needed · ${run.id}`),
      el("div", "permission-title", request.title),
    );
    const keys = permissionKeys(request.options);
    const buttons = el("div", "permission-actions");
    for (const option of request.options) {
      const primary = option.kind === "allow_once";
      const button = el("button", primary ? "button primary" : "button");
      button.type = "button";
      button.append(option.name);
      const key = keyFor(keys, option.id);
      if (key) button.append(el("kbd", "", key));
      button.addEventListener("click", () => store.answer(run.id, request.request, option.id));
      buttons.append(button);
    }
    card.append(buttons);
    return card;
  }

  return { render, stop };
}
