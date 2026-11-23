// The run panel: the tray's Run tab and the agent status at the foot of the sidebar. It
// fetches the latest run from `api/run` and draws it; ui/run.js has the rules.
//
// The Rust process calls `window.postil.runChanged()` whenever the run changes; the panel
// then fetches the entries it has not seen. Fetches never overlap: a change that arrives
// during one is fetched right after it.

import {
  diffLines,
  elapsed,
  foldRun,
  isLive,
  keyFor,
  markdownBlocks,
  openPermission,
  permissionKeys,
  statusLine,
  toolRow,
} from "./run.js";

// How long after the first Stop a second one ends the agent: src/run.rs, FORCE_AFTER.
const FORCE_AFTER_MS = 3000;

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// `panelEl` is the Run tab's body, `headerEl` the tray header's run part, `statusEl` the
// sidebar's agent status. `onChange(run, previous)` is called after every fetch that
// changed the run, so the page can follow it (switch tabs, re-read files).
export function runPanel({ panelEl, headerEl, statusEl, onChange, onError }) {
  let run = null;
  let fetching = false;
  let again = false;
  let clock = null;
  // When this page first saw the run stopping: from FORCE_AFTER_MS on, Stop ends the agent.
  let stoppingSince = null;

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
    } catch (error) {
      onError(String(error.message ?? error));
    } finally {
      fetching = false;
    }
  }

  async function post(route, body) {
    const response = await fetch(route, { method: "POST", body: JSON.stringify(body ?? {}) });
    if (!response.ok) throw new Error(await response.text());
    await sync();
  }

  // Stop asks the agent to stop. Once it has been asked for FORCE_AFTER_MS, Stop ends it
  // (src/run.rs, "Stopping"); sooner, the Rust process takes it as the same request.
  function stop() {
    if (!isLive(run)) return Promise.resolve();
    if (run.status === "cancelling" && !forceable()) return Promise.resolve();
    return post("api/run/cancel").catch((error) => onError(error.message));
  }

  function forceable() {
    return stoppingSince !== null && Date.now() - stoppingSince >= FORCE_AFTER_MS;
  }

  function answer(request, option) {
    return post("api/run/permission", { request: request.request, option }).catch((error) =>
      onError(error.message),
    );
  }

  // `a`, Shift-`a` and `r` answer the open permission request. Returns whether the key was
  // used.
  function keydown(event) {
    if (event.metaKey || event.ctrlKey || event.altKey) return false;
    const request = run && openPermission(foldRun(run.entries).items);
    if (!request) return false;
    const option = permissionKeys(request.options).get(event.key);
    if (option === undefined) return false;
    event.preventDefault();
    answer(request, option);
    return true;
  }

  function render() {
    renderStatus();
    renderHeader();
    if (!run) {
      panelEl.replaceChildren(el("div", "empty", "No run yet. Send notes to an agent to start one."));
      return;
    }
    // The panel is redrawn whole; it keeps its scroll, and follows the end while it is there.
    const atEnd = panelEl.scrollHeight - panelEl.scrollTop - panelEl.clientHeight < 24;
    const top = panelEl.scrollTop;
    const { plan, items } = foldRun(run.entries);
    const parts = [];
    if (plan?.length) parts.push(planView(plan));
    parts.push(...items.map(itemView));
    if (run.problem) parts.push(el("div", "run-notice", run.problem));
    if (parts.length === 0) parts.push(el("div", "empty", statusLine(run).text));
    panelEl.replaceChildren(...parts);
    panelEl.scrollTop = atEnd ? panelEl.scrollHeight : top;
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
    const time = el("span", "run-time");
    const tick = () => {
      time.textContent = elapsed(Date.now() / 1000 - run.startedAt);
    };
    tick();
    const title = el("span", "run-title");
    title.append(el("strong", "", run.agent), " · ", time);
    const parts = [title];
    if (isLive(run)) {
      const stopping = run.status === "cancelling";
      if (!stopping) stoppingSince = null;
      else stoppingSince ??= Date.now();
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
    } else {
      const { text, tone } = statusLine(run);
      time.textContent = run.id;
      parts.push(el("span", `run-end ${tone}`, text));
    }
    headerEl.replaceChildren(...parts);
  }

  function planView(plan) {
    const view = el("section", "run-plan");
    view.append(el("div", "run-label", "Plan"));
    for (const step of plan) {
      const row = el("div", `plan-step ${step.status}`);
      const mark = el("span", "plan-mark", { completed: "✓", in_progress: "●" }[step.status] ?? "○");
      mark.setAttribute("aria-hidden", "true");
      row.append(mark, el("span", "", step.content));
      view.append(row);
    }
    return view;
  }

  function itemView(item) {
    switch (item.type) {
      case "message":
        return markdownView(item.text);
      case "thought": {
        const details = el("details", "run-thought");
        details.append(el("summary", "", "Thinking"), el("div", "", item.text.trim()));
        return details;
      }
      case "tool":
        return toolView(item);
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

  function toolView(tool) {
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
    // A call with output opens to show it: its diffs, or what it printed.
    const details = el("details", "tool");
    const summary = el("summary");
    summary.append(line);
    details.append(summary);
    for (const diff of diffs) details.append(diffView(diff));
    for (const text of texts) details.append(el("pre", "tool-output", text.text));
    return details;
  }

  function diffView(diff) {
    const view = el("pre", "tool-diff");
    const marks = { same: "  ", del: "- ", add: "+ " };
    for (const line of diffLines(diff.oldText ?? "", diff.newText)) {
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
  };
}
