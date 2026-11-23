// The changes view: what a run, or a thread of runs, changed, shown in the code pane over the
// open file (docs/design.md, "The changes view"). One unified diff per file, from the snapshot
// before the first run to the one after the last (`GET api/run/changes`), with each note
// beside the line its lines end on. A click on a line number of the new side opens that file
// at that line with the composer, so the engineer writes the next note on the change itself.
// ui/patch.js reads the diff.

import { el } from "./dom.js";
import { notesByLine, parsePatch } from "./patch.js";

// How many lines of one file are drawn before the rest waits for a click. Arbitrary: a few
// screens of a review, well below the thousands a regenerated lockfile brings, each of which
// is four elements.
const LINES_SHOWN = 400;

// `viewEl` is the view's element. `setHeader(title, meta)` fills the code pane's header;
// `notes()` gives the notes; `noteCard(note)` builds a note's card, which opens its dialog;
// `addNote(path, line)` writes a note on 1-based line `line` of `path`; `onError(text)` shows
// a failure; `onClose()` runs when the view closes, to give the pane back to the file.
export function changesView({ viewEl, setHeader, notes, noteCard, addNote, onError, onClose }) {
  // What is shown: `{ from, to }`, or null while the view is closed.
  let shown = null;
  let request = 0;

  async function show(from, to) {
    shown = { from, to };
    const asked = ++request;
    viewEl.hidden = false;
    setHeader(title(from, to), "");
    viewEl.replaceChildren(el("div", "empty", "Reading the changes…"));
    viewEl.focus();
    let reply;
    try {
      const query = `from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`;
      const response = await fetch(`api/run/changes?${query}`);
      reply = response.ok ? await response.json() : { problem: await response.text() };
    } catch (error) {
      reply = { problem: String(error.message ?? error) };
    }
    // A newer show, or a close, while this was in flight.
    if (asked !== request || !shown) return;
    if (reply.problem) {
      viewEl.replaceChildren(el("div", "empty", reply.problem));
      onError(reply.problem);
      return;
    }
    draw(parsePatch(reply.patch), reply.after === null);
  }

  function title(from, to) {
    return from === to ? `Changes in ${from}` : `Changes in ${from} to ${to}`;
  }

  function draw(files, sinceNow) {
    const added = files.reduce((sum, file) => sum + file.added, 0);
    const removed = files.reduce((sum, file) => sum + file.removed, 0);
    const count = files.length === 1 ? "1 file" : `${files.length} files`;
    setHeader(title(shown.from, shown.to), `${count} · +${added} -${removed}`);
    const parts = [];
    // The run is under way, or ended without a snapshot after it: the files as they are now
    // stand in for it, and may hold changes made since.
    if (sinceNow) {
      parts.push(
        el(
          "div",
          "changes-notice",
          "Compared with the files as they are now: the run has no snapshot after it.",
        ),
      );
    }
    if (files.length === 0) parts.push(el("div", "empty", "No file changed."));
    const all = notes();
    for (const file of files) parts.push(fileView(file, all));
    viewEl.replaceChildren(...parts);
  }

  function fileView(file, all, limit = LINES_SHOWN) {
    const part = el("section", "changes-file");
    const head = el("header", "changes-file-head");
    const status = { added: "new", deleted: "deleted", modified: "" }[file.status];
    head.append(el("span", "changes-path", file.path));
    if (status) head.append(el("span", `changes-status ${file.status}`, status));
    head.append(el("span", "changes-stats", `+${file.added} -${file.removed}`));
    part.append(head);
    if (file.binary) {
      part.append(el("div", "changes-notice", "A binary file: its change is not shown."));
      return part;
    }
    const placed = notesByLine(file, all);
    const rows = el("div", "changes-rows");
    let drawn = 0;
    const total = file.hunks.reduce((sum, hunk) => sum + hunk.lines.length, 0);
    for (const hunk of file.hunks) {
      if (drawn >= limit) break;
      rows.append(el("div", "changes-hunk", hunk.header));
      for (const line of hunk.lines) {
        if (drawn >= limit) break;
        drawn += 1;
        rows.append(lineRow(file, line));
        if (line.new !== null && placed.has(line.new)) {
          const cards = el("div", "changes-notes");
          for (const note of placed.get(line.new)) cards.append(noteCard(note));
          rows.append(cards);
        }
      }
    }
    part.append(rows);
    if (drawn < total) {
      const more = el("button", "button changes-more", `Show the other ${total - drawn} lines`);
      more.type = "button";
      more.addEventListener("click", () => part.replaceWith(fileView(file, all, Infinity)));
      part.append(more);
    }
    return part;
  }

  function lineRow(file, line) {
    const row = el("div", `changes-line ${line.kind}`);
    const sign = { add: "+", del: "-", context: " " }[line.kind];
    row.append(el("span", "changes-number", line.old === null ? "" : String(line.old)));
    // Only a line that is in the file now can carry a new note.
    if (line.new !== null) {
      const button = el("button", "changes-number new", String(line.new));
      button.type = "button";
      button.title = `Write a note on ${file.path}:${line.new}`;
      button.addEventListener("click", () => {
        close();
        addNote(file.path, line.new);
      });
      row.append(button);
    } else row.append(el("span", "changes-number", ""));
    row.append(el("span", "changes-sign", sign), el("span", "changes-text", line.text));
    return row;
  }

  function close() {
    if (!shown) return;
    shown = null;
    request += 1;
    viewEl.hidden = true;
    viewEl.replaceChildren();
    onClose();
  }

  viewEl.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      close();
    }
  });

  return { show, close, isOpen: () => shown !== null };
}
