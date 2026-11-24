// The page of the Postil app. It draws what the Rust process serves under `api/` and has no
// build step and no dependencies: its modules in ui/ are compiled into the binary as they are.
// This module holds the page's state, builds the views over it, and wires them together.
//
// The Rust process owns the notes (`api/notes`, stored in the git directory). The page keeps
// a copy only to draw it, and fetches it again whenever it may have changed: at start, after
// saving, and when the window regains focus.

import { runBench } from "./bench.js";
import { changesView } from "./changes-view.js";
import { codeView } from "./code-view.js";
import { contextMenu } from "./context-menu.js";
import { dividers } from "./dividers.js";
import { $, el, loadIcons, showToast } from "./dom.js";
import { fileList } from "./file-list.js";
import { noteDialog } from "./note-dialog.js";
import { onLines } from "./note-place.js";
import { notesTray } from "./notes-tray.js";
import { pathNoteDialog } from "./path-note-dialog.js";
import { runPanel } from "./run-panel.js";
import { escapeAction, foldRun, isOpen, isResolved } from "./run.js";
import { settingsDialog } from "./settings-dialog.js";
import { codeMetrics } from "./settings.js";
import { reviewOf } from "./status.js";
import { buildTree } from "./tree.js";
import { welcomeScreen } from "./welcome.js";

const filesEl = $("files");
const filterEl = $("filter");
const codeEl = $("code");
const notesEl = $("notes");
const welcomeEl = $("welcome");

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
  // As `api/notes` returns them: { id, path, target, lines: { start, end }, snippet, body,
  // anchor, state }, lines 1-based and inclusive, anchor "found" or "lost". `target` is
  // "lines", "file" or "folder", and only a note on lines has `lines` and `snippet`
  // (note-place.js). A lost note is shown as outdated.
  notes: [],
  // Derived from `notes` by `indexNotes`: the open note on each 0-based line of the open
  // file, and the number of notes on each file or folder path.
  noteByLine: new Map(),
  noteCountByPath: new Map(),
  // The repository's files as a tree (tree.js), rebuilt when the file list changes, and the
  // paths of the folders open in it.
  tree: buildTree([]),
  expanded: new Set(),
  // Whether the Rust process has a project open; without one, the welcome screen shows.
  projectOpen: false,
  // Set while a project is being opened, until the page reloads into it.
  switching: false,
  // The settings in effect (`applySettings`), for the agent notes are sent to.
  settings: null,
  // The send step while it is open: the ids of the notes ticked, and the agent chosen for
  // this send. The code pane then shows the prompt instead of the file.
  send: null,
  // Which of the tray's tabs shows: "notes" or "run".
  tab: "notes",
};

// ---- The views ----------------------------------------------------------------------------
//
// Each view is built once, here, from the state and the callbacks it needs. A callback that
// names a view built further down runs only after start, when every view exists.

const menu = contextMenu({ menuEl: $("context-menu"), codeEl });

const files = fileList({
  state,
  filesEl,
  filterEl,
  codeEl,
  openFile: (path) => code.openFile(path),
  menu,
  writeNote: (path, target) => pathNote.open(path, target),
});

const code = codeView({
  state,
  codeEl,
  rowsEl: $("code-rows"),
  codeEmptyEl: $("code-empty"),
  menu,
  files,
  indexNotes,
  deleteNote,
  beforeOpen: () => {
    changes.close();
    tray.closeSend();
  },
  headerHeld: () => changes.isOpen() || Boolean(state.send),
  onComposerClosed: () => {
    tray.render();
    files.render();
  },
});

const runs = runPanel({
  panelEl: $("run"),
  headerEl: $("run-header"),
  statusEl: $("agent-status"),
  detailEl: $("run-detail"),
  onChange: followRun,
  // An older run was read: the notes sent in it show its status.
  onLoaded: () => tray.render(),
  onError: showToast,
  noteOf: (id) => state.notes.find((note) => note.id === id) ?? null,
  openNote: (id) => noteDetail.open(id),
  openFile: (path) => code.openFile(path),
  reviewers: () => state.settings?.reviewers ?? [],
  showChanges: (from, to) => {
    tray.closeSend();
    changes.show(from, to);
  },
});

const tray = notesTray({
  state,
  notesEl,
  runs,
  menu,
  openNote: (id) => noteDetail.open(id),
  drawNoteDetail: () => noteDetail.draw(),
  deleteNote,
  openSettings: () => settings.open(),
  beforeSend,
  restoreHeader: () => code.restoreHeader(),
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
  noteCard: (note) => tray.noteCard(tray.cardOf(note), () => noteDetail.open(note.id)),
  addNote: async (path, line) => {
    if (await code.openFile(path)) code.openComposer({ start: line - 1, end: line - 1 });
  },
  onError: showToast,
  onClose: () => code.restoreHeader(),
});

