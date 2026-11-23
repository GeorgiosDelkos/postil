// The run panel: the tray's Run tab and the agent status at the foot of the sidebar. It
// fetches the latest run from `api/run` and draws it; ui/run.js has the rules.
//
// The Rust process calls `window.postil.runChanged()` whenever the run changes; the panel
// then fetches the entries it has not seen. Fetches never overlap: a change that arrives
// during one is fetched right after it.

import {
  elapsed,
  foldRun,
  isLive,
  keyFor,
  markdownBlocks,
  openPermission,
  partLines,
  permissionKey,
  permissionKeys,
  statusLine,
  toolRow,
  underWay,
} from "./run.js";
import { statusIcon } from "./note-dialog.js";
import { noteStatus, planProgress } from "./status.js";

const capital = (word) => word.charAt(0).toUpperCase() + word.slice(1);

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// `noteOf(id)` gives a note by id, or null once it is deleted; `openNote(id)` opens its
// detail. `panelEl` is the Run tab's body, `headerEl` the tray header's run part, `statusEl` the
// sidebar's agent status. `onChange(run, previous)` is called after every fetch that
// changed the run, so the page can follow it (switch tabs, re-read files).
// `detailEl` is the `<dialog>` a card opens; `openFile(path)` shows a file in the code pane.
export function runPanel({
  panelEl,
  headerEl,
  statusEl,
  detailEl,
  onChange,
  onError,
  noteOf,
  openNote,
  openFile,
}) {
  let run = null;
  let fetching = false;
  let again = false;
  let clock = null;
  // When Stop starts to end the agent, by this page's clock; null unless it is stopping.
  let forceAt = null;
  // The drawn items by key, each with the signature it was drawn from: an item whose
  // signature is unchanged keeps its node, so an open diff stays open and a selection stays.
  let views = new Map();
  let drawnRun = null;
  // What the detail dialog shows while open: `{ kind: "steps" }`, `{ kind: "step", key }`,
  // `{ kind: "reply" }` or `{ kind: "plan" }`; and what it was drawn from, to draw it again only when that changes.
  let detail = null;
  let detailDrawn = null;
  detailEl.addEventListener("close", () => {
    detail = null;
    detailDrawn = null;
  });
  detailEl.addEventListener("keydown", (event) => {
    if (detail?.kind !== "step" || event.metaKey || event.altKey || event.ctrlKey) return;
    if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
      event.preventDefault();
      moveStep(event.key === "ArrowLeft" ? -1 : 1);
    }
  });
  // Whether the panel follows the end of the run: true until the engineer scrolls up, and
  // again once they scroll back to the end. Scrolling up is read as any move above where
  // `reveal` last put the panel, not as a distance from the end: a slow trackpad scroll moves
  // a few px at a time, and a render between two of its events would scroll it back down.
  let follow = true;
  let revealedTop = 0;
  // How long to wait before fetching again after a failed fetch: doubled each failure.
  let retryIn = 1000;
  panelEl.addEventListener("scroll", () => {
    if (panelEl.clientHeight === 0) return;
    if (atEnd()) follow = true;
    else if (panelEl.scrollTop < revealedTop) follow = false;
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
        if (next === null && previous === null) continue;
        run = next;
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

  function render() {
    if (drawnRun !== (run?.id ?? null)) {
      views = new Map();
      drawnRun = run?.id ?? null;
      follow = true;
      forceAt = null;
      if (detailEl.open) detailEl.close();
    }
    renderStatus();
    renderHeader();
    if (!run) {
      panelEl.replaceChildren(el("div", "empty", "No run yet. Send notes to an agent to start one."));
      return;
    }
    // Top to bottom: what needs the engineer, the plan, each note's result, the steps, and the
    // agent's reply, each a card of one line that opens the detail dialog (docs/design.md,
    // "The Run tab").
    const { plan, items } = foldRun(run.entries);
    const live = isLive(run);
    const asking = items.filter((item) => item.type === "permission" && item.answer === undefined);
    const steps = stepsOf(items);
    const messages = items.filter((item) => item.type === "message");
    const wanted = [];
    for (const item of asking) {
      wanted.push([itemKey(item, items.indexOf(item)), signature(item, live), () => permissionView(item)]);
    }
    if (plan?.length) wanted.push(["plan", JSON.stringify(plan), () => planCard(plan)]);

    const cards = run.notes.map((id) => noteCard(id));
    wanted.push(["notes", JSON.stringify(cards), () => notesView(cards)]);

    // The steps are one card, the latest step: the list and each step are in the dialog.
    if (steps.length) {
      const last = steps.at(-1);
      const sig = JSON.stringify([steps.length, last.key, signature(last.item, live)]);
      wanted.push(["steps", sig, () => stepsCard(steps.length, last)]);
    }
    if (messages.length) {
      const text = messages.map((message) => message.text.trim()).join("\n\n");
      wanted.push(["reply", JSON.stringify([text, live]), () => replyCard(text, live)]);
    }
    if (run.problem) wanted.push(["problem", run.problem, () => el("div", "run-notice", run.problem)]);
    if (wanted.length === 0) {
      wanted.push(["empty", statusLine(run).text, () => el("div", "empty", statusLine(run).text)]);
    }

    const next = new Map();
    const nodes = wanted.map(([key, sig, build]) => {
      const drawn = views.get(key);
      const node = drawn?.sig === sig ? drawn.node : build();
      next.set(key, { sig, node });
      return node;
    });
    views = next;
    // Only nodes out of place move; the rest, the top of a long run, are left untouched.
    nodes.forEach((node, index) => {
      if (panelEl.childNodes[index] !== node) panelEl.insertBefore(node, panelEl.childNodes[index] ?? null);
    });
    while (panelEl.childNodes.length > nodes.length) panelEl.lastChild.remove();
    reveal();
    drawDetail();
  }

  // The run's steps in order, each with its key: every item but the agent's messages and an
  // open permission request, which shows above.
  function stepsOf(items) {
    return items
      .map((item, index) => ({ item, key: itemKey(item, index) }))
      .filter(({ item }) => item.type !== "message" && !(item.type === "permission" && item.answer === undefined));
  }

  // Scrolls to the end while the panel follows it. Measured only while the panel shows: a
  // hidden panel has no height, and would read as both at the end and scrolled to the top.
  function reveal() {
    if (!follow || panelEl.clientHeight === 0) return;
    panelEl.scrollTop = panelEl.scrollHeight;
    revealedTop = panelEl.scrollTop;
  }

  function atEnd() {
    return panelEl.scrollHeight - panelEl.scrollTop - panelEl.clientHeight < 24;
  }

  function itemKey(item, index) {
    if (item.type === "tool") return `tool:${item.id}`;
    if (item.type === "permission") return `permission:${item.request}`;
    return `${item.type}:${index}`;
  }

  // What an item's node is drawn from: when this is unchanged, so is the node.
  function signature(item, live) {
    switch (item.type) {
      case "tool":
        // `live` only changes how a call still under way is drawn, so a run's end leaves the
        // finished ones alone.
        return JSON.stringify(toolRow(item, live && underWay(item)));
      case "permission":
        return JSON.stringify([item.title, item.answer ?? "open"]);
      default:
        return item.text;
    }
  }

  function renderStatus() {
    statusEl.hidden = !run;
    if (!run) return;
    const { text, tone } = statusLine(run);
    const dot = el("span", `status-dot ${tone}`);
    dot.setAttribute("aria-hidden", "true");
    statusEl.replaceChildren(dot, el("span", "status-text", `${run.agent} · ${text}`));
  }

  function renderHeader() {
    clearInterval(clock);
    clock = null;
    if (!run) {
      headerEl.replaceChildren();
      return;
    }
    // The status's icon and word, then the run, its agent and, while it runs, its time.
    const { text, tone } = statusLine(run);
    const word = run.status === "unfinished" ? "Unfinished" : text;
    const kind = { busy: "working", attention: "needs", done: "addressed", failed: "failed" }[tone];
    const title = el("span", "run-title");
    title.title = text;
    const time = el("span", "run-time");
    const tick = () => {
      if (isLive(run)) time.textContent = ` · ${elapsed(Date.now() / 1000 - run.startedAt)}`;
    };
    tick();
    title.append(
      statusIcon(kind ?? "stopped"),
      el("strong", `run-word ${tone}`, word),
      el("span", "run-meta", ` · ${run.id} · ${run.agent}`),
      time,
    );
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

  // A note sent in the run: its status in this run and the agent's line about it.
  function noteCard(id) {
    const note = noteOf(id);
    if (!note) return { id, where: "deleted", kind: "stopped", label: "Deleted", tone: "idle" };
    const { start, end } = note.lines;
    const where = `${note.path.split("/").at(-1)}:${start === end ? start : `${start}-${end}`}`;
    const status = noteStatus(note, run);
    return { id, where, ...status };
  }

  function notesView(cards) {
    const view = el("section", "run-notes");
    const addressed = cards.filter((card) => card.kind === "addressed").length;
    const label = el("div", "run-label");
    label.append(
      el("span", "", cards.length === 1 ? "Note" : `Notes · ${cards.length}`),
      el("span", "", addressed ? `${addressed} addressed` : ""),
    );
    view.append(label);
    for (const card of cards) {
      const button = el("button", `run-card ${card.kind}`);
      button.type = "button";
      // Its status only: what the agent said is in the note's detail.
      button.append(
        statusIcon(card.kind),
        el("span", `run-card-text run-note-status ${card.tone}`, card.label),
        el("span", "run-note-where", card.where),
        el("span", "note-id", card.id),
      );
      button.addEventListener("click", () => openNote(card.id));
      view.append(button);
    }
    return view;
  }

  // The steps' card: how many, and the latest, which opens the list of them all.
  function stepsCard(count, last) {
    const view = el("section", "run-steps");
    view.append(el("div", "run-label", count === 1 ? "Step" : `Steps · ${count}`));
    view.append(stepCard(last.item, () => openDetail({ kind: "steps" })));
    return view;
  }

  // A card: a button of one line (or a few, for the reply) that opens the detail dialog.
  function card(className, parts, open) {
    const button = el("button", `run-card ${className}`);
    button.type = "button";
    button.append(...parts, el("span", "run-chevron", "›"));
    button.addEventListener("click", open);
    return button;
  }

  function replyCard(text, live) {
    const view = el("section", "run-reply");
    view.append(el("div", "run-label", live ? `${run.agent} is writing` : `${run.agent}'s reply`));
    const preview = el("span", "run-card-text run-reply-preview", text.replace(/\s+/g, " "));
    view.append(card("reply", [preview], () => openDetail({ kind: "reply" })));
    return view;
  }

  function planCard(plan) {
    const view = el("section", "run-plan");
    const progress = planProgress(run);
    view.append(el("div", "run-label", `Plan · ${progress.done} of ${progress.total}`));
    const bar = el("span", "run-progress");
    const fill = el("span", "run-progress-fill");
    fill.style.width = `${(100 * progress.done) / progress.total}%`;
    bar.append(fill);
    // The step under way, else the next to do, else the last: what the plan is at.
    const at =
      plan.find((step) => step.status === "in_progress") ??
      plan.find((step) => step.status === "pending") ??
      plan.at(-1);
    const body = el("span", "run-plan-body");
    body.append(bar, el("span", "run-card-text", at.content));
    view.append(card("plan", [body], () => openDetail({ kind: "plan" })));
    return view;
  }

  // `open` runs on a click: the step's own detail, or for the steps' card the list.
  function stepCard(item, open) {
    switch (item.type) {
      case "tool": {
        const row = toolRow(item, isLive(run));
        const tone = row.state === "failed" || row.state === "unfinished" ? ` ${row.state}` : "";
        const state = el("span", `tool-state${tone}`, row.state);
        const node = card(`step${row.running ? " running" : ""}`, [
          el("span", "tool-verb", row.verb),
          el("span", "run-card-text tool-target", row.target),
          state,
        ], open);
        node.title = item.title;
        return node;
      }
      case "thought":
        return card("step", [
          el("span", "tool-verb", "think"),
          el("span", "run-card-text run-thought-preview", item.text.trim().replace(/\s+/g, " ")),
        ], open);
      case "permission": {
        const chosen = item.options.find((option) => option.id === item.answer);
        return card("step", [
          el("span", "tool-verb", "ask"),
          el("span", "run-card-text", item.title),
          el("span", "tool-state", chosen ? chosen.name : "cancelled"),
        ], open);
      }
      case "error":
        return el("div", "run-error", item.text);
      default:
        return el("div", "run-notice", item.text);
    }
  }

  // ---- The detail dialog --------------------------------------------------------------

  function openDetail(what) {
    detail = what;
    detailDrawn = null;
    drawDetail();
    if (!detailEl.open) detailEl.showModal();
    // The list opens at its end: the latest step is the one being watched.
    if (what.kind === "steps") {
      const body = detailEl.querySelector(".note-dialog-body");
      body.scrollTop = body.scrollHeight;
    }
  }

  // Draws the dialog again when what it shows changed: a step's state or output, the reply's
  // text, the plan. Its scroll is kept otherwise, while the run streams.
  function drawDetail() {
    if (!detail || !run) return;
    const { plan, items } = foldRun(run.entries);
    const steps = stepsOf(items);
    let stamp;
    let draw;
    if (detail.kind === "steps") {
      stamp = ["steps", JSON.stringify(steps.map(({ item, key }) => [key, signature(item, isLive(run))]))];
      draw = () => stepsDetail(steps);
    } else if (detail.kind === "step") {
      const index = steps.findIndex((step) => step.key === detail.key);
      if (index < 0) return;
      const { item } = steps[index];
      stamp = [detail.key, item.status, item.output, item.text, item.answer, isLive(run)];
      draw = () => stepDetail(item, index, steps.length);
    } else if (detail.kind === "reply") {
      const text = items.filter((item) => item.type === "message").map((item) => item.text.trim()).join("\n\n");
      stamp = ["reply", text];
      draw = () => replyDetail(text);
    } else {
      stamp = ["plan", JSON.stringify(plan)];
      draw = () => planDetail(plan ?? []);
    }
    if (detailDrawn && stamp.every((part, i) => part === detailDrawn[i])) return;
    detailDrawn = stamp;
    const scroll = detailEl.querySelector(".note-dialog-body")?.scrollTop ?? 0;
    const sameItem = detailEl.dataset.shown === stamp[0];
    detailEl.dataset.shown = stamp[0];
    detailEl.replaceChildren(...draw());
    const body = detailEl.querySelector(".note-dialog-body");
    if (body && sameItem) body.scrollTop = scroll;
  }

  function moveStep(by) {
    const steps = stepsOf(foldRun(run.entries).items);
    const index = steps.findIndex((step) => step.key === detail.key);
    const next = steps[index + by];
    if (next) openDetail({ kind: "step", key: next.key });
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

  // Every step, one card each, the latest last; a card opens that step.
  function stepsDetail(steps) {
    const body = el("div", "note-dialog-body run-steps-list");
    for (const { item, key } of steps) body.append(stepCard(item, () => openDetail({ kind: "step", key })));
    const live = isLive(run);
    return [detailHeader(null, `Steps · ${steps.length}`, `${run.id} · ${run.agent}${live ? " · under way" : ""}`), body];
  }

  function stepDetail(item, index, count) {
    const body = el("div", "note-dialog-body");
    const nav = el("div", "run-detail-nav");
    const all = el("button", "button", "‹ All steps");
    all.type = "button";
    all.addEventListener("click", () => openDetail({ kind: "steps" }));
    nav.append(all);
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
    nav.append(el("span", "", `Step ${index + 1} of ${count} · ← → to move`), previous, following);
    body.append(nav);

    if (item.type === "thought") {
      body.append(el("div", "run-detail-text", item.text.trim()));
      return [detailHeader(null, "Thinking", run.id), body];
    }
    if (item.type === "permission") {
      const chosen = item.options.find((option) => option.id === item.answer);
      body.append(el("div", "run-detail-text", `Answered: ${chosen ? chosen.name : "cancelled"}`));
      return [detailHeader(null, item.title, run.id), body];
    }
    const row = toolRow(item, isLive(run));
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
    if (item.title !== row.target) body.append(el("pre", "run-detail-command", item.title));
    for (const part of item.output) {
      if (part.type === "diff") {
        if (item.output.length > 1) body.append(el("div", "run-label", part.path));
        body.append(diffView(part));
      } else body.append(el("pre", "tool-output", part.text));
    }
    if (item.output.length === 0) {
      body.append(el("div", "run-detail-text muted", row.running ? "Under way…" : "No output."));
    }
    return [detailHeader(statusIcon(kind), `${capital(row.verb)} ${row.target}`, row.state, actions), body];
  }

  function replyDetail(text) {
    const body = el("div", "note-dialog-body");
    body.append(markdownView(text));
    return [detailHeader(null, `${run.agent}'s reply`, run.id), body];
  }

  function planDetail(plan) {
    const body = el("div", "note-dialog-body");
    for (const step of plan) {
      const row = el("div", `plan-step ${step.status}`);
      const mark = el("span", "plan-mark", { completed: "✓", in_progress: "●" }[step.status] ?? "○");
      mark.setAttribute("aria-hidden", "true");
      row.append(mark, el("span", "", step.content));
      body.append(row);
    }
    const progress = planProgress(run);
    return [detailHeader(null, "Plan", progress ? `${progress.done} of ${progress.total} done` : ""), body];
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
    const open = request.answer === undefined;
    const card = el("div", `permission${open ? " open" : ""}`);
    card.append(
      el("div", "permission-label", open ? "Permission needed" : "Permission"),
      el("div", "permission-title", request.title),
    );
    if (!open) {
      const chosen = request.options.find((option) => option.id === request.answer);
      card.append(el("div", "permission-answer", chosen ? chosen.name : "Cancelled"));
      return card;
    }
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
    run: () => run,
    live: () => isLive(run),
    render,
    reveal,
  };
}
