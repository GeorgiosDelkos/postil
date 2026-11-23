// The run panel: the tray's Run tab and the agent status at the foot of the sidebar. The tab
// lists the runs, one small card each, newest first; a card opens the run's dialog, which
// holds everything about it: its plan, its notes, every step, and the agent's reply
// (docs/design.md, "The Run tab"). ui/run.js has the rules.
//
// The panel follows the latest run: the Rust process calls `window.postil.runChanged()`
// whenever it changes, and the panel fetches the entries it has not seen. Fetches never
// overlap: a change that arrives during one is fetched right after it. Older runs are read
// once each, by id, and kept: a finished run does not change. The page's notes read their
// runs through the same store (`byId`).

import {
  elapsed,
  foldRun,
  isLive,
  keyFor,
  markdownBlocks,
  openPermission,
  openQuestion,
  partLines,
  permissionKey,
  permissionKeys,
  statusLine,
  toolRow,
  underWay,
} from "./run.js";
import { questionForm, statusIcon } from "./note-dialog.js";
import { noteStatus, planProgress } from "./status.js";

const capital = (word) => word.charAt(0).toUpperCase() + word.slice(1);

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// A run's status as a status icon's kind (note-dialog.js, `statusIcon`).
function iconKind(run) {
  const { tone } = statusLine(run);
  return { busy: "working", attention: "needs", done: "addressed", failed: "failed" }[tone] ?? "stopped";
}

// A run's status in a word or two: statusLine's text, short enough for a card.
function statusWord(run) {
  return run.status === "unfinished" ? "Unfinished" : statusLine(run).text;
}