const noteDetail = noteDialog($("note-dialog"), {
  find: (id) => {
    const note = state.notes.find((candidate) => candidate.id === id);
    return note ? { note, run: tray.runFor(note) } : null;
  },
  canSend: (note) => tray.canSendAlone() && !isResolved(note),
  agent: () => state.settings?.agent ?? null,
  showInCode: (note) =>
    note.target === "folder" ? files.showFolder(note.path) : code.showNote(note),
  send: (note, options) => tray.sendAlone(note, options),
  remove: deleteNote,
  resolve: (note) => changeNote("resolve", note),
  editNote,
  reopen: (note) => changeNote("reopen", note),
  showRun: () => tray.showTab("run"),
  answerQuestion: (request, content) => runs.answerQuestion(request, content),
  reviewOf: (id) => reviewOf(id, runs.knownViews()),
  reviewers: () => state.settings?.reviewers ?? [],
  startReview: (id) => runs.startReview(id),
  live: () => runs.live(),
});

// A note on a whole file or folder, from the file list.
const pathNote = pathNoteDialog($("path-note-dialog"), {
  add: addPathNote,
  send: (note, options) => tray.sendAlone(note, options),
  agent: () => state.settings?.agent ?? null,
});

const layout = dividers();

const welcome = welcomeScreen({
  state,
  welcomeEl,
  codeEl,
  guardDraft,
  leaveFocus,
  beforeSwitch: () => files.dropPendingSave(),
});

// ---- Notes --------------------------------------------------------------------------------

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
  if (applyNotes(await fetchNotes())) redrawNotes();
}

// Draws again everything that shows the notes, after `state.notes` changed.
function redrawNotes() {
  indexNotes();
  tray.render();
  files.render();
  code.redecorate();
}

// Rebuilds the two lookups derived from `state.notes`. Lost notes are counted but not
// drawn on the code: their lines no longer hold their text. When notes overlap on a line,
// the newest one is shown there.
function indexNotes() {
  state.noteByLine = new Map();
  state.noteCountByPath = new Map();
  for (const note of state.notes) {
    state.noteCountByPath.set(note.path, (state.noteCountByPath.get(note.path) ?? 0) + 1);
    if (!onLines(note) || note.path !== state.file?.path || note.anchor !== "found") continue;
    for (let line = note.lines.start - 1; line < note.lines.end; line += 1) {
      state.noteByLine.set(line, note);
    }
  }
}

// Stores a note on the whole file or folder at `path` (`target` "file" or "folder") and draws
// the notes the reply holds. Resolves to `{ note }`, the note stored, or `{ error }`.
async function addPathNote(path, target, body) {
  // Numbered as a read is: a read sent before the save that answers after it holds the notes
  // without this one, and `applyNotes` drops it instead of drawing it over them.
  const number = ++notesAsked;
  const response = await fetch("api/notes", {
    method: "POST",
    body: JSON.stringify({ path, target, body }),
  });
  if (!response.ok) return { error: await response.text() };
  const { notes } = await response.json();
  if (applyNotes({ ok: true, body: notes, number })) redrawNotes();
  // The Rust process adds a note last (src/notes.rs, `add`).
  return { note: notes.at(-1) };
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
    redrawNotes();
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
  // Not the file list: a new text changes no count, and a rebuild would move the tree's tab
  // stop back to the open file.
  indexNotes();
  tray.render();
  code.redecorate();
  return true;
}

// Resolves or reopens a note (`action` is the route's last part, src/site.rs), and draws
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
  redrawNotes();
}

// ---- Send and run -------------------------------------------------------------------------

// Clears the way for the send step (notes-tray.js, `openSend`). Returns false when it cannot
// open: a run is under way, or a draft note must be saved first.
function beforeSend() {
  // The send step lives in the tray, which focus mode hides.
  leaveFocus();
  if (runs.live()) {
    showToast("The agent is still working on the last run: wait for it, or stop it.");
    return false;
  }
  // The prompt covers the code, and with it a note being written.
  if (guardDraft("sending")) return false;
  if (state.composer) code.closeComposer({ save: false });
  menu.close();
  changes.close();
  return true;
}

// File edits the run has finished, to tell when the open file may have changed.
function editsDone(run) {
  const edits = ["edit", "delete", "move"];
  return foldRun(run.entries).items.filter(
    (item) => item.type === "tool" && item.status === "completed" && edits.includes(item.tool),
  ).length;
}

