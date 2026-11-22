// The page of the Postil app. It draws what the Rust process serves under `api/` and has no
// build step and no dependencies: this file is compiled into the binary as it is.
//
// Milestone 0: notes live only in this page's memory and are lost on reload. Milestone 2
// moves them into the Rust process (docs/design.md, "Milestones").

import { rank } from "./fuzzy.js";

const $ = (id) => document.getElementById(id);

const filesEl = $("files");
const filterEl = $("filter");
const codeEl = $("code");
const rowsEl = $("code-rows");
const codeEmptyEl = $("code-empty");
const notesEl = $("notes");

// One code line's height in px, owned by `--line-height` in app.css.
const LINE_HEIGHT = parseFloat(
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
  // The open note composer: { start, end, text }, lines 0-based and inclusive.
  composer: null,
  composerHeight: COMPOSER_HEIGHT_GUESS,
  // { id, path, start, end, body }, lines 0-based and inclusive.
  notes: [],
  // Derived from `notes` by `indexNotes`: the note on each line of the open file, and the
  // number of notes in each file.
  noteByLine: new Map(),
  noteCountByPath: new Map(),
  // The index of the highlighted row while the file filter has text.
  activeMatch: 0,
  // The rendered window of lines: [first, last).
  rendered: { first: 0, last: 0 },
};

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// ---- Files ----------------------------------------------------------------------------

function renderFiles() {
  const needle = filterEl.value.trim().toLowerCase();
  const rows = needle ? filteredFileRows(needle) : treeFileRows();
  filesEl.replaceChildren(...rows);
}

// With a filter: a flat list of the best fuzzy matches, with the matched characters marked.
// The row at `state.activeMatch` is the one Enter opens.
function filteredFileRows(needle) {
  const matches = rank(needle, state.repo.files, MAX_MATCHES);
  state.activeMatch = Math.min(state.activeMatch, Math.max(0, matches.length - 1));
  return matches.map(({ path, positions }, index) => {
    const row = fileRow(path, markedPath(path, positions), 0);
    row.classList.toggle("active", index === state.activeMatch);
    return row;
  });
}

// `path` as a label with the characters at `positions` wrapped in <mark>.
function markedPath(path, positions) {
  const label = el("span");
  const marked = new Set(positions);
  for (let index = 0; index < path.length; index += 1) {
    label.append(marked.has(index) ? el("mark", "", path[index]) : path[index]);
  }
  return label;
}

// Without a filter: the sorted paths as an indented tree. A directory row is emitted the
// first time a path enters that directory.
function treeFileRows() {
  const rows = [];
  let open = [];
  for (const path of state.repo.files) {
    const parts = path.split("/");
    const dirs = parts.slice(0, -1);
    let shared = 0;
    while (shared < dirs.length && shared < open.length && dirs[shared] === open[shared]) {
      shared += 1;
    }
    for (let depth = shared; depth < dirs.length; depth += 1) {
      const row = el("div", "dir", dirs[depth]);
      row.style.paddingLeft = `${8 + 14 * depth}px`;
      rows.push(row);
    }
    open = dirs;
    rows.push(fileRow(path, el("span", "", parts.at(-1)), dirs.length));
  }
  return rows;
}

function fileRow(path, label, depth) {
  const row = el("button", "file");
  row.type = "button";
  row.dataset.path = path;
  row.style.paddingLeft = `${8 + 14 * depth}px`;
  row.append(label);
  if (state.file?.path === path) row.classList.add("open");

  const count = state.noteCountByPath.get(path);
  if (count) row.append(el("span", "count", String(count)));
  return row;
}

async function openFile(path) {
  state.wanted = path;
  const response = await fetch(`api/file?path=${encodeURIComponent(path)}`);
  const body = response.ok ? await response.json() : await response.text();
  if (state.wanted !== path) return;

  state.selection = null;
  state.composer = null;
  if (response.ok) {
    state.file = body;
    codeEmptyEl.hidden = true;
  } else {
    state.file = null;
    codeEmptyEl.hidden = false;
    codeEmptyEl.textContent = body;
  }

  indexNotes();
  renderHeader(path);
  renderFiles();
  codeEl.scrollTop = 0;
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
  return ROWS_PADDING + index * LINE_HEIGHT + pushed;
}

// The line at scroll offset `y`: the inverse of `lineTop`.
function lineAt(y) {
  const composer = state.composer;
  if (composer) {
    const composerTop = lineTop(composer.end) + LINE_HEIGHT;
    if (y >= composerTop + state.composerHeight) y -= state.composerHeight;
    else if (y >= composerTop) return composer.end;
  }
  const index = Math.floor((y - ROWS_PADDING) / LINE_HEIGHT);
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
    if (state.composer?.end === index) fragment.append(composerElement());
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
  rowsEl.style.paddingTop = `${ROWS_PADDING + first * LINE_HEIGHT + above}px`;
  rowsEl.style.paddingBottom = `${ROWS_PADDING + (lineCount() - last) * LINE_HEIGHT + below}px`;
}

