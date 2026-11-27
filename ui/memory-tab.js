// The tray's Memory tab, and the run dialog's Memory section (docs/review-assist.md, "Postil
// shows memory"): the memories hippius-mem recalls for the open file, or for a run's prompt and
// changed paths, one card each with the summary, the author and the age. A card opens the
// memory's dialog (memory-dialog.js). memory.js has the rules.

import { el } from "./dom.js";
import { memoryAge, partitionPointers, pollMemory, replyProblem, shortAuthor } from "./memory.js";

// What a recall's reply shows: its cards, the pending line, or why there are none. `nothing`
// is the line for a recall that found nothing strong. `openSettings`, when given, is offered
// when hippius-mem cannot run.
export function recallView(reply, { nothing, openMemory, openSettings = null, now = Date.now() }) {
  if (reply.state === "pending") return [el("div", "empty", "Checking team memory…")];
  if (reply.state !== "ready") {
    const line = el("div", "empty", replyProblem(reply));
    if (reply.state === "unavailable" && openSettings) {
      const button = el("button", "button memory-settings", "Settings");
      button.type = "button";
      button.addEventListener("click", () => openSettings());
      line.append(" ", button);
    }
    return [line];
  }
  const { strong, weak, empty } = partitionPointers(reply.pointers);
  const parts = [];
  if (empty) parts.push(el("div", "empty", nothing));
  parts.push(...strong.map((pointer) => memoryCard(pointer, openMemory, now)));
  if (weak.length) {
    const group = el("div", "memory-weak");
    group.append(
      el("div", "memory-group-label", "Possibly related"),
      ...weak.map((pointer) => memoryCard(pointer, openMemory, now)),
    );
    parts.push(group);
  }
  return parts;
}

function memoryCard(pointer, openMemory, now) {
  const button = el("button", "note memory-card");
  button.type = "button";
  const head = el("div", "note-where");
  const author = el("span", "note-place", shortAuthor(pointer.author));
  author.title = pointer.author;
  head.append(author, el("span", "note-id", memoryAge(pointer.updated, now)));
  button.append(head, el("div", "note-body", pointer.summary));
  button.addEventListener("click", () => openMemory(pointer.id));
  return button;
}

// The tab: `panelEl` holds the cards, `headerEl` the tray header's part with Refresh.
// `currentFile()` gives the open file as `{ path, node }` (`node` the selected node's label,
// or null), or null with none; `openMemory(id)` opens a memory's dialog; `openSettings()` the
// settings. No recall runs while the tab is hidden; one runs when it shows and when a file
// opens while it shows, and its answer is kept per file for the session (`show`, `fileOpened`).
export function memoryTab({ panelEl, headerEl, currentFile, openMemory, openSettings }) {
  // The answers kept, by file and node.
  const kept = new Map();
  // The recall being asked: `{ key, stop }`.
  let asking = null;
  let shown = false;
  let refreshing = false;

  const status = el("span", "meta memory-status");
  const refresh = el("button", "button memory-refresh", "Refresh");
  refresh.type = "button";
  refresh.title = "Pull the team's latest notes, then ask again";
  refresh.addEventListener("click", startRefresh);
  headerEl.replaceChildren(status, refresh);

  const keyOf = (query) => `${query.path}\0${query.node ?? ""}`;
  const draw = (reply) =>
    panelEl.replaceChildren(
      ...recallView(reply, {
        nothing: "Nothing recorded for this file.",
        openMemory,
        openSettings,
      }),
    );

  function stopAsking() {
    asking?.stop();
    asking = null;
  }

  function recall() {
    if (!shown) return;
    const query = currentFile();
    if (!query) {
      stopAsking();
      panelEl.replaceChildren(el("div", "empty", "Open a file to see what the team recorded."));
      return;
    }
    const key = keyOf(query);
    const known = kept.get(key);
    if (known) {
      stopAsking();
      draw(known);
      return;
    }
    if (asking?.key === key) return;
    stopAsking();
    let url = `api/memory/recall?path=${encodeURIComponent(query.path)}`;
    if (query.node) url += `&node=${encodeURIComponent(query.node)}`;
    const stop = pollMemory(url, {
      onReply: (reply) => {
        if (reply.state !== "pending") {
          asking = null;
          if (reply.state === "ready") kept.set(key, reply);
        }
        draw(reply);
      },
    });
    asking = { key, stop };
  }

  // Refresh: pulls the team's latest notes, then drops what was kept and asks again.
  function startRefresh() {
    if (refreshing) return;
    refreshing = true;
    refresh.disabled = true;
    status.textContent = "Refreshing…";
    pollMemory("api/memory/refresh", {
      method: "POST",
      onReply: (reply) => {
        if (reply.state === "pending") return;
        refreshing = false;
        refresh.disabled = false;
        status.textContent = replyProblem(reply) ?? "";
        kept.clear();
        stopAsking();
        recall();
      },
    });
  }

  return {
    show() {
      shown = true;
      recall();
    },
    hide() {
      shown = false;
      stopAsking();
    },
    fileOpened: recall,
  };
}

// The run dialog's Memory section: the recall for the thread that ends with a run, asked once
// while the dialog is open, again every MEMORY_POLL_MS while pending. `onSettled()` is called
// when an answer comes in, for the dialog to draw again; `key(id)` changes then. `stop()` ends
// every ask and forgets the answers: the dialog closed.
export function runMemory({ openMemory, onSettled }) {
  const replies = new Map();
  const asking = new Map();

  function replyOf(id) {
    const known = replies.get(id);
    if (known && known.state !== "pending") return known;
    if (!asking.has(id)) {
      const stop = pollMemory(`api/memory/recall?run=${encodeURIComponent(id)}`, {
        onReply: (reply) => {
          replies.set(id, reply);
          if (reply.state === "pending") return;
          asking.delete(id);
          onSettled();
        },
      });
      asking.set(id, stop);
    }
    return known ?? { state: "pending" };
  }

  return {
    key: (id) => replyOf(id).state,
    // The section's content, or null when hippius-mem cannot run here: the section is left
    // out then.
    view(id) {
      const reply = replyOf(id);
      if (reply.state === "unavailable") return null;
      return recallView(reply, { nothing: "Nothing recorded for this run.", openMemory });
    },
    stop() {
      for (const stop of asking.values()) stop();
      asking.clear();
      replies.clear();
    },
  };
}
