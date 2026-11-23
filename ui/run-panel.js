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
import { agentSaid, noteStatus, planProgress } from "./status.js";

// A finished run shows this many of its last steps until "Show all" (docs/design.md, "The
// Run tab"). Arbitrary: enough to see how the run ended.
const STEPS_SHOWN = 5;

// A reply longer than this many characters is cut until "Show the whole reply". Arbitrary:
// about ten lines in a tray of the default width.
const REPLY_CUT = 700;

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
export function runPanel({ panelEl, headerEl, statusEl, onChange, onError, noteOf, openNote }) {
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
  // Whether the engineer asked for every step of a finished run, and for its whole reply.
  let allSteps = false;
  let wholeReply = false;
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
      allSteps = false;
      wholeReply = false;
    }
    renderStatus();
    renderHeader();
    if (!run) {
      panelEl.replaceChildren(el("div", "empty", "No run yet. Send notes to an agent to start one."));
      return;
    }
    // Top to bottom: what needs the engineer, the plan, each note's result, the steps, and the
    // agent's reply (docs/design.md, "The Run tab").
    const { plan, items } = foldRun(run.entries);
    const live = isLive(run);
    const asking = items.filter((item) => item.type === "permission" && item.answer === undefined);
    const steps = items.filter((item) => item.type !== "message" && !asking.includes(item));
    const messages = items.filter((item) => item.type === "message");
    const wanted = [];
    const itemEntry = (item, index) => [
      itemKey(item, index),
      signature(item, live),
      item.output ?? null,
      (open) => itemView(item, open),
    ];
    for (const item of asking) wanted.push(itemEntry(item, items.indexOf(item)));
    if (plan?.length) wanted.push(["plan", JSON.stringify(plan), null, () => planView(plan)]);

    const cards = run.notes.map((id) => noteCard(id));
    wanted.push(["notes", JSON.stringify(cards), null, () => notesView(cards)]);

    if (steps.length) {
      const cut = !live && !allSteps && steps.length > STEPS_SHOWN + 1;
      wanted.push(["steps", JSON.stringify([steps.length, cut]), null, () => stepsLabel(steps.length, cut)]);
      for (const item of cut ? steps.slice(-STEPS_SHOWN) : steps) {
        wanted.push(itemEntry(item, items.indexOf(item)));
      }
    }
    if (messages.length) {
      const texts = messages.map((message) => message.text);
      const cut = !live && !wholeReply && texts.join("").length > REPLY_CUT;
      wanted.push(["reply", JSON.stringify([texts, live, cut]), null, () => replyView(texts, live, cut)]);
    }
    if (run.problem) wanted.push(["problem", run.problem, null, () => el("div", "run-notice", run.problem)]);
    if (wanted.length === 0) {
      wanted.push(["empty", statusLine(run).text, null, () => el("div", "empty", statusLine(run).text)]);
    }

    const next = new Map();
    // `data` is the object the signature leaves out, compared by identity: a tool's output,
    // which an update replaces whole and entries keep from one fetch to the next.
    const nodes = wanted.map(([key, sig, data, build]) => {
      const drawn = views.get(key);
      const same = drawn?.sig === sig && drawn.data === data;
      // A redrawn tool opens, filled at once, as the one it replaces was.
      const node = same ? drawn.node : build(drawn?.node.open === true);
      next.set(key, { sig, data, node });
      return node;
    });
    views = next;
    // Only nodes out of place move; the rest, the top of a long run, are left untouched.
    nodes.forEach((node, index) => {
      if (panelEl.childNodes[index] !== node) panelEl.insertBefore(node, panelEl.childNodes[index] ?? null);
    });
    while (panelEl.childNodes.length > nodes.length) panelEl.lastChild.remove();
    reveal();
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
        // finished ones, and any text selected in them, alone.
        return JSON.stringify([item.title, item.tool, item.status, item.places, underWay(item) && live]);
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
    if (!note) return { id, where: "deleted", kind: "stopped", label: "Deleted", tone: "idle", said: null };
    const { start, end } = note.lines;
    const where = `${note.path.split("/").at(-1)}:${start === end ? start : `${start}-${end}`}`;
    const status = noteStatus(note, run);
    return { id, where, ...status, said: agentSaid(run, id) };
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
      const button = el("button", `run-note ${card.kind}`);
      button.type = "button";
      const head = el("span", "run-note-head");
      head.append(
        statusIcon(card.kind),
        el("span", `run-note-status ${card.tone}`, card.label),
        el("span", "run-note-where", card.where),
        el("span", "note-id", card.id),
      );
      button.append(head);
      if (card.said) button.append(el("span", "run-note-said", card.said));
      button.addEventListener("click", () => openNote(card.id));
      view.append(button);
    }
    return view;
  }

  function stepsLabel(count, cut) {
    const label = el("div", "run-label run-steps-label");
    label.append(el("span", "", count === 1 ? "Step" : `Steps · ${count}`));
    if (cut) {
      const more = el("button", "run-more", "Show all");
      more.type = "button";
      more.addEventListener("click", () => {
        allSteps = true;
        render();
      });
      label.append(more);
    }
    return label;
  }

  function replyView(texts, live, cut) {
    const view = el("section", "run-reply");
    view.append(el("div", "run-label", live ? `${run.agent} is writing` : `${run.agent}'s reply`));
    const body = el("div", cut ? "run-reply-body cut" : "run-reply-body");
    for (const text of texts) body.append(markdownView(text));
    view.append(body);
    if (cut) {
      const more = el("button", "button run-whole", "Show the whole reply");
      more.type = "button";
      more.addEventListener("click", () => {
        wholeReply = true;
        render();
      });
      view.append(more);
    }
    return view;
  }

  function planView(plan) {
    const view = el("section", "run-plan");
    const progress = planProgress(run);
    const label = el("div", "run-label");
    label.append(el("span", "", `Plan · ${progress.done} of ${progress.total}`));
    const bar = el("div", "run-progress");
    const fill = el("div", "run-progress-fill");
    fill.style.width = `${(100 * progress.done) / progress.total}%`;
    bar.append(fill);
    view.append(label, bar);
    for (const step of plan) {
      const row = el("div", `plan-step ${step.status}`);
      const mark = el("span", "plan-mark", { completed: "✓", in_progress: "●" }[step.status] ?? "○");
      mark.setAttribute("aria-hidden", "true");
      row.append(mark, el("span", "", step.content));
      view.append(row);
    }
    return view;
  }

  // `open`: an expandable item is drawn open, as the node it replaces was.
  function itemView(item, open) {
    switch (item.type) {
      case "message":
        return markdownView(item.text);
      case "thought": {
        const details = el("details", "run-thought");
        details.append(el("summary", "", "Thinking"), el("div", "", item.text.trim()));
        details.open = open;
        return details;
      }
      case "tool":
        return toolView(item, open);
      case "permission":
        return permissionView(item);
      case "error":
        return el("div", "run-error", item.text);
      default:
        return el("div", "run-notice", item.text);
    }
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

  // `open`: the tool is drawn open, with its output, as the node it replaces was.
  function toolView(tool, open) {
    const row = toolRow(tool, isLive(run));
    const diffs = tool.output.filter((part) => part.type === "diff");
    const texts = tool.output.filter((part) => part.type === "text");
    const line = el("div", `tool-line${row.running ? " running" : ""}`);
    line.append(el("span", "tool-verb", row.verb), el("span", "tool-target", row.target));
    const tone = row.state === "failed" || row.state === "unfinished" ? ` ${row.state}` : "";
    const state = el("span", `tool-state${tone}`, row.state);
    line.append(state);
    if (diffs.length === 0 && texts.length === 0) {
      line.title = tool.title;
      return line;
    }
    // A call with output opens to show it: its diffs, or what it printed. They are built when
    // first opened: a run can carry many large diffs nobody opens.
    const details = el("details", "tool");
    const summary = el("summary");
    summary.append(line);
    details.append(summary);
    let built = false;
    const fill = () => {
      if (built) return;
      built = true;
      for (const diff of diffs) details.append(diffView(diff));
      for (const text of texts) details.append(el("pre", "tool-output", text.text));
    };
    details.addEventListener("toggle", () => {
      if (details.open) fill();
    });
    if (open) {
      fill();
      details.open = true;
    }
    return details;
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
