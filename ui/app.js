// The page of the Postil app. It draws what the Rust process serves under `api/` and has no
// build step and no dependencies: this file is compiled into the binary as it is.
//
// The Rust process owns the notes (`api/notes`, stored in the git directory). The page keeps
// a copy only to draw it, and fetches it again whenever it may have changed: at start, after
// saving, and when the window regains focus.

import { rank } from "./fuzzy.js";
import { fileIcon, folderIcon } from "./icons.js";
import { changesView } from "./changes-view.js";
import { BIG_STEP, STEP, dragFiles, dragTray, fit, reset, step } from "./panes.js";
import { noteDialog, statusIcon } from "./note-dialog.js";
import { runPanel } from "./run-panel.js";
import { foldRun, isLive, isOpen, isResolved, promptLines, sendable, sentIn } from "./run.js";
import { settingsDialog } from "./settings-dialog.js";
import { codeMetrics } from "./settings.js";
import { groupNotes, noteStatus, planProgress, reviewOf, runOf } from "./status.js";
import { ancestors, buildTree, folderCounts, visibleRows } from "./tree.js";

const $ = (id) => document.getElementById(id);

const filesEl = $("files");
const filterEl = $("filter");
const codeEl = $("code");
const rowsEl = $("code-rows");
const codeEmptyEl = $("code-empty");
const notesEl = $("notes");

// One code line's height in px: `--line-height` in app.css until the settings arrive, then
// what they give (`applySettings`), which sets the CSS variable too.
let lineHeight = parseFloat(
  getComputedStyle(document.documentElement).getPropertyValue("--line-height"),
);
// `.code-rows` has this much padding above the first and below the last line (app.css).
const ROWS_PADDING = 12;
// Lines rendered beyond each edge of the viewport. Arbitrary: enough that a fast flick
// does not show blank space before the next render, few enough that a render stays cheap.
const OVERSCAN = 40;
// The most fuzzy matches the file filter lists. Arbitrary: more than fit on a screen, few
// enough that every keystroke rebuilds the list without a visible delay.
const MAX_MATCHES = 100;
// The composer's height before it has been measured once. A guess: label, two text rows,
// actions, and margins.
const COMPOSER_HEIGHT_GUESS = 138;

const state = {
  repo: { name: "", branch: "", files: [] },
  // The file shown in the code pane: { path, lines }, where a line is a list of
  // [cssClass, text] tokens.
  file: null,
  // The path most recently asked for, to drop replies that arrive out of order.
  wanted: null,
  // Selected lines as 0-based indexes, both inclusive: { anchor, head }.
  selection: null,
  // The open note composer: { start, end, text, saving, node }, lines 0-based and inclusive.
  // `node` is built once and moved, never rebuilt, when the rows re-render: a rebuilt
  // textarea would lose its focus, caret, and grown height while the engineer types.
  composer: null,
  composerHeight: COMPOSER_HEIGHT_GUESS,
  // As `api/notes` returns them: { id, path, lines: { start, end }, snippet, body, anchor },
  // lines 1-based and inclusive, anchor "found" or "lost". A lost note is shown as outdated.
  notes: [],
  // Derived from `notes` by `indexNotes`: the open note on each 0-based line of the open
  // file, and the number of notes in each file.
  noteByLine: new Map(),
  noteCountByPath: new Map(),
  // The repository's files as a tree (tree.js), rebuilt when the file list changes, and the
  // paths of the folders open in it.
  tree: buildTree([]),
  expanded: new Set(),
  // The index of the highlighted row while the file filter has text.
  activeMatch: 0,
  // Whether the Rust process has a project open; without one, the welcome screen shows.
  projectOpen: false,
  // Set while a project is being opened, until the page reloads into it.
  switching: false,
  // The rendered window of lines: [first, last).
  rendered: { first: 0, last: 0 },
  // The settings in effect (`applySettings`), for the agent notes are sent to.
  settings: null,
  // The send step while it is open: the ids of the notes ticked, and the agent chosen for
  // this send. The code pane then shows the prompt instead of the file.
  send: null,
  // Which of the tray's tabs shows: "notes" or "run".
  tab: "notes",
};

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

const SVG = "http://www.w3.org/2000/svg";

// An icon from the sprite that `loadIcons` put in the page, by symbol id.
function icon(id) {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS(SVG, "use");
  use.setAttribute("href", `#${id}`);
  svg.append(use);
  return svg;
}

// Puts icons.svg into the page once, so every icon refers to a symbol in this document.
// Until it arrives, or if it fails, the icons are empty boxes and nothing else changes.
async function loadIcons() {
  const response = await fetch("icons.svg");
  if (!response.ok) return;
  const sprite = new DOMParser().parseFromString(await response.text(), "image/svg+xml");
  const root = document.importNode(sprite.documentElement, true);
  root.setAttribute("class", "icon-sprite");
  root.setAttribute("aria-hidden", "true");
  document.body.prepend(root);
}

// ---- Files ----------------------------------------------------------------------------

// Pixels of indent per level of the tree, and before the first level.
const INDENT = 12;
const INDENT_BASE = 4;

const rowSelector = (path) => `[data-path="${CSS.escape(path)}"]`;

function renderFiles() {
  const needle = filterEl.value.trim().toLowerCase();
  // A row with keyboard focus is replaced; focus moves to the new row for the same path.
  const focused = filesEl.contains(document.activeElement)
    ? document.activeElement.dataset.path
    : null;

  if (needle) {
    filesEl.removeAttribute("role");
    replaceRows(filesEl, filteredFileRows(needle));
  } else {
    filesEl.setAttribute("role", "tree");
    replaceRows(filesEl, treeRows());
    setTabStop(filesEl.querySelector(".file.open") ?? filesEl.querySelector(".file, .dir"));
  }
  if (focused) filesEl.querySelector(rowSelector(focused))?.focus();
}

// Replaces the children one by one instead of spreading them into one call: a folder can
// hold more children than an engine accepts as arguments (about 65,000 in JavaScriptCore).
function replaceRows(container, rows) {
  const fragment = document.createDocumentFragment();
  for (const row of rows) fragment.append(row);
  container.replaceChildren(fragment);
}

// With a filter: a flat list of the best fuzzy matches, with the matched characters marked.
// The row at `state.activeMatch` is the one Enter opens.
function filteredFileRows(needle) {
  const matches = rank(needle, state.repo.files, MAX_MATCHES);
  state.activeMatch = Math.min(state.activeMatch, Math.max(0, matches.length - 1));
  return matches.map(({ path, positions }, index) => {
    const row = fileRow(path, markedPath(path, positions));
    row.prepend(icon(fileIcon(path.split("/").at(-1))));
    row.classList.toggle("active", index === state.activeMatch);
    return row;
  });
}

// `path` as a label with the characters at `positions` wrapped in <mark>.
function markedPath(path, positions) {
  const label = el("span", "name");
  const marked = new Set(positions);
  for (let index = 0; index < path.length; index += 1) {
    label.append(marked.has(index) ? el("mark", "", path[index]) : path[index]);
  }
  return label;
}

// Without a filter: the tree, showing the contents of open folders only. A closed folder
// carries the count of notes inside it, so collapsing it does not hide that they exist.
function treeRows() {
  const totals = folderCounts(state.noteCountByPath);
  return visibleRows(state.tree, state.expanded).map((row) => {
    const button = row.kind === "folder" ? folderRow(row, totals) : fileRow(row.path, row.name);
    // Files get an empty chevron slot, so names line up with their folder's siblings.
    if (row.kind === "file") button.prepend(el("span", "chevron"), icon(fileIcon(row.name)));
    button.style.paddingLeft = `${INDENT_BASE + INDENT * row.depth}px`;
    button.setAttribute("role", "treeitem");
    button.setAttribute("aria-level", String(row.depth + 1));
    return button;
  });
}

function folderRow(row, totals) {
  const button = el("button", "dir");
  button.type = "button";
  button.dataset.path = row.path;
  button.setAttribute("aria-expanded", String(row.open));
  button.append(
    el("span", "chevron"),
    icon(folderIcon(row.name, row.open)),
    el("span", "name", row.name),
  );
  const count = row.open ? 0 : totals.get(row.path);
  if (count) button.append(el("span", "count", String(count)));
  return button;
}