// Called after every change to the run.
function followRun(run, previous) {
  $("tab-run").hidden = run === null;
  if (run === null) {
    if (state.tab === "run") tray.showTab("notes");
    return;
  }
  const fresh = previous?.id !== run.id;
  // A run that needs the engineer comes to the front. One that starts does not: the send
  // step shows it (`sendNotes`), and a note sent alone from its card is followed on the card.
  if (run.status === "waiting" && previous?.status !== "waiting") {
    // Focus mode hides the tray, where the request is: it gives way.
    leaveFocus();
    tray.showTab("run");
  }
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
  tray.render();
}

// ---- Input --------------------------------------------------------------------------------

window.addEventListener("keydown", (event) => {
  // Cmd-Q, Cmd-W, Cmd-O, Cmd-Shift-O, Cmd-P and Cmd-\ belong to the menu bar (src/menu.rs),
  // which calls `window.postil.menu`. The page leaves them alone, so each shortcut is defined
  // once. A dialog and the welcome screen cover the panes: their keys must not act on what
  // cannot be seen. Each dialog handles its own keys (Escape closes it).
  if (document.querySelector("dialog[open]")) return;
  if (welcome.isShown()) {
    if (event.key === "Escape" && state.projectOpen) welcome.hide();
    return;
  }

  if (event.key === "Enter" && event.metaKey && event.shiftKey) {
    event.preventDefault();
    if (state.send) tray.sendNotes();
    else if (state.projectOpen) tray.openSend();
    return;
  }

  const typing = event.target.matches("input, textarea");
  // `a`, Shift-`a` and `r` answer a permission request while the run shows; focus mode hides it.
  const runKeys = !typing && !state.composer && state.tab === "run" && !inFocus();
  if (runKeys && runs.keydown(event)) return;
  if (event.key === "Escape") {
    const action = escapeAction({
      focusMode: inFocus(),
      composer: Boolean(state.composer),
      send: Boolean(state.send),
      changes: changes.isOpen(),
      selection: Boolean(state.selection),
      live: runs.live(),
    });
    if (action === "leave-focus") leaveFocus();
    else if (action === "close-composer") code.closeComposer({ save: false });
    else if (action === "close-send") tray.closeSend();
    else if (action === "close-changes") changes.close();
    else if (action === "clear-selection") code.clearSelection();
    else if (action === "stop") runs.stop();
  } else if (state.send) {
    // The code is covered by the prompt: no key acts on it.
  } else if (event.key === "c" && event.metaKey && !typing && state.file && state.selection) {
    if (code.copyLines()) event.preventDefault();
  } else if (event.key === "c" && !typing && !event.metaKey && !event.ctrlKey) {
    event.preventDefault();
    // On a row of the file list, a note on that whole file or folder.
    const row = files.focusedRow();
    if (row) pathNote.open(row.path, row.target);
    else code.openComposer();
  }
});

// A draft note lives only in the page, so anything that would end the page (quitting,
// switching projects) first sends the engineer back to it. Returns whether there is one.
function guardDraft(action) {
  // A draft in an open dialog (a note's text being edited, a follow-up) keeps its dialog,
  // which says why.
  if ([noteDetail, pathNote, runs].some((dialog) => dialog.holdDraft(action))) return true;
  if (!state.composer?.text.trim()) return false;
  // The note must be in view to be saved or discarded: a dialog over it would make it inert.
  for (const dialog of document.querySelectorAll("dialog[open]")) dialog.close();
  welcome.hide();
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
  if (welcome.isShown()) {
    await welcome.loadRecent();
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
    path ? code.fetchFile(path) : null,
  ]);
  // The engineer moved on while the replies were in flight: their action wins.
  if (state.composer || state.wanted !== path || generation !== refreshes) return;

  if (repo) showRepo(repo);
  applyNotes(notes);
  if (file) code.showFile(path, file, { keepView: true });
  else indexNotes();
  tray.render();
  files.render();
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

// Whether the last input was a key, for the focus rings of the scrolling panes (app.css,
// `keyboard-input`). `:focus-visible` alone is not enough: the page moves focus to the code
// pane by script, and WebKit then shows the ring whenever the element that had focus showed
// it, which a focused text area always does. Clicking Save in the composer would ring the
// whole code pane. Captured, so a handler that stops the event cannot hide it.
window.addEventListener("keydown", () => document.body.classList.add("keyboard-input"), true);
window.addEventListener(
  "pointerdown",
  () => document.body.classList.remove("keyboard-input"),
  true,
);

// ---- Start --------------------------------------------------------------------------------

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

