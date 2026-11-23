// The run panel: the tray's Run tab and the agent status at the foot of the sidebar. The tab
// lists the runs, one small card each, newest first; a card opens the run's dialog, which
// holds everything about it: its plan, its notes, every step, and the agent's reply
// (docs/design.md, "The Run tab"). ui/run.js has the rules. A run's follow-ups are part of
// its card and its dialog, which ends with the box that sends the next one.
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
import { noteStatus, planProgress, reviewOf, threadOf } from "./status.js";

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
// file in the code pane; `reviewers()` names the settings' reviewers, in order.
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
  // The follow-up box of the thread shown, kept across redraws for the same reason:
  // `{ root, form, update(latest) }`.
  let following = null;
  detailEl.addEventListener("close", () => {
    detail = null;
    detailDrawn = null;
    asking = null;
    following = null;
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
    // Reviewers' runs show inside the run they review, and follow-ups inside the run they
    // follow, not here.
    const shown = list.filter((id) => !byId(id)?.review && !byId(id)?.followUp);
    const label = shown.length === 1 ? "Run" : `Runs · ${shown.length}`;
    wanted.push(["label", label, () => el("div", "run-label", label)]);
    const known = knownViews();
    for (const id of shown) {
      const view = byId(id);
      const latest = view ? threadOf(id, known).at(-1) : null;
      const sig = view
        ? JSON.stringify([latest.id, latest.status, view.agent, view.notes, statusWord(latest)])
        : (failed.get(id) ?? "loading");
      wanted.push([`run:${id}`, sig, () => runCard(id, view, latest)]);
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

  // A run's card: its status, id, agent and notes, in one line. The status is its thread's
  // latest run's, `latest`: the run itself, or its last follow-up.
  function runCard(id, view, latest) {
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
    const more = latest.id === view.id ? "" : ` · +${latest.id}`;
    button.append(
      statusIcon(iconKind(latest)),
      el("span", "run-id", view.id),
      el("span", "run-card-text", `${view.agent} · ${view.notes.join(" ")}${more}`),
      el("span", `run-entry-status ${statusLine(latest).tone}`, statusWord(latest)),
      el("span", "run-chevron", "›"),
    );
    button.title = `${latest.id} · ${view.agent} · ${statusLine(latest).text}`;
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
      // A follow-up opens as its thread, from the run it started from.
      const thread = view.review ? [view] : threadOf(view.id, knownViews());
      const root = thread[0] ?? view;
      const notes = root.notes.map((id) => noteLine(id, root));
      const review = root.review ? null : reviewOf(root.id, knownViews());
      const reviewKey = JSON.stringify(review && [review.stages.map((stage) => [stage.id, stage.status, stage.total])]);
      const threadKey = JSON.stringify(thread.map((turn) => [turn.id, turn.status, turn.total]));
      stamp = [`run:${root.id}`, threadKey, JSON.stringify(notes), reviewKey, isLive(run)];
      draw = () => runDetail(root, stepsOf(root), notes, review, thread);
    } else if (detail.kind === "finding") {
      const review = reviewOf(view.id, knownViews());
      const finding = review?.findings.find((item) => item.id === detail.finding);
      if (!finding) return;
      stamp = [`finding:${view.id}:${finding.id}`, JSON.stringify(finding)];
      draw = () => findingDetail(view, finding);
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
    const typing = [asking?.form, following?.form].some((form) => form?.contains(document.activeElement))
      ? document.activeElement
      : null;
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
  // agent's reply; then each follow-up in `thread` (the run first), and the box for the next.
  function runDetail(view, steps, notes, review, thread) {
    const latest = thread.at(-1) ?? view;
    const live = isLive(view);
    const body = el("div", "note-dialog-body");
    // A reviewer's run says which run it reviews, and leads back to it.
    if (view.review) {
      const { of, reviewer, reviewers: count } = view.review;
      const role = count > 1 && reviewer === count ? "judges the findings and makes the changes" : "proposes findings, read-only";
      const back = el("button", "button", `‹ ${of}`);
      back.type = "button";
      back.addEventListener("click", () => openDetail({ kind: "run", id: of }));
      const nav = el("div", "run-detail-nav");
      nav.append(back, el("span", "", `Reviewer ${reviewer} of ${count} of ${of}: ${role}`));
      body.append(nav);
    }
    const { plan, items } = foldRun(view.entries);

    // The thread's question, from whichever of its runs is under way: the latest.
    const question = isLive(latest) ? openQuestion(foldRun(latest.entries).items) : null;
    if (question) {
      if (asking?.request !== question.request) {
        const form = questionForm(question, latest.agent, (content) => answerQuestion(question.request, content));
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
    for (const turn of thread.slice(1)) body.append(...followUpPart(turn));
    if (!view.review) {
      body.append(reviewSection(view, review));
      body.append(followUpBox(view, latest));
    }

    const started = new Date(view.startedAt * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    const when = isLive(latest) ? elapsed(Date.now() / 1000 - latest.startedAt) : `started ${started}`;
    const title = latest.id === view.id ? `${view.id} · ${view.agent}` : `${view.id} to ${latest.id} · ${view.agent}`;
    return [detailHeader(statusIcon(iconKind(latest)), title, `${statusWord(latest)} · ${when}`), body];
  }

  // One follow-up in its thread's dialog: what the engineer asked, its steps, and the reply.
  function followUpPart(turn) {
    const parts = [];
    const asked = section(`You · ${turn.id}`, el("div", "follow-up-text", turn.followUp.text));
    // The prompt holds more than the words when a fresh agent was told the thread first.
    if (turn.prompt !== turn.followUp.text) {
      asked.append(el("div", "note-dialog-said muted", `Sent to a new ${turn.agent} with the thread so far: the earlier conversation had ended.`));
    }
    parts.push(asked);
    const steps = stepsOf(turn);
    if (steps.length) {
      const cards = el("div", "run-cards");
      for (const { item, key } of steps) {
        cards.append(stepCard(turn, item, () => openDetail({ kind: "step", id: turn.id, key })));
      }
      parts.push(section(`Steps · ${steps.length}`, cards));
    }
    const { items } = foldRun(turn.entries);
    const text = items.filter((item) => item.type === "message").map((item) => item.text.trim()).join("\n\n");
    if (text) parts.push(section(isLive(turn) ? `${turn.agent} is writing` : `${turn.agent}'s reply`, markdownView(text)));
    if (turn.problem) parts.push(el("div", "run-notice", turn.problem));
    return parts;
  }

  // The box that sends a follow-up to the thread's latest run, `latest`. One node per thread,
  // kept across redraws so a draft and the focus survive the run streaming.
  function followUpBox(root, latest) {
    if (following?.root !== root.id) {
      const form = el("form", "follow-up");
      const input = el("textarea", "follow-up-input");
      input.rows = 3;
      input.setAttribute("aria-label", "Follow-up");
      const why = el("span", "note-dialog-why");
      const button = el("button", "button primary");
      button.type = "submit";
      const actions = el("div", "follow-up-actions");
      actions.append(why, button);
      form.append(input, actions);
      let target = latest;
      let sending = false;
      const update = (next) => {
        target = next;
        const busy = isLive(run);
        input.placeholder = `Ask ${next.agent} for more, or to change what it did in ${next.id}`;
        button.replaceChildren(`Send to ${next.agent}`, el("kbd", "", "⌘↵"));
        button.disabled = busy || sending;
        why.textContent = busy ? "Wait for the run under way, or stop it." : "";
      };
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        const text = input.value.trim();
        if (!text || button.disabled) return;
        sending = true;
        update(target);
        post("api/run/follow", { of: target.id, text }).then(
          () => {
            input.value = "";
          },
          (error) => onError(error.message),
        ).finally(() => {
          sending = false;
          update(target);
        });
      });
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter" && event.metaKey) {
          event.preventDefault();
          event.stopPropagation();
          form.requestSubmit();
        }
      });
      following = { root: root.id, form, update };
    }
    following.update(latest);
    return section("Follow up", following.form);
  }

  // Every run read so far, to find a run's reviews among them.
  function knownViews() {
    return [...new Set([...ids, run?.id].filter(Boolean))].map(byId).filter(Boolean);
  }

  // Starts the settings' reviewers on run `of`.
  function startReview(of) {
    return post("api/run/review", { of }).catch((error) => onError(error.message));
  }

  // The run's review: the Review button, a card per reviewer of the latest review, and a
  // card per finding with its verdict (docs/design.md, "Reviews").
  function reviewSection(view, review) {
    const names = reviewers();
    const part = el("section", "note-dialog-section");
    const label = el("div", "note-dialog-label");
    label.append(el("span", "", review ? `Review · ${review.findings.length} found` : "Review"));
    const button = el("button", "button", review ? "Review again" : "Review");
    button.type = "button";
    button.disabled = isLive(run) || isLive(view) || names.length === 0;
    button.title = names.length
      ? `Reviewers in order: ${names.join(", ")}`
      : "Add reviewers in Settings first";
    button.addEventListener("click", () => {
      // Once: a second click would only be refused, as a run is then under way.
      button.disabled = true;
      startReview(view.id);
    });
    label.append(button);
    part.append(label);
    if (!review) {
      part.append(
        el(
          "div",
          "run-detail-text muted",
          names.length ? `Not reviewed yet. Reviewers: ${names.join(", ")}.` : "Not reviewed yet. Add reviewers in Settings.",
        ),
      );
      return part;
    }
    const cards = el("div", "run-cards");
    for (const stage of review.stages) {
      const judges = review.reviewers > 1 && stage.review.reviewer === review.reviewers;
      const button = el("button", "run-card");
      button.type = "button";
      button.append(
        statusIcon(iconKind(stage)),
        el("span", "run-id", stage.id),
        el("span", "run-card-text", `${stage.agent} · ${judges ? "judges and fixes" : "proposes"}`),
        el("span", `run-entry-status ${statusLine(stage).tone}`, statusWord(stage)),
        el("span", "run-chevron", "›"),
      );
      button.addEventListener("click", () => openDetail({ kind: "run", id: stage.id }));
      cards.append(button);
    }
    for (const finding of review.findings) {
      const icon = { accepted: "addressed", rejected: "stopped", judging: "working" }[finding.verdict.word] ?? "open";
      const button = el("button", `run-card finding ${finding.verdict.word.replace(" ", "-")}`);
      button.type = "button";
      button.title = finding.text;
      button.append(
        statusIcon(icon),
        el("span", "note-id", finding.id),
        el("span", "run-card-text", finding.text),
        el("span", "run-entry-status", finding.verdict.word),
        el("span", "run-chevron", "›"),
      );
      button.addEventListener("click", () => openDetail({ kind: "finding", id: view.id, finding: finding.id }));
      cards.append(button);
    }
    part.append(cards);
    for (const problem of review.problems) part.append(el("div", "run-notice", `A reviewer's findings could not be read: ${problem}`));
    return part;
  }

  // A finding whole: what the reviewer found and proposed, where, and the judge's verdict.
  function findingDetail(view, finding) {
    const body = el("div", "note-dialog-body");
    const back = el("button", "button", `‹ ${view.id}`);
    back.type = "button";
    back.addEventListener("click", () => openDetail({ kind: "run", id: view.id }));
    const nav = el("div", "run-detail-nav");
    const where = [finding.note, finding.path && (finding.line ? `${finding.path}:${finding.line}` : finding.path)];
    nav.append(back, el("span", "", [`found by ${finding.from}`, ...where].filter(Boolean).join(" · ")));
    body.append(nav, section("Finding", el("div", "run-detail-text", finding.text)));
    if (finding.fix) body.append(section("Proposed fix", el("div", "run-detail-text", finding.fix)));
    const verdict = finding.verdict.text || {
      "not judged": "A single reviewer only proposes: no one judged it.",
      waiting: "The judging reviewer has not started yet.",
      judging: "The judging reviewer is at work.",
      "no word": "The judging reviewer ended without a line about it.",
    }[finding.verdict.word] || finding.verdict.word;
    body.append(section("Verdict", el("div", "run-detail-text", verdict)));
    return [detailHeader(null, `${finding.id} · ${finding.verdict.word}`, `review of ${view.id}`), body];
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
    knownViews,
    startReview,
    answerQuestion,
    forget,
    reload,
    run: () => run,
    live: () => isLive(run),
  };
}
