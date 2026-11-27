// The code pane: the open file as a virtual list, the selection of lines, copying, the
// right-click menu's actions on the code, and the composer that writes a note.

import { definitionsDialog } from "./definitions-dialog.js";
import { NO_DEFINITION, jumpHistory, nameAt } from "./definitions.js";
import { askWarning } from "./dialog-parts.js";
import { attachDictation } from "./dictation.js";
import { $, el, showToast } from "./dom.js";
import { onLines } from "./note-place.js";
import { shouldReveal } from "./tree.js";

// `.code-rows` has this much padding above the first and below the last line (app.css).
const ROWS_PADDING = 12;
// Lines rendered beyond each edge of the viewport. Arbitrary: enough that a fast flick
// does not show blank space before the next render, few enough that a render stays cheap.
const OVERSCAN = 40;
// The composer's height before it has been measured once. A guess: label, two text rows,
// actions, and margins.
const COMPOSER_HEIGHT_GUESS = 138;
// How long to wait before asking again for definitions while the index is first built, in
// ms: the owner's choice (2026-11-26), as the settings dialog asks again during its scan.
const DEFINITIONS_POLL_MS = 250;

// `state` is the page's state: the view keeps `file`, `wanted`, `selection` and `composer`,
// and reads `noteByLine`, `markByLine` and `mapMarkers`. `menu` is the context menu
// (context-menu.js) and `files` the file list (file-list.js), which `reveal`s the open file.
// The callbacks:
// - `indexNotes()` derives `noteByLine` and `noteCountByPath` again after the file or the
//   notes changed, and `indexMarks()` derives `markByLine` after the file changed;
// - `openMapNode(diagramId, nodeId)` opens the map at a node, a click on its stripe or chip;
// - `traceFromHere(path)` opens the map's Flows box, to trace a flow from the file at `path`;
// - `onActiveLine()` follows a change of the line the map follows (`activeLine`);
// - `deleteNote(note)` deletes a note, from the right-click menu;
// - `beforeOpen()` clears what covers the code pane before a file opens there;
// - `headerHeld()` says whether another view holds the pane's header, so a file read again
//   under it leaves the header alone;
// - `onComposerClosed()` redraws what a saved note changes outside the code;
// - `onAsked()` follows the run a question asked from the composer started;
// - `onFileOpened()` is told a file was opened, not read again (the Memory tab recalls for it).
export function codeView({
  state,
  codeEl,
  rowsEl,
  codeEmptyEl,
  menu,
  files,
  indexNotes,
  indexMarks,
  openMapNode,
  openNote,
  traceFromHere,
  onActiveLine,
  deleteNote,
  beforeOpen,
  headerHeld,
  onComposerClosed,
  onAsked,
  onFileOpened,
}) {
  // One code line's height in px: `--line-height` in app.css until the settings arrive, then
  // what they give (`setLineHeight`).
  let lineHeight = parseFloat(
    getComputedStyle(document.documentElement).getPropertyValue("--line-height"),
  );
  // The rendered window of lines: [first, last).
  let rendered = { first: 0, last: 0 };
  let composerHeight = COMPOSER_HEIGHT_GUESS;

  // The reply to `api/file`: { ok, body }, the body a file view or the reason it failed.
  async function fetchFile(path) {
    const response = await fetch(`api/file?path=${encodeURIComponent(path)}`);
    return { ok: response.ok, body: response.ok ? await response.json() : await response.text() };
  }

  // Opens `path` in the code pane. Returns whether it was shown: not when a draft note would
  // be lost (the composer is focused instead), and not when a newer open overtook this one.
  async function openFile(path) {
    return (await openFileFor(path, { onlyFound: false })) === "shown";
  }

  // As `openFile`, saying why a file was not shown: "held" (a draft note keeps the pane),
  // "superseded" (a newer open overtook this one), or, with `onlyFound`, "missing" (the file
  // cannot be read, and the open file stays, where `openFile` would show the reason instead).
  async function openFileFor(path, { onlyFound }) {
    if (state.composer?.text.trim()) {
      state.composer.node.querySelector("textarea").focus();
      return "held";
    }
    // The changes view and the send step's prompt cover the code pane: the file opened takes
    // their place.
    beforeOpen();
    state.composer = null;
    state.wanted = path;
    const reply = await fetchFile(path);
    if (state.wanted !== path) return "superseded";
    if (onlyFound && !reply.ok) {
      state.wanted = state.file?.path ?? null;
      return "missing";
    }
    showFile(path, reply, { keepView: false });
    onFileOpened();
    return "shown";
  }

  // Draws a reply of `fetchFile`. `keepView` is for the same file read again after it may have
  // changed on disk: the scroll position stays, and so does the selection while it still fits
  // in the file.
  function showFile(path, reply, { keepView }) {
    const previous = state.file?.path;
    const lines = reply.ok ? reply.body.lines.length : 0;
    const selected = Math.max(state.selection?.anchor ?? 0, state.selection?.head ?? 0);
    if (!keepView || selected >= lines) {
      state.selection = null;
    }
    if (reply.ok) {
      state.file = reply.body;
      codeEmptyEl.hidden = true;
      // The gutter fits the file's largest line number, and at least three digits so short
      // files keep one width.
      const digits = Math.max(3, String(lines).length);
      rowsEl.style.setProperty("--gutter-digits", String(digits));
    } else {
      state.file = null;
      codeEmptyEl.hidden = false;
      codeEmptyEl.textContent = reply.body;
    }

    indexNotes();
    indexMarks();
    // The changes view and the send step's prompt hold the header while they are open over the
    // file; a refresh of the file under them (on focus, or as a run moves on) leaves it alone.
    if (!headerHeld()) renderHeader(path);
    if (shouldReveal(previous, path, { keepView })) files.reveal(previous, path);
    if (!keepView) {
      codeEl.scrollTop = 0;
      held = null;
    }
    renderCode({ force: true });
    onActiveLine();
  }

  function renderHeader(path) {
    const parts = path.split("/");
    const crumbs = $("crumbs");
    crumbs.replaceChildren(
      parts
        .slice(0, -1)
        .map((part) => `${part} / `)
        .join(""),
    );
    crumbs.append(el("b", "", parts.at(-1)));
    $("file-meta").textContent = state.file ? `${state.file.lines.length} lines` : "";
  }

  // Gives the pane's header back to the open file when a view over it closes; clears it when
  // no file is open.
  function restoreHeader() {
    if (state.file) renderHeader(state.file.path);
    else {
      $("crumbs").replaceChildren();
      $("file-meta").textContent = "";
    }
  }

  // The lines of the map's node last shown in the code, tinted while that file is open:
  // `{ path, start, end }`, 0-based and inclusive, or null.
  let mapped = null;
  // The 0-based line of the open file the map follows in place of the first visible one, set
  // when the map showed its node's lines here, or null. Until the engineer scrolls, clicks or
  // types in the code: the scroll to those lines passes other nodes' lines on its way, and
  // ends with them a third of the way down, and the map must stay on the node clicked.
  let held = null;
  // The map follows the first visible line again, at once: a click or a key may move nothing
  // that would tell it.
  function release() {
    if (held === null) return;
    held = null;
    onActiveLine();
  }
  for (const type of ["wheel", "pointerdown", "keydown"]) {
    codeEl.addEventListener(type, release, { passive: true });
  }

  // Opens the file of the map's code reference `anchor` (diagram.js), with its lines a third
  // of the way down and tinted, or at the top for a whole file. A folder has no file: the
  // caller shows it in the file list.
  async function showAnchor(anchor) {
    const lines = anchor.target === "lines" ? anchor.lines : null;
    mapped = lines && { path: anchor.path, start: lines.start - 1, end: lines.end - 1 };
    if (state.file?.path !== anchor.path && !(await openFile(anchor.path))) return;
    held = lines && lines.start - 1;
    redecorateRows();
    const top = lines ? lineTop(lines.start - 1) - codeEl.clientHeight / 3 : 0;
    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
    codeEl.scrollTo({ top, behavior: reduce ? "auto" : "smooth" });
  }

  // Opens a line or file note's file, at the note's lines or at the top. A folder note has no
  // file: the file list shows it (file-list.js, `showFolder`).
  async function showNote(note) {
    if (state.file?.path !== note.path && !(await openFile(note.path))) return;
    // The note's lines, not the map's node, are what the pane shows now; a pane already there
    // does not scroll, so the map is told here.
    held = null;
    onActiveLine();
    // A third of the way down reads better than the very top edge.
    const top = onLines(note) ? lineTop(note.lines.start - 1) - codeEl.clientHeight / 3 : 0;
    codeEl.scrollTo({ top, behavior: "smooth" });
  }

  // ---- A virtual list ---------------------------------------------------------------------
  //
  // Only the lines near the viewport exist as elements. `.code-rows` gets top and bottom
  // padding equal to the height of the lines left out, so the scroll height is that of the
  // whole file. The composer sits in the flow after its last line and adds its height to
  // every line below it.

  function lineCount() {
    return state.file ? state.file.lines.length : 0;
  }

  // The y position of the top of line `index` inside the scroll content.
  function lineTop(index) {
    const pushed = state.composer && index > state.composer.end ? composerHeight : 0;
    return ROWS_PADDING + index * lineHeight + pushed;
  }

  // The line at scroll offset `y`: the inverse of `lineTop`.
  function lineAt(y) {
    const composer = state.composer;
    if (composer) {
      const composerTop = lineTop(composer.end) + lineHeight;
      if (y >= composerTop + composerHeight) y -= composerHeight;
      else if (y >= composerTop) return composer.end;
    }
    const index = Math.floor((y - ROWS_PADDING) / lineHeight);
    return Math.max(0, Math.min(lineCount(), index));
  }

  function renderCode({ force = false } = {}) {
    const count = lineCount();
    const visibleFirst = lineAt(codeEl.scrollTop);
    const visibleLast = Math.min(count, lineAt(codeEl.scrollTop + codeEl.clientHeight) + 1);

    // Scrolling inside the rendered window needs no new elements.
    const { first, last } = rendered;
    const covered = visibleFirst >= first && visibleLast <= last;
    const nearEdge =
      (visibleFirst - first < OVERSCAN / 2 && first > 0) ||
      (last - visibleLast < OVERSCAN / 2 && last < count);
    if (!force && covered && !nearEdge) return;

    const newFirst = Math.max(0, visibleFirst - OVERSCAN);
    const newLast = Math.min(count, visibleLast + OVERSCAN);
    rendered = { first: newFirst, last: newLast };

    const fragment = document.createDocumentFragment();
    for (let index = newFirst; index < newLast; index += 1) {
      fragment.append(lineRow(index));
      if (state.composer?.end === index) fragment.append(state.composer.node);
    }
    rowsEl.replaceChildren(fragment);
    setRowsPadding();
  }

  // Stands in for the lines that are not rendered, and for the composer when its line is
  // outside the rendered window.
  function setRowsPadding() {
    const { first, last } = rendered;
    const composer = state.composer;
    const above = composer && composer.end < first ? composerHeight : 0;
    const below = composer && composer.end >= last ? composerHeight : 0;
    rowsEl.style.paddingTop = `${ROWS_PADDING + first * lineHeight + above}px`;
    rowsEl.style.paddingBottom = `${ROWS_PADDING + (lineCount() - last) * lineHeight + below}px`;
    // How far a node's stripe reaches past the composer's line, to the line after it (app.css,
    // `.map-stripe.continues`).
    rowsEl.style.setProperty("--composer-height", `${composerHeight}px`);
  }

  function lineRow(index) {
    const row = el("div", "ln");
    row.dataset.line = String(index);
    // The + that hovering a line number shows, as in GitHub's and GitLab's diff views. It is
    // out of the Tab order: the keyboard way is a selection and `c`.
    const add = el("button", "add-note", "+");
    add.type = "button";
    add.tabIndex = -1;
    add.setAttribute("aria-label", `Add a note on line ${index + 1}`);
    row.append(add, el("span", "n", String(index + 1)));

    const code = el("span", "c");
    for (const [className, text] of state.file.lines[index]) {
      code.append(className ? el("span", className, text) : text);
    }
    row.append(code);

    decorateRow(row, index);
    return row;
  }

  // Applies selection, map and note styling to a row. Called for new rows and again when the
  // selection, the notes or the analysis change, so those changes do not rebuild the rows.
  function decorateRow(row, index) {
    row.classList.toggle("selected", isSelected(index));
    row.classList.toggle("jumped", jumped?.path === state.file?.path && jumped.index === index);
    const onMap = mapped?.path === state.file?.path && mapped.start <= index && index <= mapped.end;
    row.classList.toggle("mapped", onMap);

    // The stripe and chip are replaced only when their key changes, as the remark below is.
    const mark = state.mapMarkers ? (state.markByLine.get(index) ?? null) : null;
    const next = mark && state.composer?.end === index && state.markByLine.get(index + 1);
    const continues = Boolean(next) && next.nodeId === mark.nodeId;
    const markKey = mark && `${mark.key}:${continues}`;
    if ((row.dataset.markKey ?? null) !== markKey) drawMark(row, mark, continues, markKey);

    const noteMark = state.noteMarkByLine.get(index) ?? null;
    row.classList.toggle("noted", Boolean(noteMark));
    const nextNote = noteMark && state.composer?.end === index && state.noteMarkByLine.get(index + 1);
    const noteContinues = Boolean(nextNote) && nextNote.noteId === noteMark.noteId;
    const noteKey = noteMark && `${noteMark.key}:${noteContinues}`;
    if ((row.dataset.noteKey ?? null) !== noteKey) {
      drawNoteMark(row, noteMark, noteContinues, noteKey);
    }
  }

  // Draws `mark` (app.js, `markByLine`) on `row`, or nothing for null: the stripe at the
  // gutter's left edge, beside the line number rather than in it, whose press would start a
  // line selection; a flow's step number; and on the node's first line its chip, before any
  // note chip, and the count of other diagrams on the line. `continues` reaches the stripe
  // past the composer under the line to the node's next line.
  function drawMark(row, mark, continues, markKey) {
    for (const old of row.querySelectorAll(".map-stripe, .map-step, .map-chip, .map-chip-more")) {
      old.remove();
    }
    if (mark === null) {
      delete row.dataset.markKey;
      return;
    }
    row.dataset.markKey = markKey;
    const number = row.querySelector(".n");
    const stripe = el("button", "map-stripe");
    stripe.type = "button";
    stripe.tabIndex = -1;
    stripe.classList.toggle("outdated", mark.outdated);
    stripe.classList.toggle("continues", continues);
    stripe.title = mark.outdated ? `${mark.label} (outdated)` : mark.label;
    stripe.setAttribute("aria-label", `Map: ${mark.label}`);
    number.before(stripe);
    if (mark.step !== null) number.before(el("span", "map-step", String(mark.step)));
    if (!mark.chip) return;

    const noteChip = row.querySelector(".note-chip, .note-chip-more");
    const chip = el("button", "map-chip", mark.chip.label);
    chip.type = "button";
    chip.tabIndex = -1;
    chip.title = mark.chip.label;
    row.insertBefore(chip, noteChip);
    const { others } = mark.chip;
    if (others.length === 0) return;
    const more = el("button", "map-chip-more", `+${others.length}`);
    more.type = "button";
    more.tabIndex = -1;
    const diagrams = others.length === 1 ? "diagram" : "diagrams";
    more.setAttribute("aria-label", `${others.length} more ${diagrams} on this line`);
    row.insertBefore(more, noteChip);
  }

  // Draws `mark` (app.js, `noteMarkByLine`) on `row`, or nothing for null: the amber stripe
  // along the gutter's right edge spanning the note's lines, its clickable chip on its first
  // line, and a `+N` menu button when other notes overlap on that line.
  function drawNoteMark(row, mark, continues, noteKey) {
    for (const old of row.querySelectorAll(".note-stripe, .note-chip, .note-chip-more")) {
      old.remove();
    }
    if (mark === null) {
      delete row.dataset.noteKey;
      return;
    }
    row.dataset.noteKey = noteKey;
    const number = row.querySelector(".n");
    const stripe = el("button", "note-stripe");
    stripe.type = "button";
    stripe.tabIndex = -1;
    stripe.classList.toggle("resolved", mark.resolved);
    stripe.classList.toggle("continues", continues);
    stripe.title = mark.resolved ? `${mark.label} (resolved)` : mark.label;
    stripe.setAttribute("aria-label", `Note: ${mark.label}`);
    number.after(stripe);
    if (!mark.chip) return;

    const chip = el("button", "note-chip", mark.chip.label);
    chip.type = "button";
    chip.tabIndex = -1;
    chip.classList.toggle("resolved", mark.chip.resolved);
    chip.title = mark.chip.label;
    row.append(chip);
    const { others } = mark.chip;
    if (others.length === 0) return;
    const more = el("button", "note-chip-more", `+${others.length}`);
    more.type = "button";
    more.tabIndex = -1;
    const noun = others.length === 1 ? "note" : "notes";
    more.setAttribute("aria-label", `${others.length} more ${noun} on this line`);
    row.append(more);
  }

  function redecorateRows() {
    for (const row of rowsEl.querySelectorAll(".ln")) {
      decorateRow(row, Number(row.dataset.line));
    }
    onActiveLine();
  }

  // The line the map follows (docs/review-assist.md, "Code to diagram"), `{ path, line }` with
  // `line` 1-based: the composer's first line, else the selection's moving end, else the line
  // the map showed (`held`), else the first visible line. Null with no file or an empty one.
  function activeLine() {
    const count = lineCount();
    if (count === 0) return null;
    const index =
      state.composer?.start ?? state.selection?.head ?? held ?? lineAt(codeEl.scrollTop);
    return { path: state.file.path, line: Math.min(Math.max(index, 0), count - 1) + 1 };
  }

  // A new code font size: the line at the top stays at the top. The scroll offset is in
  // pixels, and the same offset at another line height would show another part of the file.
  function setLineHeight(height) {
    if (height === lineHeight) return;
    const top = lineAt(codeEl.scrollTop);
    lineHeight = height;
    if (!state.file) return;
    // Rows are placed by line height, so they are placed again, and the padding that stands
    // in for the other lines grows or shrinks with them. Only then can the scroll offset go
    // where the top line now is: before, the browser would cut it to the old height of the
    // file.
    renderCode({ force: true });
    codeEl.scrollTop = lineTop(top);
    renderCode({ force: true });
  }

  // ---- Selection --------------------------------------------------------------------------

  function selectedRange() {
    const selection = state.selection;
    if (!selection) return null;
    return {
      start: Math.min(selection.anchor, selection.head),
      end: Math.max(selection.anchor, selection.head),
    };
  }

  function isSelected(index) {
    const range = state.composer ?? selectedRange();
    return Boolean(range) && range.start <= index && index <= range.end;
  }

  function lineOfEvent(event) {
    const row = event.target.closest?.(".ln");
    return row ? Number(row.dataset.line) : null;
  }

  let dragging = false;

  rowsEl.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || !event.target.classList.contains("n")) return;
    const line = lineOfEvent(event);
    if (line === null || state.composer) return;

    event.preventDefault();
    codeEl.focus();
    // A text selection elsewhere would otherwise stay highlighted, and be what Copy copies.
    window.getSelection().removeAllRanges();
    const anchor = event.shiftKey && state.selection ? state.selection.anchor : line;
    state.selection = { anchor, head: line };
    dragging = true;
    redecorateRows();
  });

  rowsEl.addEventListener("mousemove", (event) => {
    if (!dragging) return;
    const line = lineOfEvent(event);
    if (line === null || line === state.selection.head) return;
    state.selection.head = line;
    redecorateRows();
  });

  // A drag across the code text selects the lines it touches, as a drag in the gutter does, so
  // the next `c` or right-click acts on them. The text stays highlighted for copying. A plain
  // click in the code clears the selection, as in an editor.
  //
  // While the button is down, `textDrag.lines` follows the browser's selection. It is kept
  // rather than read once at the release: a drag that scrolls the pane makes the virtual list
  // rebuild its rows, and the browser drops the selection with the nodes it was in. The
  // release itself may land anywhere, the sidebar included.
  let textDrag = null;

  rowsEl.addEventListener("mousedown", (event) => {
    // A press on the + or on a map or note stripe/chip starts neither a text selection nor a
    // drag, whose release would clear the selection: its click adds/opens the note or map.
    if (
      event.target.closest(
        ".add-note, .map-stripe, .map-chip, .map-chip-more, .note-stripe, .note-chip, .note-chip-more",
      )
    ) {
      event.preventDefault();
      return;
    }
    // Cmd-click on a name jumps to its definition. It starts no drag, so the selection stays
    // as it was: the mouseup below finds no `textDrag`.
    if (event.button === 0 && event.metaKey && event.target.closest(".c")) {
      event.preventDefault();
      jumpFromPoint(event.clientX, event.clientY);
      return;
    }
    if (event.button !== 0 || event.target.classList.contains("n") || state.composer) return;
    textDrag = { lines: null };
  });

  // The + on a line: a note on that line, or on the whole selection when the line is in it.
  // A map stripe or chip opens the map at its node; a note stripe or chip opens the note's
  // detail; and a `+N` button opens a menu of the other diagrams or notes on that line.
  rowsEl.addEventListener("click", (event) => {
    const line = lineOfEvent(event);
    const mark = state.markByLine.get(line);
    const noteMark = state.noteMarkByLine.get(line);
    if (event.target.closest(".map-stripe")) openMapNode(mark.diagramId, mark.nodeId);
    else if (event.target.closest(".map-chip")) openMapNode(mark.chip.diagramId, mark.chip.nodeId);
    else if (event.target.closest(".map-chip-more")) {
      const others = mark.chip.others.map((other) => ({
        label: other.label,
        detail: other.diagramTitle,
        run: () => openMapNode(other.diagramId, other.nodeId),
      }));
      menu.open(event.clientX, event.clientY, others);
    } else if (event.target.closest(".note-stripe")) openNote(noteMark.noteId);
    else if (event.target.closest(".note-chip")) openNote(noteMark.chip.noteId);
    else if (event.target.closest(".note-chip-more")) {
      const others = noteMark.chip.others.map((other) => ({
        label: other.noteId,
        detail: other.body,
        run: () => openNote(other.noteId),
      }));
      menu.open(event.clientX, event.clientY, others);
    } else if (event.target.closest(".add-note")) {
      const range = selectedRange();
      const inSelection = range && range.start <= line && line <= range.end;
      openComposer(inSelection ? range : { start: line, end: line });
    }
  });

  document.addEventListener("selectionchange", () => {
    if (textDrag) textDrag.lines = currentDragLines(textDrag.lines);
  });

  window.addEventListener("mouseup", (event) => {
    dragging = false;
    if (!textDrag || event.button !== 0) return;
    // `selectionchange` is delivered after the event that caused it, so the last change of
    // the drag may not have arrived yet: read the selection once more here.
    const lines = currentDragLines(textDrag.lines);
    textDrag = null;
    if (state.composer) return;
    state.selection = lines ? { anchor: lines.start, head: lines.end } : null;
    redecorateRows();
  });

  // The lines the drag covers now: those of the selection when it is in the code (none for a
  // collapsed one, a plain click), else the last ones seen (the rows it was in were rebuilt).
  function currentDragLines(last) {
    const selection = window.getSelection();
    const inCode = selection.rangeCount > 0 && rowsEl.contains(selection.anchorNode);
    return inCode ? textSelectionLines() : last;
  }

  // The lines the browser's text selection covers in the code, or null when nothing is
  // selected there. As in VS Code, the line a selection starts on counts even when it starts
  // at that line's end, and the line it ends on does not count when it ends at its start.
  function textSelectionLines() {
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || selection.rangeCount === 0) return null;
    const range = selection.getRangeAt(0);
    const lineOf = (node) => {
      const element = node.nodeType === Node.TEXT_NODE ? node.parentElement : node;
      const row = element?.closest(".ln");
      return row && rowsEl.contains(row) ? row : null;
    };
    const first = lineOf(range.startContainer);
    const last = lineOf(range.endContainer);
    if (!first || !last) return null;

    const start = Number(first.dataset.line);
    let end = Number(last.dataset.line);
    // A selection that runs to the very start of a line (a triple click, or a drag down to
    // the left edge) takes none of that line's text, so the line is not part of it.
    if (end > start) {
      const before = document.createRange();
      before.setStart(last.querySelector(".c") ?? last, 0);
      before.setEnd(range.endContainer, range.endOffset);
      if (before.toString() === "") end -= 1;
    }
    return { start, end };
  }

  function clearSelection() {
    state.selection = null;
    redecorateRows();
  }

  // ---- Copy -------------------------------------------------------------------------------

  // The text of lines `start` to `end` (0-based, inclusive) as the page shows them.
  function linesText({ start, end }) {
    const lines = state.file.lines.slice(start, end + 1);
    return lines.map((tokens) => tokens.map(([, text]) => text).join("")).join("\n");
  }

  // The code the text selection covers, without the line numbers, map chips and note remarks
  // that sit in the same rows: WebKit's own selection text includes them even though they cannot be
  // selected (seen 2026-11-22: a two-line selection copied the gutter's "3"). Null when the
  // selection is not in the code, or covers other lines than the selected ones (a leftover
  // from before the gutter or a right-click chose new lines).
  function selectedCodeText() {
    const lines = textSelectionLines();
    const chosen = selectedRange();
    if (!lines || !chosen || lines.start !== chosen.start || lines.end !== chosen.end) {
      return null;
    }
    const range = window.getSelection().getRangeAt(0);
    const code = (line) => rowsEl.querySelector(`.ln[data-line="${line}"] .c`);
    // How many characters of `cell` come before (container, offset), or null when that
    // position is not inside the cell (it is in the gutter, or on another line).
    const offsetIn = (cell, container, offset) => {
      if (!cell?.contains(container)) return null;
      const before = document.createRange();
      before.setStart(cell, 0);
      before.setEnd(container, offset);
      return before.toString().length;
    };

    const text = linesText(lines);
    const from = offsetIn(code(lines.start), range.startContainer, range.startOffset) ?? 0;
    const lastLine = linesText({ start: lines.end, end: lines.end });
    const to = offsetIn(code(lines.end), range.endContainer, range.endOffset) ?? lastLine.length;
    return text.slice(from, text.length - (lastLine.length - to));
  }

  rowsEl.addEventListener("copy", (event) => {
    const text = selectedCodeText();
    if (text === null) return;
    event.preventDefault();
    event.clipboardData.setData("text/plain", text);
  });

  // Cmd-C with lines selected. With text selected, the browser copies and the `copy` handler
  // fixes the text: returns false. Lines chosen in the gutter have no text selection, so the
  // whole lines are copied here: returns true.
  function copyLines() {
    if (selectedCodeText() !== null) return false;
    copyText(linesText(selectedRange()));
    return true;
  }

  // The Clipboard API needs a secure context, which a custom URL scheme may not count as; the
  // older copy command works from any page with a selection, so it is the fallback.
  async function copyText(text) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // Fall through to the copy command below.
    }
    const area = el("textarea");
    area.value = text;
    area.className = "copy-buffer";
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
    codeEl.focus();
  }

  // ---- Context menu -----------------------------------------------------------------------
  //
  // Right-click on the code opens actions for the selected lines. A click outside the
  // selection first selects the clicked line, as editors move the cursor there.

  rowsEl.addEventListener("contextmenu", (event) => {
    // The composer's textarea keeps the system menu: spelling, paste, and the like.
    if (event.target.closest(".composer")) return;
    const line = lineOfEvent(event);
    if (line === null || !state.file) return;
    event.preventDefault();

    const range = selectedRange();
    if (!range || line < range.start || line > range.end) {
      window.getSelection().removeAllRanges();
      state.selection = { anchor: line, head: line };
      redecorateRows();
    }
    const name = state.file.definitions ? nameAtPoint(event.clientX, event.clientY) : null;
    menu.open(event.clientX, event.clientY, codeActions(line, name));
  });

  // The actions for the selected lines; `line` is the one right-clicked, for its note, and
  // `name` the name right-clicked, `{ name, line }`, or null when the click was on none.
  function codeActions(line, name) {
    const range = selectedRange();
    const first = range.start + 1;
    const last = range.end + 1;
    const reference = `${state.file.path}:${first === last ? first : `${first}-${last}`}`;
    // The text selection when there is one inside the code, else the whole selected lines.
    const text = selectedCodeText() ?? linesText(range);
    // `matches` is the action's own key, which works while the menu is open, as it is shown.
    const actions = [
      {
        label: "Add Note",
        key: "C",
        matches: (event) => event.key === "c" && !event.metaKey,
        run: () => openComposer(range),
        disabled: Boolean(state.composer),
      },
      {
        label: "Ask",
        key: "Q",
        matches: (event) => event.key === "q" && !event.metaKey,
        run: () => openComposer(range, { intent: "ask" }),
        disabled: Boolean(state.composer),
      },
      {
        label: "Copy",
        key: "⌘C",
        matches: (event) => event.key === "c" && event.metaKey,
        run: () => copyText(text),
      },
      { label: "Copy Reference", detail: reference, run: () => copyText(reference) },
      // From the file, not its lines: a flow starts at a node or a path
      // (src/analysis/prompt.rs, `subject`).
      {
        label: "Trace from Here",
        detail: state.file.path,
        run: () => traceFromHere(state.file.path),
      },
    ];
    if (name) {
      actions.push({
        label: "Go to Definition",
        detail: name.name,
        run: () => goToDefinition(name.name, name.line),
      });
    }
    const note = state.noteByLine.get(line);
    if (note) {
      // A separator sets the one destructive item apart, as macOS menus do.
      actions.push({
        label: `Delete Note ${note.id}`,
        separated: true,
        run: () => deleteNote(note),
      });
    }
    return actions;
  }

  // ---- Jump to definition -----------------------------------------------------------------
  //
  // Cmd-click on a name, or Go to Definition in the right-click menu, asks the Rust process
  // where the name is defined (src/definitions.rs) and goes there; Cmd-[ and Cmd-] go back
  // and forward (app.js). The changes view and the send step's prompt do not jump.

  const picker = definitionsDialog($("definitions-dialog"));
  const history = jumpHistory();
  // Every ask is numbered; an answer to an older one, or to one cancelled, is dropped.
  let jumpAsked = 0;
  let jumpRetry = null;
  // The line the last jump landed on, 0-based, marked until the next jump: { path, index }.
  let jumped = null;
  // Set while Cmd-[ or Cmd-] opens a place, so a held key does not start a second step.
  let stepping = false;

  // The name under the point (`x`, `y`) in the code, with its 0-based line: `{ name, line }`,
  // or null on none, or on a string, keyword or comment.
  function nameAtPoint(x, y) {
    const caret = document.caretRangeFromPoint(x, y);
    const node = caret?.startContainer;
    if (node?.nodeType !== Node.TEXT_NODE) return null;
    const token = node.parentElement;
    const cell = token?.closest(".c");
    if (!cell || !rowsEl.contains(cell)) return null;
    if (token !== cell && NO_DEFINITION.has(token.className)) return null;
    const found = nameAt(node.data, caret.startOffset);
    if (!found) return null;
    // Past a line's end, WebKit puts the caret after its last character: only a point on
    // the name itself counts.
    const range = document.createRange();
    range.setStart(node, found.start);
    range.setEnd(node, found.end);
    const box = range.getBoundingClientRect();
    if (x < box.left || x > box.right || y < box.top || y > box.bottom) return null;
    return { name: found.name, line: Number(cell.closest(".ln").dataset.line) };
  }

  function jumpFromPoint(x, y) {
    if (!state.file) return;
    if (!state.file.definitions) {
      showToast("No definitions for this file type.");
      return;
    }
    const found = nameAtPoint(x, y);
    if (found) goToDefinition(found.name, found.line);
  }

  // Drops a jump still asking: Escape, or a newer jump.
  function cancelJump() {
    jumpAsked += 1;
    clearTimeout(jumpRetry);
  }

  // Goes to where `name`, clicked on 0-based `line` of the open file, is defined.
  async function goToDefinition(name, line) {
    cancelJump();
    await askDefinitions(jumpAsked, name, state.file.path, line);
  }

  // Jumps to definition by spoken or typed `name`, opening the picker when several match.
  async function jumpToName(name) {
    cancelJump();
    await askDefinitions(jumpAsked, name, state.file?.path ?? "", -1);
  }

  async function askDefinitions(asked, name, from, line) {
    const query = `name=${encodeURIComponent(name)}&from=${encodeURIComponent(from)}`;
    const response = await fetch(`api/definitions?${query}`);
    // A newer jump, Escape, or another file opened meanwhile: the answer is not wanted.
    if (asked !== jumpAsked || (state.file?.path ?? "") !== from) return;
    if (!response.ok) {
      showToast(await response.text());
      return;
    }
    const reply = await response.json();
    // An Escape during the parse: no further ask.
    if (asked !== jumpAsked) return;
    if (reply.state === "building") {
      jumpRetry = setTimeout(() => askDefinitions(asked, name, from, line), DEFINITIONS_POLL_MS);
      return;
    }
    const { definitions, total } = reply;
    const here = (definition) =>
      line >= 0 && definition.path === from && definition.line === line + 1;
    if (definitions.length === 0) {
      showToast(`No definition found for ${name}.`);
      return;
    }
    // Taken before the list opens: a refresh while it is open may change the open file.
    const left = state.file ? currentPlace() : null;
    const target =
      definitions.length === 1 ? definitions[0] : await picker.open(name, definitions, total);
    if (!target || asked !== jumpAsked) return;
    if (here(target)) {
      showToast("This is the definition.");
      return;
    }
    const landed = { path: target.path, line: target.line, scrollTop: null, selection: null };
    const shown = await showPlace(landed);
    if (shown === "shown" && left) history.jump(left, landed);
    // The index has not seen the file go yet: it looks again on the next focus.
    else if (shown === "missing") showToast(`${target.path} is no longer there.`);
  }

  function currentPlace() {
    return {
      path: state.file.path,
      line: lineAt(codeEl.scrollTop) + 1,
      scrollTop: codeEl.scrollTop,
      selection: state.selection && { ...state.selection },
    };
  }

  // Shows `place`, opening its file when another one is open. A place left before comes back
  // with its scroll offset and selection; a new one has its line a third of the way down, and
  // the selection of the file it was jumped from when that is the same file. Resolves to
  // "shown", "missing" (its file is gone), or "superseded" (a draft note holds the pane, or
  // another open overtook this one), as `jumpHistory().step` takes.
  async function showPlace(place) {
    if (state.file?.path !== place.path) {
      const opened = await openFileFor(place.path, { onlyFound: true });
      if (opened === "held") return "superseded";
      if (opened !== "shown") return opened;
    }
    const returning = place.scrollTop !== null;
    if (returning) {
      const fits = place.selection && Math.max(place.selection.anchor, place.selection.head);
      state.selection = fits !== null && fits < lineCount() ? { ...place.selection } : null;
    }
    // A place left before is wherever the pane was scrolled to: no line of it is marked.
    jumped = returning ? null : { path: place.path, index: place.line - 1 };
    held = null;
    redecorateRows();
    const top = returning ? place.scrollTop : lineTop(place.line - 1) - codeEl.clientHeight / 3;
    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
    codeEl.scrollTo({ top, behavior: returning || reduce ? "auto" : "smooth" });
    return "shown";
  }

  async function stepHistory(direction) {
    if (stepping || !state.file) return;
    // A draft note keeps the pane, and its lines on screen: no step, not even in the file.
    if (state.composer?.text.trim()) {
      state.composer.node.querySelector("textarea").focus();
      return;
    }
    cancelJump();
    stepping = true;
    try {
      await history.step(direction, currentPlace(), showPlace);
    } finally {
      stepping = false;
    }
  }

  // ---- Composer ---------------------------------------------------------------------------

  // Opens the composer on `range` (0-based lines), by default the selection. The context
  // menu passes the lines it was opened on, so its Add note acts on them whatever happened
  // to the selection between the right-click and the click. With `intent` "ask" it asks a
  // question about the lines instead (docs/review-assist.md, "Ask without changing"): saving
  // sends it at once to the default agent, which only reads.
  function openComposer(range = selectedRange(), { intent = "change", text = "" } = {}) {
    if (!range || state.composer) return;
    state.selection = { anchor: range.start, head: range.end };

    // The composer opens under the selection, so the selection must be on screen to see it.
    const top = lineTop(range.start);
    const bottom = lineTop(range.end) + lineHeight;
    if (top < codeEl.scrollTop || bottom > codeEl.scrollTop + codeEl.clientHeight) {
      codeEl.scrollTop = top - codeEl.clientHeight / 3;
    }

    const composer = { ...range, intent, text, saving: false, node: null };
    composer.node = buildComposer(composer);
    state.composer = composer;
    renderCode({ force: true });
    onActiveLine();
    const textarea = composer.node.querySelector("textarea");
    if (text) {
      textarea.value = text;
      textarea.style.height = "auto";
      textarea.style.height = `${textarea.scrollHeight}px`;
      requestAnimationFrame(() => measureComposer(composer.node));
    }
    textarea.focus();
    if (intent === "ask") showAskWarning(composer);
  }

  // Asks a question about the selected lines, as `q` does.
  function askOnSelection() {
    openComposer(selectedRange(), { intent: "ask" });
  }

  // The working tree's changes since the last commit, above the question once read: the agent
  // reads that commit, and the asked file's lines may differ there. Nothing when they cannot be
  // read: the question can go all the same.
  async function showAskWarning(composer) {
    const path = state.file.path;
    let reply = null;
    try {
      const response = await fetch(`api/run/uncommitted?file=${encodeURIComponent(path)}`);
      if (response.ok) reply = await response.json();
    } catch {
      // A request that never got an answer costs the warning only.
    }
    if (!reply || state.composer !== composer) return;
    const warning = askWarning(reply);
    if (!warning || state.composer !== composer) return;
    composer.node.querySelector("textarea").before(warning);
    measureComposer(composer.node);
  }

  // Closes the composer, saving its text as a note first when `save` is set. When saving
  // fails, the composer stays open with the reason in its label, so the text is not lost.
  async function closeComposer({ save }) {
    const composer = state.composer;
    if (!composer || composer.saving) return;
    const saves = save && composer.text.trim() !== "";
    if (saves) {
      composer.saving = true;
      let failure;
      try {
        failure = await saveNote(composer);
      } catch (error) {
        failure = `cannot ${composer.intent === "ask" ? "ask" : "save the note"}: ${error}`;
      } finally {
        composer.saving = false;
      }
      if (failure) {
        const label = composer.node.querySelector(".composer-label");
        label.textContent = failure;
        label.classList.add("failed");
        return;
      }
    }
    state.composer = null;
    state.selection = null;
    renderCode({ force: true });
    onActiveLine();
    onComposerClosed();
    if (saves && composer.intent === "ask") onAsked();
    codeEl.focus();
  }

  // Sends the composer's note to the Rust process with the text the page showed on its lines,
  // so a file changed on disk meanwhile cannot attach the note to other text; a question goes
  // to `api/run/ask`, which saves it only once its run starts (src/run.rs, `Runner::ask`).
  // Returns the reason on failure, or null.
  async function saveNote(composer) {
    const note = {
      path: state.file.path,
      lines: { start: composer.start + 1, end: composer.end + 1 },
      snippet: linesText(composer),
      body: composer.text,
    };
    const response =
      composer.intent === "ask"
        ? await fetch("api/run/ask", { method: "POST", body: JSON.stringify({ note }) })
        : await fetch("api/notes", { method: "POST", body: JSON.stringify(note) });
    if (!response.ok) return await response.text();
    state.notes = (await response.json()).notes;
    indexNotes();
    return null;
  }

  // Builds the composer's element once, when it opens. `renderCode` moves this same node.
  function buildComposer(composer) {
    const node = $("composer-template").content.firstElementChild.cloneNode(true);

    const kind = composer.intent === "ask" ? "Ask" : "Note";
    const label =
      composer.start === composer.end
        ? `${kind} on line ${composer.start + 1}`
        : `${kind} on lines ${composer.start + 1} to ${composer.end + 1}`;
    node.querySelector("label").textContent = label;
    if (composer.intent === "ask") {
      node.querySelector("[data-action=save]").textContent = "Ask";
      node.querySelector(".hint").textContent = "⌘↵ ask · esc discard";
    }

    const textarea = node.querySelector("textarea");
    const dictation = attachDictation(textarea);
    node.querySelector(".buttons").prepend(dictation.button);
    textarea.addEventListener("input", () => {
      composer.text = textarea.value;
      // Grow with the text, then tell the virtual list the new height.
      textarea.style.height = "auto";
      textarea.style.height = `${textarea.scrollHeight}px`;
      measureComposer(node);
    });
    textarea.addEventListener("keydown", (event) => {
      // Cmd-Shift-Enter is Send, which the window's handler answers.
      if (event.key === "Enter" && event.metaKey && !event.shiftKey) {
        closeComposer({ save: true });
      }
    });

    node.querySelector("[data-action=save]").addEventListener("click", () => {
      closeComposer({ save: true });
    });
    node.querySelector("[data-action=discard]").addEventListener("click", () => {
      closeComposer({ save: false });
    });

    // Measured after it is in the document; until then the guess stands in.
    requestAnimationFrame(() => measureComposer(node));
    return node;
  }

  function measureComposer(node) {
    if (!node.isConnected) return;
    const style = getComputedStyle(node);
    const height = node.offsetHeight + parseFloat(style.marginTop) + parseFloat(style.marginBottom);
    if (height !== composerHeight) {
      composerHeight = height;
      setRowsPadding();
    }
  }

  // ---- Scrolling --------------------------------------------------------------------------

  let renderQueued = false;
  codeEl.addEventListener(
    "scroll",
    () => {
      menu.close();
      if (renderQueued) return;
      renderQueued = true;
      requestAnimationFrame(() => {
        renderQueued = false;
        renderCode();
        // Inside the rendered window `renderCode` draws nothing, but the first visible line
        // moved all the same.
        onActiveLine();
      });
    },
    { passive: true },
  );

  new ResizeObserver(() => renderCode()).observe(codeEl);

  return {
    fetchFile,
    openFile,
    showFile,
    restoreHeader,
    showNote,
    showAnchor,
    lineCount,
    activeLine,
    render: renderCode,
    redecorate: redecorateRows,
    setLineHeight,
    clearSelection,
    copyLines,
    openComposer,
    askOnSelection,
    closeComposer,
    cancelJump,
    jumpToName,
    stepHistory,
  };
}
