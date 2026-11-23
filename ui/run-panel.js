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
  permissionKeys,
  statusLine,
  toolRow,
} from "./run.js";

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
  // When Stop starts to end the agent, by this page's clock; null unless it is stopping.
  let forceAt = null;
  // The drawn items by key, each with the signature it was drawn from: an item whose
  // signature is unchanged keeps its node, so an open diff stays open and a selection stays.
  let views = new Map();
  let drawnRun = null;
  // Whether the panel follows the end of the run: true until the engineer scrolls up.
  let follow = true;
  panelEl.addEventListener("scroll", () => {
    if (panelEl.clientHeight > 0) follow = atEnd();
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
    } catch (error) {
      onError(String(error.message ?? error));
      // A change that arrived during the failed fetch is fetched once more.
      if (again) setTimeout(sync, 1000);
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

  // `a`, Shift-`a` and `r` answer the open permission request. Returns whether the key was
  // used. The physical key decides, not the character: with Caps Lock on, `a` types "A",
  // which must not allow always. A held key answers once.
  function keydown(event) {
    if (event.metaKey || event.ctrlKey || event.altKey || event.repeat) return false;
    const request = run && openPermission(foldRun(run.entries).items);
    if (!request) return false;
    const key = { KeyA: event.shiftKey ? "A" : "a", KeyR: event.shiftKey ? null : "r" }[event.code];
    if (!key) return false;
    const option = permissionKeys(request.options).get(key);
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
    if (drawnRun !== run.id) {
      views = new Map();
      drawnRun = run.id;
      follow = true;
    }
    const { plan, items } = foldRun(run.entries);
    const live = isLive(run);
    const wanted = [];
    if (plan?.length) wanted.push(["plan", JSON.stringify(plan), () => planView(plan)]);
    items.forEach((item, index) => {
      wanted.push([itemKey(item, index), signature(item, live), () => itemView(item)]);
    });
    if (run.problem) wanted.push(["problem", run.problem, () => el("div", "run-notice", run.problem)]);
    if (wanted.length === 0) {
      wanted.push(["empty", statusLine(run).text, () => el("div", "empty", statusLine(run).text)]);
    }

    const next = new Map();
    const nodes = wanted.map(([key, sig, build]) => {
      const drawn = views.get(key);
      const node = drawn?.sig === sig ? drawn.node : build();
      // A redrawn details element opens as the one it replaces was.
      if (drawn && drawn.node !== node && drawn.node.open && node.tagName === "DETAILS") {
        node.open = true;
      }
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
  }

  // Scrolls to the end while the panel follows it. Measured only while the panel shows: a
  // hidden panel has no height, and would read as both at the end and scrolled to the top.
  function reveal() {
    if (follow && panelEl.clientHeight > 0) panelEl.scrollTop = panelEl.scrollHeight;
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
        return JSON.stringify([item.title, item.tool, item.status, item.places, item.output.length, live]);
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
      // The Rust process says how long until it would force the agent, so a reloaded page
      // offers Force stop as soon as the process would act on it.
      if (!stopping) forceAt = null;
      else forceAt ??= Date.now() + (run.forceIn ?? 0);
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
    // A call with output opens to show it: its diffs, or what it printed. They are built when
    // first opened: a run can carry many large diffs nobody opens.
    const details = el("details", "tool");
    const summary = el("summary");
    summary.append(line);
    details.append(summary);
    let built = false;
    details.addEventListener("toggle", () => {
      if (!details.open || built) return;
      built = true;
      for (const diff of diffs) details.append(diffView(diff));
      for (const text of texts) details.append(el("pre", "tool-output", text.text));
    });
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