function lineRow(index) {
  const row = el("div", "ln");
  row.dataset.line = String(index);
  row.append(el("span", "n", String(index + 1)));

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
  if (note && note.end === index) {
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

window.addEventListener("mouseup", () => {
  dragging = false;
});

// ---- Composer and notes ---------------------------------------------------------------

function openComposer() {
  const range = selectedRange();
  if (!range || state.composer) return;
  state.composer = { ...range, text: "" };
  renderCode({ force: true });
  rowsEl.querySelector("textarea")?.focus();
}

function closeComposer({ save }) {
  const composer = state.composer;
  if (!composer) return;
  const body = composer.text.trim();
  if (save && body) {
    const id = `N${state.notes.length + 1}`;
    state.notes.push({ id, path: state.file.path, start: composer.start, end: composer.end, body });
    indexNotes();
  }
  state.composer = null;
  state.selection = null;
  renderCode({ force: true });
  renderNotes();
  renderFiles();
  codeEl.focus();
}

// Rebuilds the two lookups derived from `state.notes`. When notes overlap on a line, the
// newest one is shown there.
function indexNotes() {
  state.noteByLine = new Map();
  state.noteCountByPath = new Map();
  for (const note of state.notes) {
    state.noteCountByPath.set(note.path, (state.noteCountByPath.get(note.path) ?? 0) + 1);
    if (note.path !== state.file?.path) continue;
    for (let line = note.start; line <= note.end; line += 1) {
      state.noteByLine.set(line, note);
    }
  }
}

function composerElement() {
  const composer = state.composer;
  const node = $("composer-template").content.firstElementChild.cloneNode(true);

  const label =
    composer.start === composer.end
      ? `Note on line ${composer.start + 1}`
      : `Note on lines ${composer.start + 1} to ${composer.end + 1}`;
  node.querySelector("label").textContent = label;

  const textarea = node.querySelector("textarea");
  textarea.value = composer.text;
  textarea.addEventListener("input", () => {
    composer.text = textarea.value;
    // Grow with the text, then tell the virtual list the new height.
    textarea.style.height = "auto";
    textarea.style.height = `${textarea.scrollHeight}px`;
    measureComposer(node);
  });
  textarea.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.metaKey) closeComposer({ save: true });
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

function renderNotes() {
  const count = state.notes.length;
  $("note-count").textContent = count === 1 ? "1 note" : `${count} notes`;
  if (count === 0) return;

  const cards = state.notes.map((note) => {
    const card = el("button", "note");
    card.type = "button";
    const lines = note.start === note.end ? note.start + 1 : `${note.start + 1}-${note.end + 1}`;
    const where = el("div", "note-where");
    where.append(el("span", "", `${note.path}:${lines}`), el("span", "note-id", note.id));
    card.append(where, el("div", "note-body", note.body));
    card.addEventListener("click", () => showNote(note));
    return card;
  });
  notesEl.replaceChildren(...cards);
}

async function showNote(note) {
  if (state.file?.path !== note.path) await openFile(note.path);
  // A third of the way down reads better than the very top edge.
  const top = lineTop(note.start) - codeEl.clientHeight / 3;
  codeEl.scrollTo({ top, behavior: "smooth" });
}

// ---- Input ----------------------------------------------------------------------------

let renderQueued = false;
codeEl.addEventListener(
  "scroll",
  () => {
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
  const row = event.target.closest(".file");
  if (row) openFile(row.dataset.path);
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
    const active = filesEl.querySelector(".file.active") ?? filesEl.querySelector(".file");
    if (active) openFile(active.dataset.path);
  } else if (event.key === "Escape") {
    filterEl.value = "";
    renderFiles();
    codeEl.focus();
  }
});

window.addEventListener("keydown", (event) => {
  const typing = event.target.matches("input, textarea");
  if (event.key === "Escape") {
    if (state.composer) closeComposer({ save: false });
    else if (state.selection) {
      state.selection = null;
      redecorateRows();
    }
  } else if (event.key === "c" && !typing && !event.metaKey && !event.ctrlKey) {
    event.preventDefault();
    openComposer();
  } else if (event.key === "p" && event.metaKey) {
    event.preventDefault();
    filterEl.focus();
    filterEl.select();
  }
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
    // Frames that took more than one and a half display intervals: a visible stutter.
    slowFrames: frameMs.filter((ms) => ms > median * 1.5).length,
    scrolledPx: Math.round(codeEl.scrollTop),
  };
  window.ipc.postMessage(`bench:${JSON.stringify(report)}`);
}

// ---- Start ----------------------------------------------------------------------------

async function start() {
  const response = await fetch("api/repo");
  if (!response.ok) {
    codeEmptyEl.textContent = await response.text();
    return;
  }
  state.repo = await response.json();
  $("repo-name").textContent = state.repo.name;
  $("repo-branch").textContent = state.repo.branch || "detached HEAD";
  renderFiles();

  if (state.repo.bench) await runBench(state.repo.bench);
}

start();