// `label` is the text to show, or an element (the filter marks matched characters).
function fileRow(path, label) {
  const row = el("button", "file");
  row.type = "button";
  row.dataset.path = path;
  row.append(typeof label === "string" ? el("span", "name", label) : label);
  if (state.file?.path === path) row.classList.add("open");

  const count = state.noteCountByPath.get(path);
  if (count) row.append(el("span", "count", String(count)));
  return row;
}

// One row of the tree is in the Tab order; the arrow keys move between rows (the ARIA tree
// pattern). Without this, Tab would stop on every visible file.
function setTabStop(row) {
  for (const other of filesEl.querySelectorAll(".file, .dir")) {
    if (other !== row) other.tabIndex = -1;
  }
  if (row) row.tabIndex = 0;
}

function setFolderOpen(path, open) {
  if (open) state.expanded.add(path);
  else state.expanded.delete(path);
  renderFiles();
  saveFolders();
}

// The folders open when the project was last shown (src/folders.rs), read once as it opens.
// One that cannot be read leaves the tree closed, as before folders were kept.
async function loadFolders() {
  const response = await fetch("api/folders");
  if (response.ok) state.expanded = new Set((await response.json()).open);
}

// Saves the open folders a moment after the last change: a key held on a folder, or opening
// a file deep in the tree, changes several at once.
let foldersTimer = null;
function saveFolders() {
  clearTimeout(foldersTimer);
  foldersTimer = setTimeout(() => {
    const body = JSON.stringify({ open: [...state.expanded].sort() });
    fetch("api/folders", { method: "PUT", body })
      .then(async (response) => {
        if (!response.ok) showToast(await response.text());
      })
      .catch((error) => showToast(String(error)));
  }, 300);
}

// Opens the folders around `path` so its row exists, marks it as the open file, and scrolls
// it into view. Moves the mark without rebuilding the list when no folder had to open.
function revealFile(previous, path) {
  const closed = ancestors(path).filter((folder) => !state.expanded.has(folder));
  for (const folder of closed) state.expanded.add(folder);

  if (closed.length > 0) {
    renderFiles();
    saveFolders();
  } else {
    if (previous) filesEl.querySelector(`.file${rowSelector(previous)}`)?.classList.remove("open");
    const row = filesEl.querySelector(`.file${rowSelector(path)}`);
    row?.classList.add("open");
    // Tab into the tree lands on the open file, as it does after a rebuild.
    if (row && filesEl.getAttribute("role") === "tree") setTabStop(row);
  }
  filesEl.querySelector(`.file${rowSelector(path)}`)?.scrollIntoView({ block: "nearest" });
}

// The reply to `api/file`: { ok, body }, the body a file view or the reason it failed.
async function fetchFile(path) {
  const response = await fetch(`api/file?path=${encodeURIComponent(path)}`);
  return { ok: response.ok, body: response.ok ? await response.json() : await response.text() };
}

// Opens `path` in the code pane. Returns whether it was shown: not when a draft note would
// be lost (the composer is focused instead), and not when a newer open overtook this one.
async function openFile(path) {
  if (state.composer?.text.trim()) {
    state.composer.node.querySelector("textarea").focus();
    return false;
  }
  changes.close();
  state.composer = null;
  state.wanted = path;
  const reply = await fetchFile(path);
  if (state.wanted !== path) return false;
  showFile(path, reply, { keepView: false });
  return true;
}

// Draws a reply of `fetchFile`. `keepView` is for the same file read again after it may have
// changed on disk: the scroll position stays, and so does the selection while it still fits
// in the file.
function showFile(path, reply, { keepView }) {
  const previous = state.file?.path;
  const lines = reply.ok ? reply.body.lines.length : 0;
  if (!keepView || Math.max(state.selection?.anchor ?? 0, state.selection?.head ?? 0) >= lines) {
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
  // The changes view holds the header while it is open over the file; a refresh of the file
  // under it (on focus, or as a run moves on) leaves the header alone.
  if (!changes.isOpen()) renderHeader(path);
  revealFile(previous, path);
  if (!keepView) codeEl.scrollTop = 0;
  renderCode({ force: true });
}

function renderHeader(path) {
  const parts = path.split("/");
  const crumbs = $("crumbs");
  crumbs.replaceChildren(parts.slice(0, -1).map((part) => `${part} / `).join(""));
  crumbs.append(el("b", "", parts.at(-1)));
  $("file-meta").textContent = state.file ? `${state.file.lines.length} lines` : "";
}

// ---- Code: a virtual list -------------------------------------------------------------
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
  const pushed = state.composer && index > state.composer.end ? state.composerHeight : 0;
  return ROWS_PADDING + index * lineHeight + pushed;
}

// The line at scroll offset `y`: the inverse of `lineTop`.
function lineAt(y) {
  const composer = state.composer;
  if (composer) {
    const composerTop = lineTop(composer.end) + lineHeight;
    if (y >= composerTop + state.composerHeight) y -= state.composerHeight;
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
  const { first, last } = state.rendered;
  const covered = visibleFirst >= first && visibleLast <= last;
  const nearEdge =
    (visibleFirst - first < OVERSCAN / 2 && first > 0) ||
    (last - visibleLast < OVERSCAN / 2 && last < count);
  if (!force && covered && !nearEdge) return;

  const newFirst = Math.max(0, visibleFirst - OVERSCAN);
  const newLast = Math.min(count, visibleLast + OVERSCAN);
  state.rendered = { first: newFirst, last: newLast };

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
  const { first, last } = state.rendered;
  const composer = state.composer;
  const above = composer && composer.end < first ? state.composerHeight : 0;
  const below = composer && composer.end >= last ? state.composerHeight : 0;
  rowsEl.style.paddingTop = `${ROWS_PADDING + first * lineHeight + above}px`;
  rowsEl.style.paddingBottom = `${ROWS_PADDING + (lineCount() - last) * lineHeight + below}px`;
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

// Applies selection and note styling to a row. Called for new rows and again when the
// selection or the notes change, so those changes do not rebuild the rows.
function decorateRow(row, index) {
  row.classList.toggle("selected", isSelected(index));

  const note = state.noteByLine.get(index);
  row.classList.toggle("noted", Boolean(note));
  row.querySelector(".remark")?.remove();
  if (note && note.lines.end - 1 === index) {
    row.append(el("span", "remark", `${note.id} · ${note.body}`));
  }
}

function redecorateRows() {
  for (const row of rowsEl.querySelectorAll(".ln")) {
    decorateRow(row, Number(row.dataset.line));
  }
}

// ---- Selection ------------------------------------------------------------------------

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
  // A press on the + starts neither a text selection nor a drag: its click adds the note.
  if (event.target.closest(".add-note")) {
    event.preventDefault();
    return;
  }
  if (event.button !== 0 || event.target.classList.contains("n") || state.composer) return;
  textDrag = { lines: null };
});

// The + on a line: a note on that line, or on the whole selection when the line is in it.
rowsEl.addEventListener("click", (event) => {
  if (!event.target.closest(".add-note")) return;
  const line = lineOfEvent(event);
  const range = selectedRange();
  const inSelection = range && range.start <= line && line <= range.end;
  openComposer(inSelection ? range : { start: line, end: line });
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

// The text of lines `start` to `end` (0-based, inclusive) as the page shows them.
function linesText({ start, end }) {
  const lines = state.file.lines.slice(start, end + 1);
  return lines.map((tokens) => tokens.map(([, text]) => text).join("")).join("\n");
}

// The code the text selection covers, without the line numbers and note remarks that sit
// in the same rows: WebKit's own selection text includes them even though they cannot be
// selected (seen 2026-11-22: a two-line selection copied the gutter's "3"). Null when the
// selection is not in the code, or covers other lines than the selected ones (a leftover
// from before the gutter or a right-click chose new lines).
function selectedCodeText() {
  const lines = textSelectionLines();
  const chosen = selectedRange();
  if (!lines || !chosen || lines.start !== chosen.start || lines.end !== chosen.end) return null;
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

// ---- Context menu ---------------------------------------------------------------------
//
// Right-click on the code opens actions for the selected lines. A click outside the
// selection first selects the clicked line, as editors move the cursor there.

const menuEl = $("context-menu");

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
  openMenu(event.clientX, event.clientY, codeActions(line));
});

// The actions for the selected lines; `line` is the one right-clicked, for its note.
function codeActions(line) {
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
      label: "Copy",
      key: "⌘C",
      matches: (event) => event.key === "c" && event.metaKey,
      run: () => copyText(text),
    },
    { label: "Copy Reference", detail: reference, run: () => copyText(reference) },
  ];
  const note = state.noteByLine.get(line);
  if (note) {
    // A separator sets the one destructive item apart, as macOS menus do.
    actions.push({ label: `Delete Note ${note.id}`, separated: true, run: () => deleteNote(note) });
  }
  return actions;
}

