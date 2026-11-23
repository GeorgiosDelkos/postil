// The note detail: a dialog over the window that shows one note and what happened to it, from
// a card's click (docs/design.md, "Notes and their status"). It is drawn from the note and its
// run (status.js), and drawn again when either changes while it is open.

import { isLive, statusLine } from "./run.js";
import { agentSaid, fileStats, noteHistory, noteStatus } from "./status.js";

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// The status icon of a card or the dialog: its shape is CSS (`.status-icon.<kind>`).
export function statusIcon(kind) {
  const glyph = { needs: "!", addressed: "✓", silent: "–", failed: "×" }[kind] ?? "";
  const icon = el("span", `status-icon ${kind}`, glyph);
  icon.setAttribute("aria-hidden", "true");
  return icon;
}

// `dialogEl` is the `<dialog>`. `find(id)` gives `{ note, run }` for a note id, or null once
// the note is gone; `canSend(note)` whether it can be sent now. The callbacks act on the note:
// `showInCode(note)`, `send(note, { review })` (review: open the send step with it alone
// ticked), `remove(note)`, and `showRun()`.
export function noteDialog(dialogEl, { find, canSend, showInCode, send, remove, showRun }) {
  let shownId = null;
  // What the dialog was drawn from: drawn again only when this changes, so a run's every
  // token leaves the dialog, its scroll and its focus alone.
  let drawn = null;

  dialogEl.addEventListener("close", () => {
    shownId = null;
    drawn = null;
  });
  dialogEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.metaKey) {
      event.preventDefault();
      dialogEl.querySelector(".note-dialog-send:not(:disabled)")?.click();
    }
  });

  function open(id) {
    shownId = id;
    drawn = null;
    draw();
    if (!dialogEl.open) dialogEl.showModal();
    dialogEl.querySelector(".note-dialog-show")?.focus();
  }

  // Draws the dialog again if what it shows changed; closes it when its note is gone.
  function draw() {
    if (shownId === null) return;
    const found = find(shownId);
    if (!found) {
      dialogEl.close();
      return;
    }
    const { note, run } = found;
    const key = JSON.stringify([note, run?.id, run?.status, run?.total, canSend(note)]);
    if (key === drawn) return;
    drawn = key;
    const focused = document.activeElement?.className;
    dialogEl.replaceChildren(header(note, run), body(note, run), footer(note, run));
    // A redraw keeps the focus on the button that had it.
    if (focused) dialogEl.getElementsByClassName(focused)[0]?.focus();
  }

  function header(note, run) {
    const status = noteStatus(note, run);
    const head = el("header", "note-dialog-header");
    const title = el("h2", `note-dialog-title ${status.tone}`, status.label);
    title.id = "note-dialog-title";
    const close = el("button", "button note-dialog-close", "esc");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => dialogEl.close());
    const show = el("button", "button note-dialog-show", "Show in code");
    show.type = "button";
    show.addEventListener("click", () => {
      dialogEl.close();
      showInCode(note);
    });
    const actions = el("span", "note-dialog-actions");
    actions.append(show, close);
    head.append(statusIcon(status.kind), title, el("span", "note-dialog-where", where(note)), actions);
    return head;
  }

  function body(note, run) {
    const main = el("div", "note-dialog-body");
    main.append(section("Your note", el("div", "note-dialog-note", note.body)));

    const lost = note.anchor === "lost";
    const stats = run && fileStats(note, run);
    const label = lost ? "Its lines when written; they are no longer in the file" : "Its lines";
    const lines = section(label, snippetView(note));
    if (stats) lines.firstChild.append(el("span", "note-dialog-stats", `+${stats.added} -${stats.removed}`));
    main.append(lines);

    if (run) {
      main.append(section(`What ${run.agent} said about ${note.id}`, said(note, run)));
      main.append(section(`History · ${run.id} · ${run.agent}`, history(note, run)));
    }
    return main;
  }

  function said(note, run) {
    const line = agentSaid(run, note.id);
    if (line !== null) return el("div", "note-dialog-said", line);
    const words = {
      starting: `${run.agent} is starting.`,
      working: `${run.agent} is working on it. Its line comes when the run ends.`,
      needs: `${run.agent} is waiting for you in the run.`,
      silent: `Nothing: the run ended without a line for ${note.id}.`,
      stopped: "Nothing: the run was stopped before the agent wrote its summary.",
      failed: run.problem ?? failure(run) ?? "The run failed.",
    }[noteStatus(note, run).kind];
    const text = el("div", "note-dialog-said muted", words ?? "");
    if (run.status === "waiting") {
      const go = el("button", "button note-dialog-run", "Open the run");
      go.type = "button";
      go.addEventListener("click", () => {
        dialogEl.close();
        showRun();
      });
      text.append(" ", go);
    }
    return text;
  }

  function history(note, run) {
    const list = el("ol", "note-dialog-history");
    const others = run.notes.filter((id) => id !== note.id);
    const sentWith = others.length ? ` with ${others.join(", ")}` : " alone";
    list.append(el("li", "", `Sent to ${run.agent}${sentWith}`));
    for (const step of noteHistory(note, run)) {
      const item = el("li", step.kind === "tool" ? "tool" : "step");
      if (step.kind === "tool") {
        item.append(el("span", "verb", step.verb), el("span", "target", step.target));
        if (step.state) item.append(el("span", "state", step.state));
      } else {
        item.append(el("span", "verb", step.status === "completed" ? "done" : "plan"), el("span", "target", step.text));
      }
      list.append(item);
    }
    if (!isLive(run)) list.append(el("li", "end", `Run ended: ${statusLine(run).text}`));
    return list;
  }

  function footer(note, run) {
    const foot = el("footer", "note-dialog-footer");
    const remove = deleteButton(note);
    const sendable = canSend(note);
    const agent = run?.agent;
    const button = el("button", "button primary note-dialog-send");
    button.type = "button";
    button.disabled = !sendable;
    button.append(run && !isLive(run) && agent ? `Send again to ${agent}` : "Send", el("kbd", "", "⌘↵"));
    button.title = sendable
      ? "Sends this note alone to the default agent. Shift-click shows the prompt first."
      : "A run is under way, or the note's lines are no longer in the file.";
    button.addEventListener("click", (event) => {
      dialogEl.close();
      send(note, { review: event.shiftKey });
    });
    foot.append(remove, el("span", "spacer"), button);
    return foot;
  }

  // A note is the engineer's writing, so deleting takes two clicks, as on the card.
  function deleteButton(note) {
    const button = el("button", "button note-dialog-delete", "Delete note");
    button.type = "button";
    button.addEventListener("click", () => {
      if (button.dataset.confirming) {
        dialogEl.close();
        remove(note);
        return;
      }
      button.dataset.confirming = "1";
      button.textContent = "Click again to delete";
    });
    button.addEventListener("blur", () => {
      delete button.dataset.confirming;
      button.textContent = "Delete note";
    });
    return button;
  }

  return { open, draw, isOpen: () => dialogEl.open };
}

function section(label, content) {
  const part = el("section", "note-dialog-section");
  const title = el("div", "note-dialog-label");
  title.append(el("span", "", label));
  part.append(title, content);
  return part;
}

// The note's lines as they were when it was written, numbered from where they are now.
function snippetView(note) {
  const view = el("pre", "note-dialog-snippet");
  note.snippet.split("\n").forEach((text, index) => {
    const row = el("span", "row");
    row.append(el("span", "number", String(note.lines.start + index)), el("span", "text", text || " "));
    view.append(row);
  });
  return view;
}

function where(note) {
  const { start, end } = note.lines;
  return `${note.id} · ${note.path}:${start === end ? start : `${start}-${end}`}`;
}

function failure(run) {
  return run.entries.findLast((entry) => entry.kind === "failed")?.error ?? null;
}