// What the Rust process calls into the page:
// - `openFolder`, with the folder from the Open Folder dialog, or one dropped on the window;
// - `menu`, with the id of a menu bar command (src/menu.rs, `Command::id`). The page runs
//   them, so they share its guards, such as a draft note blocking Quit.
window.postil = {
  openFolder: (path) => {
    menu.close();
    welcome.requestProject("api/open", path);
  },
  menu: (command) => {
    // A click in the menu bar reaches the page as no event at all, so nothing else closes
    // the right-click menu: it would stay drawn over the welcome screen or the filter.
    menu.close();
    if (command === "open-folder") welcome.chooseFolder();
    else if (command === "switch-project") welcome.show();
    else if (command === "go-to-file") goToFile();
    else if (command === "settings") settings.open();
    else if (command === "focus-mode") toggleFocus();
    else if (command === "close-window" || command === "quit") quit();
  },
  // The run changed (src/run.rs); fetch what is new.
  runChanged: () => runs.sync(),
};

// Focus mode (docs/design.md, "Look and feel"): Cmd-\ hides the file list and the tray, and
// the code column centers at a readable width. A pill in the corner keeps the open notes'
// count and Send, which leaves focus mode for the send step. Nothing to focus on the welcome
// screen.
// The pill shows only in focus mode, by CSS: no code path can leave it behind. Every way out
// goes through `leaveFocus`.
function toggleFocus() {
  if (welcome.isShown()) return;
  if (inFocus()) {
    leaveFocus();
    return;
  }
  // The send step's ticks live in the tray, which focus mode hides: it closes first.
  tray.closeSend();
  movePanes(() => document.body.classList.add("focus-mode"));
  // A control in a hidden pane would keep the keys.
  codeEl.focus();
}

function leaveFocus() {
  if (inFocus()) movePanes(() => document.body.classList.remove("focus-mode"));
}

// Eases the side panes while `change` opens or closes them, and only then (app.css,
// `panes-moving`): a divider dragged by hand follows the pointer at once. The class goes 20 ms
// after the 160 ms easing, which starts at the next style update: a busier main thread only
// makes the panes snap to where they were going. The code pane redraws through its
// ResizeObserver as its width changes.
let panesSettle = null;
function movePanes(change) {
  document.body.classList.add("panes-moving");
  change();
  clearTimeout(panesSettle);
  panesSettle = setTimeout(() => document.body.classList.remove("panes-moving"), 180);
}

function inFocus() {
  return document.body.classList.contains("focus-mode");
}

// Focuses the file filter, as Cmd-P does. Nothing to find on the welcome screen.
function goToFile() {
  if (welcome.isShown()) return;
  // The filter is in the file list, which focus mode hides.
  leaveFocus();
  filterEl.focus();
  filterEl.select();
}

// ---- Settings -----------------------------------------------------------------------------
//
// The Rust process keeps them (src/settings.rs) and sets the window's theme at start; the
// page applies the rest, and sends a changed theme back to the window.

// The theme the window has, so only a change is sent.
let appliedTheme = null;

function applySettings(settings) {
  state.settings = settings;
  const metrics = codeMetrics(settings.codeFontSize);
  const root = document.documentElement.style;
  root.setProperty("--code-font-size", `${metrics.fontSize}px`);
  root.setProperty("--line-height", `${metrics.lineHeight}px`);
  root.setProperty("--tab-width", String(settings.tabWidth));
  code.setLineHeight(metrics.lineHeight);
  // The first settings are the ones the window started with.
  if (appliedTheme !== null && settings.theme !== appliedTheme) {
    window.ipc.postMessage(`theme:${settings.theme}`);
  }
  appliedTheme = settings.theme;
  tray.renderSendBar();
}

const settings = settingsDialog($("settings"), { apply: applySettings });
$("open-settings").addEventListener("click", () => settings.open());

async function start() {
  // The icons load beside the repository, not before it: the tree can draw without them.
  loadIcons();
  // Before the first file is drawn, so it is drawn once at the right size. The defaults in
  // app.css stand when the settings cannot be read; the dialog says why when opened.
  await Promise.all([settings.load().catch(() => {}), layout.load().catch(() => {})]);
  const response = await fetch("api/repo");
  if (!response.ok) {
    $("code-empty").textContent = await response.text();
    return;
  }
  const repo = await response.json();
  if (repo === null) {
    welcome.show();
    return;
  }
  state.projectOpen = true;
  showRepo(repo);
  await files.loadFolders().catch(() => {});
  files.render();
  await loadNotes();
  // The latest run, this session's or one from before a restart.
  await runs.sync();

  if (state.repo.bench) await runBench(state.repo.bench, { code, codeEl });
}

// A promise nobody awaits, such as an event handler's async call, would otherwise fail
// without a word: its error is shown as any other failure is. One listener rather than a
// `.catch` at each call site, so a call added later is covered too (review item JS7).
addEventListener("unhandledrejection", (event) => {
  showToast(String(event.reason?.message ?? event.reason));
});

start();