// The actions of the open menu, for its keyboard shortcuts.
let menuActions = [];

// Closes the menu and runs `action`. A failure is shown, never swallowed: an action that
// silently did nothing is what made notes look broken once.
async function runAction(action) {
  closeMenu();
  try {
    await action.run();
  } catch (error) {
    showToast(`${action.label} failed: ${error}`);
  }
}

// A short message at the bottom of the window, for failures with no better place to show.
function showToast(text) {
  const toast = $("toast");
  toast.textContent = text;
  toast.hidden = false;
  clearTimeout(showToast.timer);
  showToast.timer = setTimeout(() => {
    toast.hidden = true;
  }, 5000);
}

function openMenu(x, y, actions) {
  menuActions = actions;
  const items = actions.flatMap((action) => {
    const item = el("button", "menu-item");
    item.type = "button";
    item.setAttribute("role", "menuitem");
    item.disabled = Boolean(action.disabled);
    item.append(el("span", "menu-label", action.label));
    if (action.detail) item.append(el("span", "menu-detail", action.detail));
    if (action.key) item.append(el("kbd", "", action.key));
    item.addEventListener("click", () => runAction(action));
    // The highlight is the focus, and the pointer moves it, so the pointer and the arrow keys
    // never highlight two items at once.
    // A disabled item cannot take the focus, so hovering it clears the highlight instead of
    // leaving it on the item before, which Enter would then run.
    item.addEventListener("mouseenter", () => (item.disabled ? menuEl : item).focus());
    if (!action.separated) return [item];
    const separator = el("div", "menu-separator");
    separator.setAttribute("role", "separator");
    return [separator, item];
  });
  menuEl.replaceChildren(...items);
  menuEl.hidden = false;

  // Opens at the pointer, moved back inside the window when it would cross an edge.
  const { width, height } = menuEl.getBoundingClientRect();
  menuEl.style.left = `${Math.max(4, Math.min(x, innerWidth - width - 4))}px`;
  menuEl.style.top = `${Math.max(4, Math.min(y, innerHeight - height - 4))}px`;
  // Opened by the pointer, so no item is highlighted yet, as in a macOS menu. The menu itself
  // takes the focus so the arrow keys and Escape still reach it.
  menuEl.focus();
}

function closeMenu() {
  if (menuEl.hidden) return;
  menuEl.hidden = true;
  if (menuEl.contains(document.activeElement) || document.activeElement === document.body) {
    codeEl.focus();
  }
}

// While open, the menu owns the keyboard, as system menus do: no key reaches the window's
// handlers, so `c` cannot open a composer under it and Escape cannot discard one.
menuEl.addEventListener("keydown", (event) => {
  // A Command shortcut the menu has no item for (Cmd-Q, Cmd-H, Cmd-Shift-O) belongs to the
  // menu bar: close this menu and let the key through, so WebKit passes it on.
  const own = menuActions.some((each) => !each.disabled && each.matches?.(event));
  if (event.metaKey && !own) {
    closeMenu();
    return;
  }
  event.stopPropagation();
  // Enter and Space are the focused item's own: they click it.
  if (event.key === "Enter" || event.key === " ") return;
  event.preventDefault();

  const items = [...menuEl.querySelectorAll("button:not(:disabled)")];
  // -1 while no item is highlighted: Down then takes the first item and Up the last.
  const index = items.indexOf(document.activeElement);
  const up = index === -1 ? items.length - 1 : (index - 1 + items.length) % items.length;
  if (event.key === "ArrowDown") items[(index + 1) % items.length]?.focus();
  else if (event.key === "ArrowUp") items[up]?.focus();
  else if (event.key === "Home") items[0]?.focus();
  else if (event.key === "End") items.at(-1)?.focus();
  else if (event.key === "Escape" || event.key === "Tab") closeMenu();
  else {
    const action = menuActions.find((each) => !each.disabled && each.matches?.(event));
    if (action) runAction(action);
  }
});

// WebKit does not focus a button that is clicked: the press would move focus out of the menu
// first. Keeping the focus where it is lets the click reach the item. (A `focusout` handler
// that closed the menu here once made every item do nothing.)
menuEl.addEventListener("mousedown", (event) => event.preventDefault());
// Leaving the menu clears the highlight, as in a macOS menu.
menuEl.addEventListener("mouseleave", () => menuEl.focus());

// Any press outside the menu, a scroll of the code, or leaving the window closes it.
window.addEventListener(
  "mousedown",
  (event) => {
    if (!menuEl.contains(event.target)) closeMenu();
  },
  true,
);
window.addEventListener("blur", closeMenu);

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

// ---- Composer and notes ---------------------------------------------------------------

// Opens the composer on `range` (0-based lines), by default the selection. The context
// menu passes the lines it was opened on, so its Add note acts on them whatever happened
// to the selection between the right-click and the click.
function openComposer(range = selectedRange()) {
  if (!range || state.composer) return;
  state.selection = { anchor: range.start, head: range.end };

  // The composer opens under the selection, so the selection must be on screen to see it.
  const top = lineTop(range.start);
  const bottom = lineTop(range.end) + lineHeight;
  if (top < codeEl.scrollTop || bottom > codeEl.scrollTop + codeEl.clientHeight) {
    codeEl.scrollTop = top - codeEl.clientHeight / 3;
  }

  const composer = { ...range, text: "", saving: false, node: null };
  composer.node = buildComposer(composer);
  state.composer = composer;
  renderCode({ force: true });
  composer.node.querySelector("textarea").focus();
}