// `panelEl` is the Run tab's body, `headerEl` the tray header's run part, `statusEl` the
// sidebar's agent status, `detailEl` the `<dialog>` a card opens. `onChange(run, previous)` is
// called after every fetch that changed the latest run, so the page can follow it (switch
// tabs, re-read files); `onLoaded()` after an older run was read. `noteOf(id)` gives a note
// by id, or null once it is deleted; `openNote(id)` opens its detail; `openFile(path)` shows a
// file in the code pane.
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
}) {
  // The latest run, followed as it changes.
  let run = null;
  let fetching = false;
  let again = false;
  // How long to wait before fetching again after a failed fetch: doubled each failure.
  let retryIn = 1000;
  // Every run's id, newest first (`api/runs`), read again when the latest run changes or a
  // run may have been deleted (`reload`).
  let ids = [];
  let listStale = true;
  // Older runs by id; the ids asked for and not answered yet; and why a run could not be read,
  // by id (an unreadable or newer log). None is asked again until `reload` or `forget`.
  const others = new Map();
  const asked = new Set();
  const failed = new Map();

  let clock = null;
  // When Stop starts to end the agent, by this page's clock; null unless it is stopping.
  let forceAt = null;
  // The drawn cards by key, each with the signature it was drawn from: a card whose
  // signature is unchanged keeps its node.
  let views = new Map();
  let drawnRun = null;

  // What the dialog shows while open: `{ kind: "run", id }` or `{ kind: "step", id, key }`;
  // and what it was drawn from, to draw it again only when that changes.
  let detail = null;
  let detailDrawn = null;
  // The open question's form, kept while it stays open: a redraw must not drop what the
  // engineer has chosen or typed.
  let asking = null;
  detailEl.addEventListener("close", () => {
    detail = null;
    detailDrawn = null;
    asking = null;
  });
  detailEl.addEventListener("keydown", (event) => {
    if (detail?.kind !== "step" || event.metaKey || event.altKey || event.ctrlKey) return;
    if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
      event.preventDefault();
      moveStep(event.key === "ArrowLeft" ? -1 : 1);
    }
  });

  async function fetchRun() {
    // Only the entries not seen yet, unless the run is another one: then all of them.
    const after = run?.total ?? 0;
    const response = await fetch(`api/run?after=${after}`);
    if (!response.ok) throw new Error(await response.text());
    const reply = await response.json();
    if (reply === null) return null;
    if (run && reply.id === run.id) return { ...reply, entries: run.entries.concat(reply.entries) };
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
    ids = (await response.json()).runs;
  }

  async function sync() {
    if (fetching) {
      again = true;
      return;
    }
    fetching = true;
    try {
      do {
        again = false;
        const previous = run;
        const next = await fetchRun();
        if (next === null && previous === null && !listStale) continue;
        run = next;
        if (listStale || previous?.id !== run?.id) await fetchIds();
        render();
        onChange(run, previous);
      } while (again);
      retryIn = 1000;
    } catch (error) {
      onError(String(error.message ?? error));
      // The nudge behind the failed fetch may have been the run's last, so the fetch is
      // tried again, less often each time it fails.
      setTimeout(sync, retryIn);
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
    const known = others.get(id);
    if (known || asked.has(id)) return known ?? null;
    asked.add(id);
    fetch(`api/run?id=${encodeURIComponent(id)}`)
      .then(async (response) => {
        if (!response.ok) throw new Error(await response.text());
        others.set(id, await response.json());
        render();
        onLoaded();
      })
      // The run's card says why, and its notes say "Sent in R2": the tray stays usable.
      .catch((error) => {
        failed.set(id, String(error.message ?? error));
        render();
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
    listStale = true;
  }

  // Drops every run read, after a note's delete may have deleted runs, and fetches again.
  function reload() {
    others.clear();
    asked.clear();
    failed.clear();
    listStale = true;
    return sync();
  }

  async function post(route, body) {
    const response = await fetch(route, { method: "POST", body: JSON.stringify(body ?? {}) });
    if (!response.ok) throw new Error(await response.text());
    await sync();
  }

  // Stop asks the agent to stop. Once the Rust process would force it (forceAt), Stop ends it
  // (src/run.rs, "Stopping"); sooner, the Rust process takes it as the same request.
  function stop() {
    if (!isLive(run)) return Promise.resolve();
    if (run.status === "cancelling" && !forceable()) return Promise.resolve();
    return post("api/run/cancel").catch((error) => onError(error.message));
  }

  function forceable() {
    return forceAt !== null && Date.now() >= forceAt;
  }

  // Answers the run's open question with the form's values, or `null` to skip it. Resolves
  // to whether the answer was taken.
  function answerQuestion(request, content) {
    return post("api/run/question", { request, content }).then(
      () => true,
      (error) => {
        onError(error.message);
        return false;
      },
    );
  }

  function answer(request, option) {
    return post("api/run/permission", { request: request.request, option }).catch((error) =>
      onError(error.message),
    );
  }

  // `a`, Shift-`a` and `r` answer the open permission request (`permissionKey`). Returns
  // whether the key was used.
  function keydown(event) {
    const key = permissionKey(event);
    if (!key) return false;
    const request = run && openPermission(foldRun(run.entries).items);
    if (!request) return false;
    const option = permissionKeys(request.options).get(key);
    if (option === undefined) return false;
    event.preventDefault();
    answer(request, option);
    return true;
  }

  // ---- The tab: one card per run ----------------------------------------------------------

  function render() {
    if (drawnRun !== (run?.id ?? null)) {
      drawnRun = run?.id ?? null;
      forceAt = null;
    }
    renderStatus();
    renderHeader();
    const list = ids.length ? ids : run ? [run.id] : [];
    if (list.length === 0) {
      views = new Map();
      panelEl.replaceChildren(el("div", "empty", "No run yet. Send notes to an agent to start one."));
      closeIfGone(list);
      return;
    }
    const wanted = [];
    // What needs an answer now shows above the runs: a question, as a card that opens the
    // run's dialog where it is answered, and a permission request Postil could not answer.
    if (isLive(run)) {
      const items = foldRun(run.entries).items;
      const question = openQuestion(items);
      if (question) {
        const key = `question:${run.id}:${question.request}`;
        wanted.push([key, question.message, () => needsYouCard(run, question)]);
      }
      for (const item of items) {
        if (item.type !== "permission" || item.answer !== undefined) continue;
        wanted.push([`permission:${item.request}`, item.title, () => permissionView(item)]);
      }
    }
    const label = list.length === 1 ? "Run" : `Runs · ${list.length}`;
    wanted.push(["label", label, () => el("div", "run-label", label)]);
    for (const id of list) {
      const view = byId(id);
      const sig = view
        ? JSON.stringify([view.status, view.agent, view.notes, statusWord(view)])
        : (failed.get(id) ?? "loading");
      wanted.push([`run:${id}`, sig, () => runCard(id, view)]);
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
      if (panelEl.childNodes[index] !== node) panelEl.insertBefore(node, panelEl.childNodes[index] ?? null);
    });
    while (panelEl.childNodes.length > nodes.length) panelEl.lastChild.remove();
    closeIfGone(list);
    drawDetail();
  }

  // The card of a question waiting for the engineer: it opens the run's dialog.
  function needsYouCard(view, question) {
    const button = el("button", "run-card needs");
    button.type = "button";
    button.append(
      statusIcon("needs"),
      el("span", "run-card-text", `${view.agent} has a question: ${question.message}`),
      el("span", "run-id", view.id),
      el("span", "run-chevron", "›"),
    );
    button.addEventListener("click", () => openDetail({ kind: "run", id: view.id }));
    return button;
  }

  // A run's card: its status, id, agent and notes, in one line.
  function runCard(id, view) {
    const button = el("button", "run-card run-entry");
    button.type = "button";
    if (!view) {
      button.disabled = true;
      const problem = failed.get(id);
      button.append(statusIcon(problem ? "failed" : "stopped"), el("span", "run-id", id));
      button.append(el("span", "run-card-text", problem ? `Cannot be shown: ${problem}` : "Loading…"));
      if (problem) button.title = problem;
      return button;
    }
    button.append(
      statusIcon(iconKind(view)),
      el("span", "run-id", view.id),
      el("span", "run-card-text", `${view.agent} · ${view.notes.join(" ")}`),
      el("span", `run-entry-status ${statusLine(view).tone}`, statusWord(view)),
      el("span", "run-chevron", "›"),
    );
    button.title = `${view.id} · ${view.agent} · ${statusLine(view).text}`;
    button.addEventListener("click", () => openDetail({ kind: "run", id }));
    return button;
  }

  function renderStatus() {
    statusEl.hidden = !run;
    if (!run) return;
    const { text, tone } = statusLine(run);
    const dot = el("span", `status-dot ${tone}`);
    dot.setAttribute("aria-hidden", "true");
    statusEl.replaceChildren(dot, el("span", "status-text", `${run.agent} · ${text}`));
  }

  // The tray header: the latest run's status and, while it runs, its time and Stop.
  function renderHeader() {
    clearInterval(clock);
    clock = null;
    if (!run) {
      headerEl.replaceChildren();
      return;
    }
    const { text, tone } = statusLine(run);
    const title = el("span", "run-title");
    title.title = `${run.id} · ${run.agent} · ${text}`;
    const time = el("span", "run-time");
    const tick = () => {
      if (isLive(run)) time.textContent = ` · ${elapsed(Date.now() / 1000 - run.startedAt)}`;
    };
    tick();
    title.append(statusIcon(iconKind(run)), el("strong", `run-word ${tone}`, statusWord(run)), time);
    const parts = [title];
    if (!isLive(run) || run.status !== "cancelling") forceAt = null;
    if (isLive(run)) {
      const stopping = run.status === "cancelling";
      // The Rust process says how long until it would force the agent, so a reloaded page
      // offers Force stop as soon as the process would act on it.
      if (stopping) forceAt ??= Date.now() + (run.forceIn ?? 0);
      const button = el("button", "button stop");
      button.type = "button";
      const label = () => {
        const force = stopping && forceable();
        button.disabled = stopping && !force;
        button.replaceChildren(force ? "Force stop" : "Stop", el("kbd", "", "esc"));
      };
      label();
      clock = setInterval(() => {
        tick();
        label();
      }, 1000);
      button.addEventListener("click", stop);
      parts.push(button);
    }
    headerEl.replaceChildren(...parts);
  }

  // ---- The dialog: a run whole, or one of its steps -----------------------------------------

  function openDetail(what) {
    detail = what;
    detailDrawn = null;
    drawDetail();
    if (!detailEl.open) {
      detailEl.showModal();
      detailEl.focus();
    }
  }

  // A run deleted while its dialog is open closes the dialog.
  function closeIfGone(list) {
    if (detail && !list.includes(detail.id)) detailEl.close();
  }

  // The run's steps in order, each with its key: every item but the agent's messages and,
  // while the run is live, an open request, which shows above the runs. Once the run is over
  // a request left open is a step too, shown as closed.
  function stepsOf(view) {
    const live = isLive(view);
    return foldRun(view.entries)
      .items.map((item, index) => ({ item, key: itemKey(item, index) }))
      .filter(
        ({ item }) =>
          item.type !== "message" &&
          !(live && (item.type === "permission" || item.type === "question") && item.answer === undefined),
      );
  }

  function itemKey(item, index) {
    if (item.type === "tool") return `tool:${item.id}`;
    if (item.type === "permission") return `permission:${item.request}`;
    return `${item.type}:${index}`;
  }

  // Draws the dialog again when what it shows changed, and keeps its scroll otherwise, while
  // the run streams.
  function drawDetail() {
    if (!detail) return;
    const view = byId(detail.id);
    if (!view) return;
    const steps = stepsOf(view);
    let stamp;
    let draw;
    if (detail.kind === "run") {
      const notes = view.notes.map((id) => noteLine(id, view));
      stamp = [`run:${view.id}`, view.status, view.total, JSON.stringify(notes)];
      draw = () => runDetail(view, steps, notes);
    } else {
      const index = steps.findIndex((step) => step.key === detail.key);
      if (index < 0) return;
      const { item } = steps[index];
      stamp = [`${view.id}:${detail.key}`, item.status, item.output, item.text, item.answer, isLive(view)];
      draw = () => stepDetail(view, item, index, steps.length);
    }
    if (detailDrawn && stamp.every((part, i) => part === detailDrawn[i])) return;
    detailDrawn = stamp;
    const scroll = detailEl.querySelector(".note-dialog-body")?.scrollTop ?? 0;
    const same = detailEl.dataset.shown === stamp[0];
    detailEl.dataset.shown = stamp[0];
    // The question's form is the same node across redraws; so is the control that had the
    // focus in it, which gets it back.
    const typing = asking?.form.contains(document.activeElement) ? document.activeElement : null;
    detailEl.replaceChildren(...draw());
    typing?.focus();
    const body = detailEl.querySelector(".note-dialog-body");
    if (body && same) body.scrollTop = scroll;
  }

  function moveStep(by) {
    const view = byId(detail.id);
    if (!view) return;
    const steps = stepsOf(view);
    const next = steps[steps.findIndex((step) => step.key === detail.key) + by];
    if (next) openDetail({ kind: "step", id: detail.id, key: next.key });
  }

  function detailHeader(icon, title, extra, actions = []) {
    const head = el("header", "note-dialog-header");
    const heading = el("h2", "note-dialog-title", title);
    heading.id = "run-detail-title";
    const close = el("button", "button", "esc");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => detailEl.close());
    const right = el("span", "note-dialog-actions");
    right.append(...actions, close);
    head.append(...(icon ? [icon] : []), heading, el("span", "note-dialog-where", extra), right);
    return head;
  }

  function section(label, ...content) {
    const part = el("section", "note-dialog-section");
    part.append(el("div", "note-dialog-label", label), ...content);
    return part;
  }

  // A note sent in the run: its status in this run, its place and id.
  function noteLine(id, view) {
    const note = noteOf(id);
    if (!note) return { id, where: "deleted", kind: "stopped", label: "Deleted", tone: "idle" };
    const { start, end } = note.lines;
    const where = `${note.path.split("/").at(-1)}:${start === end ? start : `${start}-${end}`}`;
    return { id, where, ...noteStatus(note, view) };
  }

  // The run whole: its question if it asks one, its plan, its notes, every step, and the
  // agent's reply.
  function runDetail(view, steps, notes) {
    const live = isLive(view);
    const body = el("div", "note-dialog-body");
    const { plan, items } = foldRun(view.entries);

    const question = live ? openQuestion(items) : null;
    if (question) {
      if (asking?.request !== question.request) {
        const form = questionForm(question, view.agent, (content) => answerQuestion(question.request, content));
        asking = { request: question.request, form };
      }
      body.append(asking.form);
    } else asking = null;

    if (plan?.length) {
      const progress = planProgress(view);
      const rows = el("div", "run-plan-rows");
      for (const step of plan) {
        const row = el("div", `plan-step ${step.status}`);
        const mark = el("span", "plan-mark", { completed: "✓", in_progress: "●" }[step.status] ?? "○");
        mark.setAttribute("aria-hidden", "true");
        row.append(mark, el("span", "", step.content));
        rows.append(row);
      }
      body.append(section(`Plan · ${progress.done} of ${progress.total}`, rows));
    }

    const noteCards = el("div", "run-cards");
    for (const card of notes) {
      const button = el("button", `run-card ${card.kind}`);
      button.type = "button";
      button.append(
        statusIcon(card.kind),
        el("span", `run-card-text run-note-status ${card.tone}`, card.label),
        el("span", "run-note-where", card.where),
        el("span", "note-id", card.id),
      );
      button.disabled = card.label === "Deleted";
      button.addEventListener("click", () => {
        detailEl.close();
        openNote(card.id);
      });
      noteCards.append(button);
    }
    body.append(section(notes.length === 1 ? "Note" : `Notes · ${notes.length}`, noteCards));

    if (steps.length) {
      const stepCards = el("div", "run-cards");
      for (const { item, key } of steps) {
        stepCards.append(stepCard(view, item, () => openDetail({ kind: "step", id: view.id, key })));
      }
      body.append(section(`Steps · ${steps.length}`, stepCards));
    }

    const text = items.filter((item) => item.type === "message").map((item) => item.text.trim()).join("\n\n");
    if (text) body.append(section(live ? `${view.agent} is writing` : `${view.agent}'s reply`, markdownView(text)));
    if (view.problem) body.append(el("div", "run-notice", view.problem));

    const started = new Date(view.startedAt * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    const when = live ? elapsed(Date.now() / 1000 - view.startedAt) : `started ${started}`;
    return [
      detailHeader(statusIcon(iconKind(view)), `${view.id} · ${view.agent}`, `${statusWord(view)} · ${when}`),
      body,
    ];
  }

  // A step's card: what it did and how it went, in one line.
  function stepCard(view, item, open) {
    const card = (parts) => {
      const button = el("button", "run-card step");
      button.type = "button";
      button.append(...parts, el("span", "run-chevron", "›"));
      button.addEventListener("click", open);
      return button;
    };
    switch (item.type) {
      case "tool": {
        const row = toolRow(item, isLive(view));
        const tone = row.state === "failed" || row.state === "unfinished" ? ` ${row.state}` : "";
        const state = row.running ? statusIcon("working") : el("span", `tool-state${tone}`, row.state);
        const node = card([el("span", "tool-verb", row.verb), el("span", "run-card-text tool-target", row.target), state]);
        node.title = item.title;
        return node;
      }
      case "thought":
        return card([
          el("span", "tool-verb", "think"),
          el("span", "run-card-text run-thought-preview", item.text.trim().replace(/\s+/g, " ")),
        ]);
      case "permission": {
        const chosen = item.options.find((option) => option.id === item.answer);
        // "allow" when it was allowed, by Postil or by hand; "ask" when rejected or closed.
        const verb = chosen?.kind?.startsWith("allow") ? "allow" : "ask";
        return card([
          el("span", "tool-verb", verb),
          el("span", "run-card-text", item.title),
          el("span", "tool-state", chosen ? chosen.name : "cancelled"),
        ]);
      }
      case "question":
        return card([
          el("span", "tool-verb", "ask"),
          el("span", "run-card-text", item.message),
          el("span", "tool-state", questionState(item)),
        ]);
      case "error":
        return el("div", "run-error", item.text);
      default:
        return el("div", "run-notice", item.text);
    }
  }

  function stepDetail(view, item, index, count) {
    const body = el("div", "note-dialog-body");
    const nav = el("div", "run-detail-nav");
    const back = el("button", "button", `‹ ${view.id}`);
    back.type = "button";
    back.setAttribute("aria-label", `Back to ${view.id}`);
    back.addEventListener("click", () => openDetail({ kind: "run", id: view.id }));
    const previous = el("button", "button", "‹");
    previous.type = "button";
    previous.setAttribute("aria-label", "The step before");
    previous.disabled = index === 0;
    previous.addEventListener("click", () => moveStep(-1));
    const following = el("button", "button", "›");
    following.type = "button";
    following.setAttribute("aria-label", "The step after");
    following.disabled = index === count - 1;
    following.addEventListener("click", () => moveStep(1));
    nav.append(back, el("span", "", `Step ${index + 1} of ${count} · ← → to move`), previous, following);
    body.append(nav);

    if (item.type === "thought") {
      body.append(el("div", "run-detail-text", item.text.trim()));
      return [detailHeader(null, "Thinking", view.id), body];
    }
    if (item.type === "permission") {
      const chosen = item.options.find((option) => option.id === item.answer);
      body.append(el("div", "run-detail-text", `Answered: ${chosen ? chosen.name : "cancelled"}`));
      return [detailHeader(null, item.title, view.id), body];
    }
    if (item.type === "question") {
      body.append(el("div", "run-detail-text", item.message));
      const content = item.answer?.content;
      const answered = content ? Object.entries(content) : [];
      if (answered.length) {
        const list = el("dl", "question-answers");
        for (const [key, value] of answered) {
          list.append(el("dt", "", key), el("dd", "", Array.isArray(value) ? value.join(", ") : String(value)));
        }
        body.append(section("Your answer", list));
      } else body.append(el("div", "run-detail-text muted", questionState(item)));
      return [detailHeader(null, "Question", `${view.id} · ${questionState(item)}`), body];
    }
    const row = toolRow(item, isLive(view));
    const kind = item.status === "failed" ? "failed" : row.running ? "working" : "addressed";
    const actions = [];
    const path = item.places[0]?.path ?? item.output.find((part) => part.type === "diff")?.path;
    if (path) {
      const show = el("button", "button", "Open file");
      show.type = "button";
      show.addEventListener("click", () => {
        detailEl.close();
        openFile(path);
      });
      actions.push(show);
    }
    // A command's title is the command; a file tool's only repeats the header.
    const onFile = ["read", "edit", "delete", "move"].includes(item.tool);
    if (!onFile) body.append(el("pre", "run-detail-command", item.title));
    for (const part of item.output) {
      if (part.type === "diff") body.append(section(part.path, diffView(part)));
      else body.append(el("pre", "tool-output", part.text));
    }
    if (item.output.length === 0) {
      body.append(el("div", "run-detail-text muted", row.running ? "Under way…" : "No output."));
    }
    return [detailHeader(statusIcon(kind), `${capital(row.verb)} ${row.target}`, row.state, actions), body];
  }

  // How a closed question ended: answered, skipped, or closed with no answer.
  function questionState(item) {
    if (item.answer === null) return "closed";
    return item.answer?.content ? "answered" : "skipped";
  }

  // The agent's Markdown as elements, built node by node: its text is never parsed as HTML.
  function markdownView(text) {
    const view = el("div", "run-message");
    for (const block of markdownBlocks(text.trim())) {
      if (block.type === "code") view.append(el("pre", "run-code", block.text));
      else if (block.type === "list") {
        const list = el(block.ordered ? "ol" : "ul");
        for (const item of block.items) list.append(inlineView("li", item));
        view.append(list);
      } else view.append(inlineView(block.type === "heading" ? "h4" : "p", block.inline));
    }
    return view;
  }

  function inlineView(tag, parts) {
    const node = el(tag);
    for (const part of parts) {
      const wrap = { code: "code", strong: "strong", em: "em" }[part.type];
      node.append(wrap ? el(wrap, "", part.text) : part.text);
    }
    return node;
  }

  function diffView(diff) {
    const view = el("pre", "tool-diff");
    const marks = { same: "  ", del: "- ", add: "+ " };
    for (const line of partLines(diff)) {
      view.append(el("span", line.kind, `${marks[line.kind]}${line.text}\n`));
    }
    return view;
  }

  function permissionView(request) {
    const card = el("div", "permission open");
    card.append(el("div", "permission-label", "Permission needed"), el("div", "permission-title", request.title));
    const keys = permissionKeys(request.options);
    const buttons = el("div", "permission-actions");
    for (const option of request.options) {
      const primary = option.kind === "allow_once";
      const button = el("button", primary ? "button primary" : "button");
      button.type = "button";
      button.append(option.name);
      const key = keyFor(keys, option.id);
      if (key) button.append(el("kbd", "", key));
      button.addEventListener("click", () => answer(request, option.id));
      buttons.append(button);
    }
    card.append(buttons);
    return card;
  }

  return {
    sync,
    stop,
    keydown,
    byId,
    answerQuestion,
    forget,
    reload,
    run: () => run,
    live: () => isLive(run),
  };
}