// Closes the composer, saving its text as a note first when `save` is set. When saving
// fails, the composer stays open with the reason in its label, so the text is not lost.
async function closeComposer({ save }) {
  const composer = state.composer;
  if (!composer || composer.saving) return;
  if (save && composer.text.trim()) {
    composer.saving = true;
    let failure;
    try {
      failure = await saveNote(composer);
    } catch (error) {
      failure = `cannot save the note: ${error}`;
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
  renderNotes();
  renderFiles();
  codeEl.focus();
}

// Sends the composer's note to the Rust process with the text the page showed on its lines,
// so a file changed on disk meanwhile cannot attach the note to other text. Returns the
// reason on failure, or null.
async function saveNote(composer) {
  const shown = state.file.lines.slice(composer.start, composer.end + 1);
  const request = {
    path: state.file.path,
    lines: { start: composer.start + 1, end: composer.end + 1 },
    snippet: shown.map((tokens) => tokens.map(([, text]) => text).join("")).join("\n"),
    body: composer.text,
  };
  const response = await fetch("api/notes", { method: "POST", body: JSON.stringify(request) });
  if (!response.ok) return await response.text();
  state.notes = (await response.json()).notes;
  indexNotes();
  return null;
}

// The reply to `api/notes`: { ok, body }, the body the notes or the reason they failed.
// Every notes reply is numbered as it is asked for, and one older than a reply already shown
// is dropped: a run's updates read the notes again and again, and an older file must not be
// painted over a newer one.
let notesAsked = 0;
let notesShown = 0;

async function fetchNotes() {
  const number = ++notesAsked;
  const response = await fetch("api/notes");
  const body = response.ok ? (await response.json()).notes : await response.text();
  return { ok: response.ok, body, number };
}

// Takes a reply of `fetchNotes`. A failure (an unreadable review file) is shown in the tray,
// and the notes drawn so far stay.
function applyNotes(reply) {
  if (reply.number < notesShown) return false;
  notesShown = reply.number;
  if (reply.ok) state.notes = reply.body;
  else notesEl.replaceChildren(el("div", "empty", reply.body));
  return reply.ok;
}

async function loadNotes() {
  if (!applyNotes(await fetchNotes())) return;
  indexNotes();
  renderNotes();
  renderFiles();
  redecorateRows();
}

// Rebuilds the two lookups derived from `state.notes`. Lost notes are counted but not
// drawn on the code: their lines no longer hold their text. When notes overlap on a line,
// the newest one is shown there.
function indexNotes() {
  state.noteByLine = new Map();
  state.noteCountByPath = new Map();
  for (const note of state.notes) {
    state.noteCountByPath.set(note.path, (state.noteCountByPath.get(note.path) ?? 0) + 1);
    if (note.path !== state.file?.path || note.anchor !== "found") continue;
    for (let line = note.lines.start - 1; line < note.lines.end; line += 1) {
      state.noteByLine.set(line, note);
    }
  }
}

// Builds the composer's element once, when it opens. `renderCode` moves this same node.
function buildComposer(composer) {
  const node = $("composer-template").content.firstElementChild.cloneNode(true);

  const label =
    composer.start === composer.end
      ? `Note on line ${composer.start + 1}`
      : `Note on lines ${composer.start + 1} to ${composer.end + 1}`;
  node.querySelector("label").textContent = label;

  const textarea = node.querySelector("textarea");
  textarea.addEventListener("input", () => {
    composer.text = textarea.value;
    // Grow with the text, then tell the virtual list the new height.
    textarea.style.height = "auto";
    textarea.style.height = `${textarea.scrollHeight}px`;
    measureComposer(node);
  });
  textarea.addEventListener("keydown", (event) => {
    // Cmd-Shift-Enter is Send, which the window's handler answers.
    if (event.key === "Enter" && event.metaKey && !event.shiftKey) closeComposer({ save: true });
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
  if (height !== state.composerHeight) {
    state.composerHeight = height;
    setRowsPadding();
  }
}

// The tray: in the send step, every note with its checkbox, in the prompt's order; otherwise
// the notes grouped by where they are in the loop (status.js), each card with its status.
// Drawn again only when what it shows changed: a run updates many times a second, and a
// redraw would drop a card's hover and a delete waiting for its second click.
let drawnNotes = null;
function renderNotes() {
  const count = state.notes.length;
  $("note-count").textContent = state.send
    ? `${state.send.ticked.size} of ${count} selected`
    : count === 1
      ? "1 note"
      : `${count} notes`;
  renderSendBar();
  noteDetail.draw();
  if (count === 0) {
    drawnNotes = null;
    const empty = el("div", "empty", "Select lines, then press ");
    empty.append(el("kbd", "", "c"), " to write a note.");
    notesEl.replaceChildren(empty);
    return;
  }
  if (state.send) {
    drawnNotes = null;
    // A resolved note is sent only once reopened (src/notes.rs, `resolve`).
    notesEl.replaceChildren(...state.notes.filter((note) => !isResolved(note)).map(sendItem));
    return;
  }

  const live = runs.run();
  const cards = new Map(state.notes.map((note) => [note.id, cardOf(note)]));
  const key = JSON.stringify([...cards.values(), live?.id, live?.status, live && planProgress(live)]);
  if (key === drawnNotes) return;
  drawnNotes = key;

  const groups = groupNotes(state.notes, (note) => cards.get(note.id).status);
  const parts = [];
  if (groups.inRun.length && isLive(live)) {
    const progress = planProgress(live);
    parts.push(
      groupHeader(
        `In ${live.id} · ${live.agent}`,
        progress ? `${progress.done} of ${progress.total} steps` : "",
      ),
      ...groups.inRun.map((note) => noteItem(note, cards.get(note.id))),
    );
  }
  if (groups.open.length) {
    parts.push(
      groupHeader(`Open · ${groups.open.length}`, ""),
      ...groups.open.map((note) => noteItem(note, cards.get(note.id))),
    );
  }
  if (groups.done.length) {
    parts.push(
      groupHeader(`Done · ${groups.done.length}`, "Newest first"),
      ...groups.done.map((note) => noteItem(note, cards.get(note.id))),
    );
  }
  if (groups.resolved.length) {
    parts.push(
      groupHeader(`Resolved · ${groups.resolved.length}`, ""),
      ...groups.resolved.map((note) => noteItem(note, cards.get(note.id))),
    );
  }
  notesEl.replaceChildren(...parts);
}

// The run a note's status comes from, or null (status.js, `runOf`).
function runFor(note) {
  return runOf(note, { get: (id) => runs.byId(id) }, runs.run());
}

// Everything a note's card shows, as plain data: two cards with equal data look the same. A card
// is small, its status and the note in two lines at most, so the tray scans fast; what the
// agent said and did is in the note's detail. The owner set this for every view on 2026-11-23:
// compact cards, full information in the dialogs.
function cardOf(note) {
  const run = runFor(note);
  const status = noteStatus(note, run);
  return {
    id: note.id,
    path: note.path,
    lines: note.lines,
    body: note.body,
    outdated: note.anchor === "lost",
    status,
    sendable: canSendAlone() && !isResolved(note),
  };
}

// Whether the arrow on a card sends its note now: no run under way. A note whose lines moved
// can be sent too: the prompt quotes the lines as they were (src/notes.rs, `prompt`), and an
// agent's edit is the usual reason they moved.
function canSendAlone() {
  return !runs.live();
}

function groupHeader(left, right) {
  const header = el("div", "note-group");
  header.append(el("span", "", left), el("span", "", right));
  return header;
}

// A note's card: its status, its place and id, and the note in two lines at most.
function noteCard(card, onClick) {
  const { start, end } = card.lines;
  const lines = start === end ? start : `${start}-${end}`;
  const button = el("button", `note ${card.status.kind}${card.outdated ? " outdated" : ""}`);
  button.type = "button";
  const head = el("div", "note-where");
  const label = el("span", `note-status ${card.status.tone}`);
  label.append(statusIcon(card.status.kind), card.outdated ? `${card.status.label} · outdated` : card.status.label);
  const place = el("span", "note-place", `${card.path.split("/").at(-1)}:${lines}`);
  place.title = card.path;
  head.append(label, place, el("span", "note-id", card.id));
  button.append(head, el("div", "note-body", card.body));
  button.addEventListener("click", onClick);
  return button;
}

function noteItem(note, card) {
  const item = el("div", "note-item");
  item.append(noteCard(card, () => noteDetail.open(note.id)));
  if (card.sendable) item.append(sendButton(note));
  item.append(deleteButton(item, note));
  return item;
}

// The arrow on a card: sends the note alone to the default agent, or with Shift opens the
// send step with only it ticked (decided with the owner on 2026-11-23).
function sendButton(note) {
  const button = el("button", "note-send", "↗");
  button.type = "button";
  button.setAttribute("aria-label", `Send note ${note.id} alone`);
  button.title = `Send ${note.id} to ${state.settings?.agent ?? "the agent"} now · Shift-click to see the prompt first`;
  button.addEventListener("click", (event) => sendAlone(note, { review: event.shiftKey }));
  return button;
}

// A card of the send step: the same card, with its checkbox, and a click that ticks it.
function sendItem(note) {
  const item = el("div", "note-item");
  item.append(tickBox(note), noteCard(cardOf(note), () => toggleTick(note.id)), deleteButton(item, note));
  return item;
}

// The checkbox that puts a note in the send, or takes it out.
function tickBox(note) {
  const box = el("input", "note-tick");
  box.type = "checkbox";
  box.checked = state.send.ticked.has(note.id);
  box.setAttribute("aria-label", `Send note ${note.id}`);
  box.addEventListener("change", () => toggleTick(note.id));
  return box;
}

function toggleTick(id) {
  if (state.send.ticked.has(id)) state.send.ticked.delete(id);
  else state.send.ticked.add(id);
  renderNotes();
  showPrompt();
}

// The ×  on a note card. A note is the engineer's writing, so deleting takes two clicks: the
// first turns the button into "Delete", the second deletes. Leaving the card undoes the first.
function deleteButton(item, note) {
  const button = el("button", "note-delete", "×");
  button.type = "button";
  button.setAttribute("aria-label", `Delete note ${note.id}`);
  const reset = () => {
    item.classList.remove("confirming");
    button.textContent = "×";
  };
  button.addEventListener("click", () => {
    if (item.classList.contains("confirming")) {
      deleteNote(note);
      return;
    }
    item.classList.add("confirming");
    button.textContent = "Delete";
  });
  item.addEventListener("mouseleave", reset);
  button.addEventListener("blur", reset);
  return button;
}

// Deletes the note, and with it every run none of whose notes is left (src/run.rs,
// `delete_runs_without_notes`): the runs read before are dropped and the latest is fetched again.
async function deleteNote(note) {
  const response = await fetch(`api/notes?id=${encodeURIComponent(note.id)}`, {
    method: "DELETE",
  });
  if (!response.ok) {
    // A run log that could not be removed: the note may be gone all the same.
    showToast(await response.text());
    await loadNotes();
  } else {
    state.notes = (await response.json()).notes;
    indexNotes();
    renderNotes();
    renderFiles();
    redecorateRows();
  }
  await runs.reload();
}

// Saves a note's new text, and draws the notes the reply holds. Resolves to whether it was
// saved; a refusal is shown, and the editor stays open with the text.
async function editNote(note, body) {
  const response = await fetch(`api/notes?id=${encodeURIComponent(note.id)}`, {
    method: "PATCH",
    body: JSON.stringify({ body }),
  });
  if (!response.ok) {
    showToast(await response.text());
    return false;
  }
  state.notes = (await response.json()).notes;
  indexNotes();
  renderNotes();
  redecorateRows();
  return true;
}

// Resolves or reopens a note (`action` is the route's last part, src/shell.rs), and draws
// the notes the reply holds.
async function changeNote(action, note) {
  const response = await fetch(`api/notes/${action}?id=${encodeURIComponent(note.id)}`, {
    method: "POST",
  });
  if (!response.ok) {
    showToast(await response.text());
    await loadNotes();
    return;
  }
  state.notes = (await response.json()).notes;
  indexNotes();
  renderNotes();
  renderFiles();
  redecorateRows();
}

async function showNote(note) {
  if (state.file?.path !== note.path && !(await openFile(note.path))) return;
  // A third of the way down reads better than the very top edge.
  const top = lineTop(note.lines.start - 1) - codeEl.clientHeight / 3;
  codeEl.scrollTo({ top, behavior: "smooth" });
}

// ---- Send and run ---------------------------------------------------------------------
//
// Sending (docs/design.md, "The loop", step 5): Cmd-Shift-Enter or the Send button opens the
// send step. The tray's notes get checkboxes, the open ones ticked (run.js, `sendable`),
// and the code pane shows the prompt exactly as the agent will receive it. Cmd-Shift-Enter
// again, or Send, sends; Escape closes the step. The run then shows in the tray's Run tab
// (run-panel.js), which takes the front whenever the agent needs the engineer.

const previewEl = $("prompt-preview");
const sendBarEl = $("send-bar");

const runs = runPanel({
  panelEl: $("run"),
  headerEl: $("run-header"),
  statusEl: $("agent-status"),
  detailEl: $("run-detail"),
  onChange: followRun,
  // An older run was read: the notes sent in it show its status.
  onLoaded: () => renderNotes(),
  onError: showToast,
  noteOf: (id) => state.notes.find((note) => note.id === id) ?? null,
  openNote: (id) => noteDetail.open(id),
  openFile,
  reviewers: () => state.settings?.reviewers ?? [],
  showChanges: (from, to) => {
    closeSend();
    changes.show(from, to);
  },
});

// What a run changed, over the open file; Esc, or opening a file, gives the pane back.
const changes = changesView({
  viewEl: $("changes-view"),
  setHeader: (title, meta) => {
    const close = el("button", "button changes-close", "esc");
    close.type = "button";
    close.title = "Back to the file";
    close.addEventListener("click", () => changes.close());
    $("crumbs").replaceChildren(el("b", "", title), close);
    $("file-meta").textContent = meta;
  },
  notes: () => state.notes,
  noteCard: (note) => noteCard(cardOf(note), () => noteDetail.open(note.id)),
  addNote: async (path, line) => {
    if (await openFile(path)) openComposer({ start: line - 1, end: line - 1 });
  },
  onError: showToast,
  onClose: () => {
    if (state.file) renderHeader(state.file.path);
    else {
      $("crumbs").replaceChildren();
      $("file-meta").textContent = "";
    }
  },
});

const noteDialogEl = $("note-dialog");
const noteDetail = noteDialog(noteDialogEl, {
  find: (id) => {
    const note = state.notes.find((candidate) => candidate.id === id);
    return note ? { note, run: runFor(note) } : null;
  },
  canSend: (note) => canSendAlone() && !isResolved(note),
  agent: () => state.settings?.agent ?? null,
  showInCode: showNote,
  send: sendAlone,
  remove: deleteNote,
  resolve: (note) => changeNote("resolve", note),
  editNote,
  reopen: (note) => changeNote("reopen", note),
  showRun: () => showTab("run"),
  answerQuestion: (request, content) => runs.answerQuestion(request, content),
  reviewOf: (id) => reviewOf(id, runs.knownViews()),
  reviewers: () => state.settings?.reviewers ?? [],
  startReview: (id) => runs.startReview(id),
  live: () => runs.live(),
});

// The agent of this send, or the settings' agent.
function chosenAgent() {
  return state.send?.agent ?? state.settings?.agent ?? null;
}

// The notes ticked for the send, in the tray's order, which is the prompt's order.
function tickedIds() {
  return state.notes.filter((note) => state.send.ticked.has(note.id)).map((note) => note.id);
}

function renderSendBar() {
  const open = state.notes.filter(isOpen).length;
  const shown =
    state.projectOpen && state.tab === "notes" && !runs.live() && (state.send !== null || open > 0);
  sendBarEl.hidden = !shown;
  if (!shown) return;

  // In the send step the bar is one split button: the wide part sends and names the agent,
  // the narrow part chooses it (chosen with the owner on 2026-11-23 over a separate picker).
  const picker = $("agent-picker");
  picker.hidden = !state.send;
  sendBarEl.classList.toggle("split", Boolean(state.send));
  const button = $("send-button");
  const count = state.send ? tickedIds().length : open;
  const noun = count === 1 ? "1 note" : `${count} notes`;
  const agent = chosenAgent();
  const words = !state.send ? `Send ${noun}…` : agent ? `Send ${noun} to ${agent}` : "Choose an agent";
  button.replaceChildren(el("span", "send-words", words), el("kbd", "", "⌘⇧↵"));
  button.disabled = Boolean(state.send) && agent !== null && (count === 0 || state.send.sending);
}

// Opens the send step with `ids` ticked: the open notes, or the one note of a Shift-click.
function openSend(ids = sendable(state.notes)) {
  if (runs.live()) {
    showToast("The agent is still working on the last run: wait for it, or stop it.");
    return;
  }
  // The prompt covers the code, and with it a note being written.
  if (guardDraft("sending")) return;
  if (state.composer) closeComposer({ save: false });
  closeMenu();
  changes.close();
  state.send = { ticked: new Set(ids), agent: null, sending: false };
  showTab("notes");
  renderNotes();
  showPrompt();
}

function closeSend() {
  if (!state.send) return;
  state.send = null;
  previewEl.hidden = true;
  if (state.file) renderHeader(state.file.path);
  else {
    $("crumbs").replaceChildren();
    $("file-meta").textContent = "";
  }
  renderNotes();
}

// Shows the prompt of the ticked notes in the code pane, as the Rust process builds it.
let promptRequest = 0;
async function showPrompt() {
  if (!state.send) return;
  const ids = tickedIds();
  const request = ++promptRequest;
  $("crumbs").replaceChildren(el("b", "", "What the agent receives"));
  previewEl.hidden = false;
  if (ids.length === 0) {
    $("file-meta").textContent = "";
    previewEl.replaceChildren(el("div", "empty", "Tick the notes to send."));
    return;
  }
  const response = await fetch(`api/run/preview?notes=${ids.join(",")}`);
  const reply = response.ok ? await response.json() : await response.text();
  // A later tick, or the step closed, while this was in flight.
  if (request !== promptRequest || !state.send) return;
  if (!response.ok) {
    previewEl.replaceChildren(el("div", "empty", reply));
    return;
  }

  const files = [...new Set(state.notes.filter((n) => state.send.ticked.has(n.id)).map((n) => n.path))];
  const notes = ids.length === 1 ? "1 note" : `${ids.length} notes`;
  $("file-meta").textContent = `${notes} · ${files.length === 1 ? "1 file" : `${files.length} files`}`;
  const text = el("pre", "prompt-text");
  for (const line of promptLines(reply.prompt)) {
    text.append(el("span", `prompt-${line.kind}`, `${line.text}\n`));
  }
  const links = el("div", "prompt-links");
  links.append(el("span", "prompt-links-label", "Linked files"));
  for (const path of files) links.append(el("span", "prompt-link", path));
  previewEl.replaceChildren(text, links);
}

async function sendNotes() {
  const agent = chosenAgent();
  if (!state.send || state.send.sending || tickedIds().length === 0 || !agent) return;
  state.send.sending = true;
  renderSendBar();
  if (!(await startRun(tickedIds(), agent))) {
    if (state.send) state.send.sending = false;
    renderSendBar();
    return;
  }
  closeSend();
  showTab("run");
}

// Sends `note` alone to the default agent, and stays on the notes: its card shows the run.
// With `review`, or with no agent chosen yet, opens the send step with only it ticked.
async function sendAlone(note, { review }) {
  const agent = state.settings?.agent;
  if (review || !agent) {
    openSend([note.id]);
    return;
  }
  if (await startRun([note.id], agent)) showTab("notes");
}

// Starts a run of `ids` with `agent`. Returns whether it started; a refusal is shown.
async function startRun(ids, agent) {
  const response = await fetch("api/run", { method: "POST", body: JSON.stringify({ notes: ids, agent }) });
  if (!response.ok) {
    showToast(await response.text());
    return false;
  }
  await runs.sync();
  return true;
}

$("send-button").addEventListener("click", () => {
  if (!state.send) openSend();
  else if (chosenAgent()) sendNotes();
  else $("agent-picker").click();
});

// The agents turned on in the settings, to send this one run to; the settings' agent is the
// default.
$("agent-picker").addEventListener("click", (event) => {
  const { left, top } = event.currentTarget.getBoundingClientRect();
  const agents = state.settings?.enabledAgents ?? [];
  const actions = agents.map((name) => ({
    label: name,
    detail: name === state.settings.agent ? "default" : "",
    key: name === chosenAgent() ? "✓" : "",
    run: () => {
      if (!state.send) return;
      state.send.agent = name;
      renderSendBar();
    },
  }));
  actions.push({
    label: agents.length === 0 ? "Turn on an agent in Settings…" : "Agents in Settings…",
    separated: agents.length > 0,
    run: () => settings.open(),
  });
  openMenu(left, top, actions);
});

function showTab(tab) {
  state.tab = tab;
  const run = tab === "run";
  if (run) closeSend();
  $("tab-notes").setAttribute("aria-selected", String(!run));
  $("tab-run").setAttribute("aria-selected", String(run));
  notesEl.hidden = run;
  $("run").hidden = !run;
  $("note-count").hidden = run;
  $("run-header").hidden = !run;
  renderSendBar();
}

$("tab-notes").addEventListener("click", () => showTab("notes"));
$("tab-run").addEventListener("click", () => showTab("run"));
$("agent-status").addEventListener("click", () => showTab("run"));

// File edits the run has finished, to tell when the open file may have changed.
function editsDone(run) {
  return foldRun(run.entries).items.filter(
    (item) =>
      item.type === "tool" && item.status === "completed" && ["edit", "delete", "move"].includes(item.tool),
  ).length;
}

// Called after every change to the run.
function followRun(run, previous) {
  $("tab-run").hidden = run === null;
  if (run === null) {
    if (state.tab === "run") showTab("notes");
    return;
  }
  const fresh = previous?.id !== run.id;
  // A run that needs the engineer comes to the front. One that starts does not: the send
  // step shows it (`sendNotes`), and a note sent alone from its card is followed on the card.
  if (run.status === "waiting" && previous?.status !== "waiting") showTab("run");
  // The agent edits files, and the notes are marked sent once it has the prompt: both are
  // read again when the run moves on and when an edit finishes. There is no file watcher
  // (docs/design.md, "Working with the repository").
  if (!fresh && (previous.status !== run.status || editsDone(previous) !== editsDone(run))) {
    refresh().catch((error) => showToast(String(error)));
  } else if (
    !run.problem &&
    // A follow-up marks no note sent, so it is not waited for.
    !run.followUp &&
    run.notes.some((id) => state.notes.find((note) => note.id === id && isOpen(note)))
  ) {
    // Its notes are marked sent once the agent takes the turn, which is no status change and
    // no edit: read them until they show it. A run whose notes could not be marked says so
    // in `problem`, and they are not read on every update after that.
    loadNotes().catch((error) => showToast(String(error)));
  }
  renderNotes();
}

// ---- Panes ----------------------------------------------------------------------------
//
// The file list and the tray are resized by the dividers beside them. The widths are kept per
// Mac in layout.json (src/layout.rs); ui/panes.js has the rules. Until the kept widths arrive,
// app.css's defaults stand.

const panes = {
  // As `api/layout` gives them: { sidebar, tray }, the defaults, and the limits.
  layout: null,
  defaults: null,
  limits: null,
  // The divider being dragged: "files" or "tray".
  dragging: null,
  saveTimer: 0,
};

async function loadLayout() {
  const response = await fetch("api/layout");
  if (!response.ok) return;
  const { defaults, limits, ...layout } = await response.json();
  panes.defaults = defaults;
  panes.limits = limits;
  for (const [id, [min, max]] of [
    ["divider-files", limits.sidebar],
    ["divider-tray", limits.tray],
  ]) {
    $(id).setAttribute("aria-valuemin", String(min));
    $(id).setAttribute("aria-valuemax", String(max));
  }
  setLayout(layout, { save: false });
}

// Draws `layout`, fitted to the window, and saves it unless asked not to. Saving waits for a
// moment of quiet, so holding an arrow key writes the file once.
function setLayout(layout, { save }) {
  panes.layout = layout;
  drawLayout();
  if (!save) return;
  clearTimeout(panes.saveTimer);
  panes.saveTimer = setTimeout(saveLayout, 300);
}

function drawLayout() {
  if (!panes.layout) return;
  const { sidebar, tray } = fit(panes.layout, panes.limits, innerWidth);
  const root = document.documentElement.style;
  root.setProperty("--sidebar-width", `${sidebar}px`);
  root.setProperty("--tray-width", `${tray}px`);
  $("divider-files").setAttribute("aria-valuenow", String(sidebar));
  $("divider-tray").setAttribute("aria-valuenow", String(tray));
}

async function saveLayout() {
  const response = await fetch("api/layout", { method: "PUT", body: JSON.stringify(panes.layout) });
  // The widths still apply in this window; the toast says they will not be kept.
  if (!response.ok) showToast(await response.text());
}

for (const [id, divider] of [
  ["divider-files", "files"],
  ["divider-tray", "tray"],
]) {
  const el = $(id);
  const stop = () => {
    if (panes.dragging !== divider) return;
    panes.dragging = null;
    el.classList.remove("active");
    document.body.classList.remove("resizing");
    setLayout(panes.layout, { save: true });
  };
  el.addEventListener("pointerdown", (event) => {
    if (event.button !== 0 || !panes.layout) return;
    event.preventDefault();
    el.setPointerCapture(event.pointerId);
    panes.dragging = divider;
    el.classList.add("active");
    document.body.classList.add("resizing");
  });
  el.addEventListener("pointermove", (event) => {
    if (panes.dragging !== divider) return;
    // A release outside the window can arrive without a pointerup here.
    if ((event.buttons & 1) === 0) return stop();
    const next =
      divider === "files"
        ? dragFiles(panes.layout, panes.limits, event.clientX, innerWidth)
        : dragTray(panes.layout, panes.limits, event.clientX, innerWidth);
    setLayout(next, { save: false });
  });
  // Fires after pointerup and pointercancel, and also when the capture is lost without either:
  // a dialog opened with Cmd-, mid-drag makes the divider inert.
  el.addEventListener("lostpointercapture", stop);
  el.addEventListener("dblclick", () => {
    if (panes.layout) setLayout(reset(panes.layout, panes.defaults, divider), { save: true });
  });
  el.addEventListener("keydown", (event) => {
    if (!panes.layout || (event.key !== "ArrowLeft" && event.key !== "ArrowRight")) return;
    // The arrows move the divider and nothing else; the window's other keys still work here.
    event.preventDefault();
    event.stopPropagation();
    const size = event.shiftKey ? BIG_STEP : STEP;
    const delta = event.key === "ArrowRight" ? size : -size;
    setLayout(step(panes.layout, panes.limits, divider, delta, innerWidth), { save: true });
  });
}

// The kept widths are fitted to the window again whenever it changes size; that is drawing,
// not a change of layout, so nothing is saved.
window.addEventListener("resize", drawLayout);

// ---- Input ----------------------------------------------------------------------------

let renderQueued = false;
codeEl.addEventListener(
  "scroll",
  () => {
    closeMenu();
    if (renderQueued) return;
    renderQueued = true;
    requestAnimationFrame(() => {
      renderQueued = false;
      renderCode();
    });
  },
  { passive: true },
);

new ResizeObserver(() => renderCode()).observe(codeEl);

filesEl.addEventListener("click", (event) => {
  const row = event.target.closest(".file, .dir");
  if (!row) return;
  if (row.classList.contains("dir")) {
    setFolderOpen(row.dataset.path, row.getAttribute("aria-expanded") !== "true");
  } else {
    openFile(row.dataset.path);
  }
});

filesEl.addEventListener("focusin", (event) => {
  const row = event.target.closest(".file, .dir");
  if (row && filesEl.getAttribute("role") === "tree") setTabStop(row);
});

// The keys of a file tree in an IDE: up and down move between visible rows; right opens a
// folder, or moves into an open one; left closes an open folder, or moves to the parent.
// Enter and Space are the buttons' own: open the file, or toggle the folder.
filesEl.addEventListener("keydown", (event) => {
  const row = event.target.closest(".file, .dir");
  if (!row || filesEl.getAttribute("role") !== "tree") return;

  const rows = [...filesEl.querySelectorAll(".file, .dir")];
  const index = rows.indexOf(row);
  const path = row.dataset.path;
  const folder = row.classList.contains("dir");
  const open = row.getAttribute("aria-expanded") === "true";

  if (event.key === "ArrowDown") rows[index + 1]?.focus();
  else if (event.key === "ArrowUp") rows[index - 1]?.focus();
  else if (event.key === "Home") rows[0]?.focus();
  else if (event.key === "End") rows.at(-1)?.focus();
  else if (event.key === "ArrowRight" && folder && !open) setFolderOpen(path, true);
  else if (event.key === "ArrowRight" && folder) rows[index + 1]?.focus();
  else if (event.key === "ArrowLeft" && folder && open) setFolderOpen(path, false);
  else if (event.key === "ArrowLeft") {
    const parent = ancestors(path).at(-1);
    if (parent !== undefined) filesEl.querySelector(rowSelector(parent))?.focus();
  } else return;
  event.preventDefault();
});

filterEl.addEventListener("input", () => {
  state.activeMatch = 0;
  renderFiles();
});
filterEl.addEventListener("keydown", (event) => {
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    // `filteredFileRows` clamps the upper end to the number of matches.
    state.activeMatch = Math.max(0, state.activeMatch + (event.key === "ArrowDown" ? 1 : -1));
    renderFiles();
    filesEl.querySelector(".file.active")?.scrollIntoView({ block: "nearest" });
  } else if (event.key === "Enter") {
    // During IME composition Enter confirms the text; with no filter text there is no match.
    if (event.isComposing || !filterEl.value.trim()) return;
    const active = filesEl.querySelector(".file.active") ?? filesEl.querySelector(".file");
    // Focus moves to the code so the next `c` opens the composer instead of typing a letter.
    if (active) openFile(active.dataset.path).then((shown) => shown && codeEl.focus());
  } else if (event.key === "Escape") {
    // The window's Escape handler would otherwise discard an open draft note.
    event.stopPropagation();
    filterEl.value = "";
    renderFiles();
    codeEl.focus();
  }
});

window.addEventListener("keydown", (event) => {
  // Cmd-Q, Cmd-W, Cmd-O, Cmd-Shift-O and Cmd-P belong to the menu bar (src/menu.rs), which
  // calls `window.postil.menu`. The page leaves them alone, so each shortcut is defined once.
  // A dialog and the welcome screen cover the panes: their keys must not act on what cannot be
  // seen. Each dialog handles its own keys (Escape closes it).
  if (document.querySelector("dialog[open]")) return;
  if (!welcomeEl.hidden) {
    if (event.key === "Escape" && state.projectOpen) hideWelcome();
    return;
  }

  if (event.key === "Enter" && event.metaKey && event.shiftKey) {
    event.preventDefault();
    if (state.send) sendNotes();
    else if (state.projectOpen) openSend();
    return;
  }

  const typing = event.target.matches("input, textarea");
  // `a`, Shift-`a` and `r` answer a permission request while the run shows.
  if (!typing && !state.composer && state.tab === "run" && runs.keydown(event)) return;
  if (event.key === "Escape") {
    if (state.composer) closeComposer({ save: false });
    else if (state.send) closeSend();
    else if (state.selection) {
      state.selection = null;
      redecorateRows();
    } else if (runs.live()) runs.stop();
  } else if (state.send) {
    // The code is covered by the prompt: no key acts on it.
  } else if (event.key === "c" && event.metaKey && !typing && state.file && state.selection) {
    // With text selected, the browser copies and the `copy` handler fixes the text. Lines
    // chosen in the gutter have no text selection, so the whole lines are copied here.
    if (selectedCodeText() !== null) return;
    event.preventDefault();
    copyText(linesText(selectedRange()));
  } else if (event.key === "c" && !typing && !event.metaKey && !event.ctrlKey) {
    event.preventDefault();
    openComposer();
  }
});

// A draft note lives only in the page, so anything that would end the page (quitting,
// switching projects) first sends the engineer back to it. Returns whether there is one.
function guardDraft(action) {
  if (!state.composer?.text.trim()) return false;
  // The note must be in view to be saved or discarded.
  if (settingsEl.open) settingsEl.close();
  hideWelcome();
  const label = state.composer.node.querySelector(".composer-label");
  label.textContent = `Save or discard this note before ${action}.`;
  label.classList.add("failed");
  state.composer.node.querySelector("textarea").focus();
  return true;
}

function quit() {
  if (!guardDraft("quitting")) window.ipc.postMessage("quit");
}

// There is no file watcher (docs/design.md, "Working with the repository"): the file list,
// the open file, and the notes change while the engineer is in another app, so all three are
// read again on return and drawn once, so the notes never show on the old text. Not while
// the composer is open: reading the file again would move its lines under the text.
let refreshes = 0;
async function refresh() {
  // A project being opened replaces this page: what a refresh drew would be thrown away,
  // and its requests would already reach the new project.
  if (state.switching) return;
  runs.forget();
  // The welcome screen is what is on screen, whether or not a project is open behind it.
  if (!welcomeEl.hidden) {
    await loadRecent();
    return;
  }
  if (!state.projectOpen || state.composer) return;
  const path = state.wanted;
  // A later refresh may answer first, as a run's edits come one after another: only the
  // latest refresh draws, or an older file would be painted over a newer one.
  const generation = ++refreshes;
  const [repo, notes, file] = await Promise.all([
    fetch("api/repo").then(async (response) => (response.ok ? response.json() : null)),
    fetchNotes(),
    path ? fetchFile(path) : null,
  ]);
  // The engineer moved on while the replies were in flight: their action wins.
  if (state.composer || state.wanted !== path || generation !== refreshes) return;

  if (repo) showRepo(repo);
  applyNotes(notes);
  if (file) showFile(path, file, { keepView: true });
  else indexNotes();
  renderNotes();
  renderFiles();
}

window.addEventListener("focus", () => {
  refresh().catch((error) => notesEl.replaceChildren(el("div", "empty", String(error))));
});

// The window has no title bar, so the page says where it can be dragged from.
window.addEventListener("mousedown", (event) => {
  if (event.button !== 0 || !event.target.closest("[data-drag]")) return;
  if (event.target.closest("button, input, textarea")) return;
  window.ipc.postMessage("drag");
});

// ---- Benchmark ------------------------------------------------------------------------
//
// Runs when the app was started with `--bench FILE`. Opens the file, scrolls through it one
// step per frame, and posts the timings to the Rust process, which prints them and exits.

const nextFrame = () => new Promise((resolve) => requestAnimationFrame(resolve));

async function runBench(path) {
  const openStarted = performance.now();
  await openFile(path);
  await nextFrame();
  await nextFrame();
  window.ipc.postMessage("bench-painted");
  const paintedAtMs = performance.now();
  const openMs = paintedAtMs - openStarted;

  // 300 frames of 120 px, about five lines per frame: a fast, steady flick.
  const frameMs = [];
  let previous = await nextFrame();
  for (let frame = 0; frame < 300; frame += 1) {
    codeEl.scrollTop += 120;
    const now = await nextFrame();
    frameMs.push(now - previous);
    previous = now;
  }

  frameMs.sort((a, b) => a - b);
  const at = (fraction) => Number(frameMs[Math.floor((frameMs.length - 1) * fraction)].toFixed(2));
  const median = at(0.5);
  const report = {
    file: path,
    lines: lineCount(),
    // From the page starting to run until the file was painted, and the open alone.
    pagePaintedAtMs: Math.round(paintedAtMs),
    openFileMs: Math.round(openMs),
    frames: frameMs.length,
    frameMs: { p50: median, p95: at(0.95), max: at(1) },
    // Frames that took more than one and a half times the median frame, which is the display
    // interval when scrolling keeps up: a visible stutter.
    slowFrames: frameMs.filter((ms) => ms > median * 1.5).length,
    scrolledPx: Math.round(codeEl.scrollTop),
  };
  window.ipc.postMessage(`bench:${JSON.stringify(report)}`);
}

// ---- Start ----------------------------------------------------------------------------

function showRepo(repo) {
  state.repo = repo;
  state.tree = buildTree(repo.files);
  $("repo-name").textContent = repo.name;
  $("repo-name").title = `${repo.root}\nSwitch project (⇧⌘O)`;
  $("repo-branch").textContent = repo.branch || "detached HEAD";
  // Set once by the Rust process when the recent projects could not be updated. It does
  // not stop anything, so it is a line under the branch, not a dialog.
  const warning = $("repo-warning");
  warning.hidden = !repo.warning;
  if (repo.warning) warning.textContent = repo.warning;
}

// ---- Projects -------------------------------------------------------------------------
//
// One window shows one project. Without one, the welcome screen offers Open Folder, the
// recent projects, and dropping a folder on the window. Opening another project replaces the
// current one in the Rust process; the page then reloads, which resets everything it drew.

const welcomeEl = $("welcome");

// What the Rust process calls into the page:
// - `openFolder`, with the folder from the Open Folder dialog, or one dropped on the window;
// - `menu`, with the id of a menu bar command (src/menu.rs, `Command::id`). The page runs
//   them, so they share its guards, such as a draft note blocking Quit.
window.postil = {
  openFolder: (path) => {
    closeMenu();
    requestProject("api/open", path);
  },
  menu: (command) => {
    // A click in the menu bar reaches the page as no event at all, so nothing else closes
    // the right-click menu: it would stay drawn over the welcome screen or the filter.
    closeMenu();
    if (command === "open-folder") chooseFolder();
    else if (command === "switch-project") showWelcome();
    else if (command === "go-to-file") goToFile();
    else if (command === "settings") settings.open();
    else if (command === "close-window" || command === "quit") quit();
  },
  // The run changed (src/run.rs); fetch what is new.
  runChanged: () => runs.sync(),
};

// Focuses the file filter, as Cmd-P does. Nothing to find on the welcome screen.
function goToFile() {
  if (!welcomeEl.hidden) return;
  filterEl.focus();
  filterEl.select();
}

// Checked before the dialog opens too, so a folder is never chosen only to be refused.
function chooseFolder() {
  if (!guardDraft("opening another project")) window.ipc.postMessage("choose-folder");
}

// Asks the Rust process to open `path` (`api/open`), or to make it a git repository first
// (`api/init`). On success the page reloads into the project. A draft note would be lost by
// the reload, so the composer keeps the focus instead.
async function requestProject(route, path) {
  if (guardDraft("opening another project")) return;
  // A save still waiting would land in the next project: its folders are dropped instead.
  clearTimeout(foldersTimer);
  state.switching = true;
  let response;
  try {
    response = await fetch(route, { method: "POST", body: JSON.stringify({ path }) });
  } catch (error) {
    state.switching = false;
    showWelcome();
    showWelcomeError(`cannot open ${path}: ${error}`, null);
    return;
  }
  if (response.ok) {
    location.reload();
    return;
  }
  state.switching = false;
  // 422: the folder is not in a repository, which `git init` can fix in place.
  showWelcome();
  showWelcomeError(await response.text(), response.status === 422 ? path : null);
}

function showWelcomeError(text, folderToInit) {
  $("welcome-error").hidden = false;
  $("welcome-error-text").textContent = text;
  const init = $("init-repo");
  init.hidden = folderToInit === null;
  init.onclick = () => requestProject("api/init", folderToInit);
}

function showWelcome() {
  document.body.classList.add("welcome-mode");
  welcomeEl.hidden = false;
  const back = $("welcome-back");
  back.hidden = !state.projectOpen;
  if (state.projectOpen) back.textContent = `Back to ${state.repo.name}`;
  loadRecent();
  $("open-folder").focus();
}

function hideWelcome() {
  document.body.classList.remove("welcome-mode");
  welcomeEl.hidden = true;
  $("welcome-error").hidden = true;
  codeEl.focus();
}

async function loadRecent() {
  const response = await fetch("api/recent");
  const projects = response.ok ? await response.json() : [];
  const recentEl = $("recent");
  if (projects.length === 0) {
    recentEl.replaceChildren(el("div", "recent-empty", "Projects you open appear here."));
    return;
  }
  recentEl.replaceChildren(
    ...projects.map((project) => {
      const row = el("button", "recent-item");
      row.type = "button";
      row.disabled = !project.exists;
      row.append(icon(folderIcon(project.name, false)), el("span", "recent-name", project.name));
      const where = project.exists ? project.path : `${project.path} (moved or deleted)`;
      row.append(el("span", "recent-path", where));
      row.addEventListener("click", () => requestProject("api/open", project.path));
      return row;
    }),
  );
}

$("open-folder").addEventListener("click", chooseFolder);
$("welcome-back").addEventListener("click", hideWelcome);
$("repo-name").addEventListener("click", showWelcome);

// ---- Settings -------------------------------------------------------------------------
//
// The Rust process keeps them (src/settings.rs) and sets the window's theme at start; the
// page applies the rest, and sends a changed theme back to the window.

const settingsEl = $("settings");

// The theme the window has, so only a change is sent.
let appliedTheme = null;

function applySettings(settings) {
  state.settings = settings;
  const metrics = codeMetrics(settings.codeFontSize);
  const root = document.documentElement.style;
  root.setProperty("--code-font-size", `${metrics.fontSize}px`);
  root.setProperty("--line-height", `${metrics.lineHeight}px`);
  root.setProperty("--tab-width", String(settings.tabWidth));
  if (metrics.lineHeight !== lineHeight) {
    // The line at the top stays at the top: the scroll offset is in pixels, and the same
    // offset at another line height would show another part of the file.
    const top = lineAt(codeEl.scrollTop);
    lineHeight = metrics.lineHeight;
    if (state.file) {
      // Rows are placed by line height, so they are placed again, and the padding that
      // stands in for the other lines grows or shrinks with them. Only then can the scroll
      // offset go where the top line now is: before, the browser would cut it to the old
      // height of the file.
      renderCode({ force: true });
      codeEl.scrollTop = lineTop(top);
      renderCode({ force: true });
    }
  }
  // The first settings are the ones the window started with.
  if (appliedTheme !== null && settings.theme !== appliedTheme) {
    window.ipc.postMessage(`theme:${settings.theme}`);
  }
  appliedTheme = settings.theme;
  renderSendBar();
}

const settings = settingsDialog(settingsEl, { apply: applySettings });
$("open-settings").addEventListener("click", () => settings.open());

async function start() {
  // The icons load beside the repository, not before it: the tree can draw without them.
  loadIcons();
  // Before the first file is drawn, so it is drawn once at the right size. The defaults in
  // app.css stand when the settings cannot be read; the dialog says why when opened.
  await Promise.all([settings.load().catch(() => {}), loadLayout().catch(() => {})]);
  const response = await fetch("api/repo");
  if (!response.ok) {
    codeEmptyEl.textContent = await response.text();
    return;
  }
  const repo = await response.json();
  if (repo === null) {
    showWelcome();
    return;
  }
  state.projectOpen = true;
  showRepo(repo);
  await loadFolders().catch(() => {});
  renderFiles();
  await loadNotes();
  // The latest run, this session's or one from before a restart.
  await runs.sync();

  if (state.repo.bench) await runBench(state.repo.bench);
}

start();
