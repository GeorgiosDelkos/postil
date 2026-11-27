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
import { diagramView } from "./diagram-view.js";
import { fileMarks } from "./diagram.js";
import {
  getDictationContext,
  isBoxDictating,
  setDictationContext,
  startDictationFor,
  stopActiveDictation,
} from "./dictation.js";
import { dividers } from "./dividers.js";
import { $, el, loadIcons, showToast } from "./dom.js";
import { fileList } from "./file-list.js";
import { rank as rankFiles } from "./fuzzy.js";
import { memoryDialog, rememberDialog } from "./memory-dialog.js";
import { memoryTab } from "./memory-tab.js";
import { noteDialog } from "./note-dialog.js";
import { fullPlace, noteMarks } from "./note-place.js";
import { notesTray } from "./notes-tray.js";
import { pathNoteDialog } from "./path-note-dialog.js";
import { runPanel } from "./run-panel.js";
import { escapeAction, foldRun, isAsk, isOpen, isResolved, sentIn } from "./run.js";
import { settingsDialog } from "./settings-dialog.js";
import { codeMetrics, formatSlot } from "./settings.js";
import { reportOf, reviewOf } from "./status.js";
import { buildTree } from "./tree.js";
import { usageBar } from "./usage-bar.js";
import { usageDialog } from "./usage-dialog.js";
import { command as voiceCommand, normalize as normalizeVoice } from "./voice.js";
import { welcomeScreen } from "./welcome.js";
import { wizardDialog } from "./wizard-dialog.js";

const filesEl = $("files");
const filterEl = $("filter");
const codeEl = $("code");
const notesEl = $("notes");
const welcomeEl = $("welcome");

const state = {
  repo: { name: "", branch: "", files: [], memoryRepo: null },
  // The file shown in the code pane: { path, lines }, where a line is a list of
  // [cssClass, text] tokens.
  file: null,
  // The path most recently asked for, to drop replies that arrive out of order.
  wanted: null,
  // Selected lines as 0-based indexes, both inclusive: { anchor, head }.
  selection: null,
  // The open note composer: { start, end, intent, text, saving, node }, lines 0-based and
  // inclusive; `intent` "ask" when it asks a question instead (code-view.js, `openComposer`).
  // `node` is built once and moved, never rebuilt, when the rows re-render: a rebuilt
  // textarea would lose its focus, caret, and grown height while the engineer types.
  composer: null,
  // As `api/notes` returns them: { id, path, target, lines: { start, end }, snippet, body,
  // anchor, state, intent }, lines 1-based and inclusive, anchor "found" or "lost". `target`
  // is "lines", "file" or "folder", and only a note on lines has `lines` and `snippet`
  // (note-place.js). A lost note is shown as outdated. `intent` is "change", or "ask" for a
  // question, which is never sent with the notes (run.js, `isAsk`). `memory`, when there, is
  // the id of the team memory remembered from the note.
  notes: [],
  // Derived from `notes` by `indexNotes`: the winning note on each 0-based line of the open
  // file, its stripe and chip mark (note-place.js, `noteMarks`), and the count per path.
  noteByLine: new Map(),
  noteMarkByLine: new Map(),
  noteCountByPath: new Map(),
  // Derived from the map's analysis (diagram-view.js, read from `api/analysis` at start, so
  // they show before the map is first opened) by `indexMarks`: the stripe and chip on each
  // 0-based line of the open file (diagram.js, `fileMarks`).
  markByLine: new Map(),
  // Whether the stripes and chips show (View > Map Markers), for this window's session.
  mapMarkers: true,
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
  // Which of the tray's tabs shows: "notes", "run" or "memory".
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
  writeNote: (path, target, options) => pathNote.open(path, target, options),
  analyzeDeeper: (path) => {
    showMap();
    return map.analyzeDeeper(path);
  },
  isAnalyzing: () => map.isAnalyzing(),
});

const code = codeView({
  state,
  codeEl,
  rowsEl: $("code-rows"),
  codeEmptyEl: $("code-empty"),
  menu,
  files,
  indexNotes,
  indexMarks,
  openMapNode: openNodeInMap,
  openNote: (id) => noteDetail.open(id),
  traceFromHere: (path) => {
    showMap();
    map.traceFrom({ of: path, label: path });
  },
  onActiveLine: syncCodeToMap,
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
  // The ask's card shows its run, as a note sent alone does.
  onAsked: () => runs.sync(),
  onFileOpened: () => memory.fileOpened(),
});

const runs = runPanel({
  panelEl: $("run"),
  headerEl: $("run-header"),
  statusEl: $("agent-status"),
  detailEl: $("run-detail"),
  onChange: followRun,
  // An older run was read: the notes sent in it show its status, and the map reads an
  // analysis run that ended before the list of runs saw it live.
  onLoaded: () => {
    tray.render();
    map.runChanged();
  },
  onError: showToast,
  noteOf: (id) => state.notes.find((note) => note.id === id) ?? null,
  openNote: (id) => noteDetail.open(id),
  openFile: (path) => code.openFile(path),
  reviewers: () => (state.settings?.reviewers ?? []).map((slot) => formatSlot(slot)),
  openMemory: (id) => memoryDetail.open(id),
  showChanges: (from, to) => showChangesAt(from, to, null),
  changeMapState: () => map.changeMapState(),
  showChangeMap: (from, to) => {
    showMap();
    map.showChangeMap(from, to);
  },
});

// Shows what runs `from` to `to` changed in the code pane, scrolled to the first hunk
// `match(file, hunk)` holds when given (changes-view.js, `show`).
function showChangesAt(from, to, match) {
  tray.closeSend();
  // A window too narrow for both panes gives this one to the code, where the changes show.
  showCode();
  changes.show(from, to, walkOf(from, to), { match });
}

// What a walkthrough of runs `from` to `to` needs, `{ report, findings }`, or null for a thread
// of several runs: a walkthrough belongs to one run (docs/review-assist.md, "Guided
// walkthrough and risk").
function walkOf(from, to) {
  const run = from === to ? runs.byId(from) : null;
  return (
    run && {
      report: reportOf(run)?.report ?? null,
      findings: reviewOf(run.id, runs.knownViews())?.findings ?? [],
    }
  );
}

const tray = notesTray({
  state,
  notesEl,
  runs,
  menu,
  quotaBadge: (name) => liveQuotas.inlineBadge(name),
  openNote: (id) => noteDetail.open(id),
  drawNoteDetail: () => noteDetail.draw(),
  deleteNote,
  openSettings: () => settings.open(),
  beforeSend,
  restoreHeader: () => code.restoreHeader(),
  onTab: (tab) => (tab === "memory" ? memory.show() : memory.hide()),
});

// Team memory: the tray's Memory tab and a memory's dialog. The node is the map's selected
// node's label, which the map pane (step 8.3) gives; none until then.
const memoryDetail = memoryDialog($("memory-dialog"));
const memory = memoryTab({
  panelEl: $("memory"),
  headerEl: $("memory-header"),
  currentFile: () => (state.file ? { path: state.file.path, node: null } : null),
  openMemory: (id) => memoryDetail.open(id),
  openSettings: () => settings.open(),
});

// The Remember dialog: a team memory drafted from a note, edited and saved by the engineer.
const remember = rememberDialog($("remember-dialog"), {
  memoryRepo: () => state.repo.memoryRepo ?? null,
  openMemory: (id) => memoryDetail.open(id),
  onDraftStarted: () => runs.started(),
  onSaved: (reply) => {
    showToast(
      reply.noteError
        ? `Saved to team memory, but the note was not marked: ${reply.noteError}`
        : "Saved to team memory.",
    );
    memory.remembered();
    void loadNotes();
  },
});

// What a run changed, over the open file; Esc, or opening a file, gives the pane back.
const changes = changesView({
  viewEl: $("changes-view"),
  setHeader: (title, meta, controls) => {
    const close = el("button", "button changes-close", "esc");
    close.type = "button";
    close.title = "Back to the file";
    close.addEventListener("click", () => changes.close());
    $("crumbs").replaceChildren(el("b", "", title), close);
    $("file-meta").replaceChildren(...controls, meta);
  },
  notes: () => state.notes,
  noteCard: (note) => tray.noteCard(tray.cardOf(note), () => noteDetail.open(note.id)),
  addNote: async (path, line) => {
    if (await code.openFile(path)) code.openComposer({ start: line - 1, end: line - 1 });
  },
  onError: showToast,
  onClose: () => code.restoreHeader(),
  openMemory: (id) => memoryDetail.open(id),
});

const noteDetail = noteDialog($("note-dialog"), {
  find: (id) => {
    const note = state.notes.find((candidate) => candidate.id === id);
    return note ? { note, run: tray.runFor(note) } : null;
  },
  canSend: (note) =>
    tray.canSendAlone() && !isResolved(note) && !isAsk(note) && !tray.inLiveRun(note),
  agent: () => state.settings?.agent?.name ?? null,
  showInCode: (note) => {
    if (note.target === "folder") {
      showFiles();
      files.showFolder(note.path);
    } else {
      code.showNote(note);
    }
  },
  send: (note, options) => tray.sendAlone(note, options),
  remove: deleteNote,
  resolve: (note) => changeNote("resolve", note),
  editNote,
  reopen: (note) => changeNote("reopen", note),
  showRun: () => tray.showTab("run"),
  answerQuestion: (id, request, content) => runs.answerQuestion(id, request, content),
  reviewOf: (id) => reviewOf(id, runs.knownViews()),
  reviewers: () => (state.settings?.reviewers ?? []).map((slot) => formatSlot(slot)),
  startReview: (id) => runs.startReview(id),
  busy: (id) => runs.busyThread(id),
  makeNote,
  openRun: (id) => runs.openRun(id),
  remember: (note) =>
    remember.open({ note: note.id, where: `${note.id} · ${fullPlace(note)}`, body: note.body }),
  // The thread a note was sent in starts with that run (src/run.rs, `Runner::thread_tip`).
  threadLive: (note) => {
    const root = sentIn(note);
    return root !== null && runs.threadLive(root);
  },
  openMemory: (id) => memoryDetail.open(id),
});

// A note on a whole file or folder, or a question about one, from the file list.
const pathNote = pathNoteDialog($("path-note-dialog"), {
  add: addPathNote,
  ask: askAboutPath,
  uncommitted: async (path, target) => {
    const query = target === "file" ? `?file=${encodeURIComponent(path)}` : "";
    const response = await fetch(`api/run/uncommitted${query}`);
    return response.ok ? await response.json() : null;
  },
  openSend: () => state.projectOpen && tray.openSend(),
});

// Whether the map shows (View > Map, Cmd-Shift-M), for this window's session, and which of
// the map and the code a window too narrow for both shows (ui/panes.js, `fit`).
const mapMode = { open: false, shows: "map" };

// Whether the file list drawer shows beside the activity bar (View > Files, Cmd-B).
let filesOpen = true;

const layout = dividers({ onNarrow: drawMapMode });

const map = diagramView({
  mapEl: $("map"),
  dialogEl: $("node-dialog"),
  runs,
  menu,
  settings: () => state.settings,
  notes: () => state.notes,
  noteCard: (note) => tray.noteCard(tray.cardOf(note), () => noteDetail.open(note.id)),
  showAnchor: (anchor) => {
    showCode();
    if (anchor.target === "folder") {
      showFiles();
      files.showFolder(anchor.path);
    } else {
      code.showAnchor(anchor);
    }
  },
  writeNote: writeOnAnchor,
  showChangesAt,
  alone: () => mapMode.open && layout.isNarrow(),
  // A new analysis, or a flow opened or closed, changes the stripes, chips and step numbers,
  // and whether a run's dialog offers Change map.
  onChange: () => {
    indexMarks();
    code.redecorate();
    runs.drawDialog();
  },
  onError: showToast,
});

const welcome = welcomeScreen({
  state,
  welcomeEl,
  codeEl,
  guardDraft,
  leaveFocus,
  beforeSwitch: () => {
    liveQuotas.closePopover();
    files.dropPendingSave();
  },
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

// Rebuilds the lookups derived from `state.notes`. Lost notes are counted but not drawn on the
// code: their lines no longer hold their text.
function indexNotes() {
  state.noteCountByPath = new Map();
  for (const note of state.notes) {
    state.noteCountByPath.set(note.path, (state.noteCountByPath.get(note.path) ?? 0) + 1);
  }
  state.noteMarkByLine = noteMarks(state.notes, state.file?.path, code.lineCount());
  state.noteByLine = new Map();
  for (const [line, mark] of state.noteMarkByLine) {
    state.noteByLine.set(line, mark.note);
  }
}

// Rebuilds `markByLine` for the open file from the map's analysis, with the step numbers of
// the flow the map shows.
function indexMarks() {
  const lineCount = code.lineCount();
  const openDiagramId = map.openDiagramId();
  state.markByLine = fileMarks(map.analysis(), state.file?.path, lineCount, { openDiagramId });
}

// Stores a note on the whole file or folder at `path` (`target` "file" or "folder") and draws
// the notes the reply holds. Resolves to null when stored, or the reason it was refused.
async function addPathNote(path, target, body) {
  // Numbered as a read is: a read sent before the save that answers after it holds the notes
  // without this one, and `applyNotes` drops it instead of drawing it over them.
  const number = ++notesAsked;
  const response = await fetch("api/notes", {
    method: "POST",
    body: JSON.stringify({ path, target, body }),
  });
  if (!response.ok) return await response.text();
  const { notes } = await response.json();
  if (applyNotes({ ok: true, body: notes, number })) redrawNotes();
  return null;
}

// Asks `body` about the whole file or folder at `path` (`target` "file" or "folder") of the
// default agent (src/run.rs, `Runner::ask`), and draws the notes the reply holds, the ask
// among them. Resolves to null when asked, or the reason it was refused: nothing is saved then.
async function askAboutPath(path, target, body) {
  const number = ++notesAsked;
  const response = await fetch("api/run/ask", {
    method: "POST",
    body: JSON.stringify({ note: { path, target, body } }),
  });
  if (!response.ok) return await response.text();
  const { notes } = await response.json();
  if (applyNotes({ ok: true, body: notes, number })) redrawNotes();
  await runs.sync();
  return null;
}

// Makes a change note of the ask `ask`, on what it asks about, with its text, from its
// answer's dialog: the ask stays as it is (docs/review-assist.md, "Ask without changing").
// Lines no longer in the file make a note on the whole file. Resolves to null when made, or the
// reason it was refused.
async function makeNote(ask) {
  const onLines = ask.target === "lines" && ask.anchor === "found";
  const note = onLines
    ? { path: ask.path, lines: ask.lines, snippet: ask.snippet, body: ask.body }
    : { path: ask.path, target: ask.target === "folder" ? "folder" : "file", body: ask.body };
  const number = ++notesAsked;
  const response = await fetch("api/notes", { method: "POST", body: JSON.stringify(note) });
  if (!response.ok) return await response.text();
  const { notes } = await response.json();
  if (applyNotes({ ok: true, body: notes, number })) redrawNotes();
  return null;
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
  // Resolving an ask ended its thread (src/run.rs, `Runner::end_ask`): the thread the run
  // dialog holds is read again, or it would offer Discard on a thread already ended.
  if (action === "resolve" && isAsk(note)) {
    runs.forget();
    await runs.sync();
  }
}

// ---- Send and run -------------------------------------------------------------------------

// Clears the way for the send step (notes-tray.js, `openSend`). Returns false when it cannot
// open: as many runs as a repository may have are under way, or a draft note must be saved
// first.
function beforeSend() {
  // The send step lives in the tray, which focus mode hides.
  leaveFocus();
  if (runs.full()) {
    showToast("Three agents are already working: wait for one to end, or stop one.");
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

// Called after every change to the run. `liveEnded` says another live run ended: an analysis,
// or a run whose end frees one of the 3 places a forced update waits for, so the page refreshes
// and checks HEAD again (`map.check`).
function followRun(run, previous, { liveEnded } = {}) {
  // An analysis is never the latest run: the map follows it.
  map.runChanged();
  $("tab-run").hidden = run === null;
  if (run === null) {
    if (state.tab === "run") tray.showTab("notes");
    if (liveEnded) refresh().catch((error) => showToast(String(error)));
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
  const moved =
    !fresh && (previous.status !== run.status || editsDone(previous) !== editsDone(run));
  if (moved || liveEnded) {
    refresh().catch((error) => showToast(String(error)));
    if (run.status === "ended" && previous.status !== run.status) {
      liveQuotas.refresh({ force: true });
    }
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
  // Cmd-[ and Cmd-] go back and forward through the places jumped from and to (code-view.js).
  // Claimed even when they do nothing, so WebKit never takes them for page history.
  if (event.metaKey && (event.key === "[" || event.key === "]")) {
    event.preventDefault();
    if (!typing && !state.send && !changes.isOpen()) code.stepHistory(event.key === "[" ? -1 : 1);
    return;
  }
  // `a`, Shift-`a` and `r` answer a permission request while the run shows; focus mode hides it.
  const runKeys = !typing && !state.composer && state.tab === "run" && !inFocus();
  if (runKeys && runs.keydown(event)) return;
  if (event.key === "Escape") {
    // A jump still waiting for the index is not wanted any more.
    code.cancelJump();
    const action = escapeAction({
      focusMode: inFocus(),
      composer: Boolean(state.composer),
      send: Boolean(state.send),
      changes: changes.isOpen(),
      changeMap: map.showsChangeMap(),
      selection: Boolean(state.selection),
      live: runs.live(),
    });
    if (action === "leave-focus") leaveFocus();
    else if (action === "close-composer") code.closeComposer({ save: false });
    else if (action === "close-send") tray.closeSend();
    else if (action === "close-changes") changes.close();
    else if (action === "leave-change-map") map.leaveChangeMap();
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
  } else if (event.key === "q" && !typing && !event.metaKey && !event.ctrlKey) {
    event.preventDefault();
    // A question about the selected lines, or the file or folder of a row of the file list.
    const row = files.focusedRow();
    if (row) pathNote.open(row.path, row.target, { intent: "ask" });
    else code.askOnSelection();
  }
});

// ---- Hold-to-talk voice commands (docs/review-assist.md, "Voice commands") ----------------
//
// Right Option (`AltRight`), confirmed by the owner on 2026-11-27: starts after held 200 ms
// with no other key, so typing an accented character with Option is never mistaken for a
// command. Inside a text box it dictates there (10.2); anywhere else what is said is matched
// against the fixed grammar in `ui/voice.js`.

const HOLD_TO_TALK_CODE = "AltRight";
const HOLD_DELAY_MS = 200;

let holdTimer = null;
let holdMode = null; // "box" | "command" | null
let voiceUnsub = null;
let voiceSessionId = 0;
let voiceTranscript = "";
let voiceHandled = false;

function currentVoiceContext() {
  const projectOpen = state.projectOpen && !welcome.isShown();
  const dialogOpen = Boolean(document.querySelector("dialog[open]"));
  const latestRun = projectOpen ? runs.run() : null;
  const hasSnapshot = Boolean(
    latestRun?.entries?.some((entry) => entry.kind === "snapshot" && entry.point === "before"),
  );
  const canSend = projectOpen && state.notes.some((note) => isOpen(note) && !isAsk(note));
  const changesOpen = projectOpen && changes.isOpen();
  const hasTarget = projectOpen
    ? changesOpen
      ? Boolean(changes.currentTarget())
      : Boolean(state.selection && !state.composer)
    : false;
  return {
    projectOpen,
    dialogOpen,
    changesOpen,
    hasChanges: Boolean(latestRun && hasSnapshot),
    hasTarget,
    composerOpen: projectOpen && Boolean(state.composer),
    noteDialogOpen: noteDetail.canResolve(),
    sendOpen: projectOpen && Boolean(state.send),
    canSend,
    liveRun: projectOpen && runs.live(),
  };
}

async function runVoiceCommand(transcript) {
  const trimmed = transcript.trim();
  if (!trimmed) return;
  const cmd = voiceCommand(trimmed, currentVoiceContext());
  if (!cmd) {
    showToast(`Not a command: "${trimmed}"`);
    return;
  }
  if (cmd.type === "noop") {
    showToast(cmd.reason);
    return;
  }
  showToast(cmd.label);
  switch (cmd.type) {
    case "next":
      if (!changes.step(1)) showToast("Already at the last step.");
      break;
    case "previous":
      if (!changes.step(-1)) showToast("Already at the first step.");
      break;
    case "note":
    case "ask": {
      const intent = cmd.type === "ask" ? "ask" : "change";
      if (changes.isOpen()) {
        const target = changes.currentTarget();
        if (!target) {
          showToast("Nothing selected.");
          return;
        }
        changes.close();
        if (state.file?.path !== target.path && !(await code.openFile(target.path))) return;
        const line = Math.max(0, target.line - 1);
        code.openComposer({ start: line, end: line }, { intent, text: cmd.text });
      } else {
        code.openComposer(undefined, { intent, text: cmd.text });
      }
      break;
    }
    case "save":
      code.closeComposer({ save: true });
      break;
    case "open": {
      const matches = rankFiles(cmd.query, state.repo.files, 20);
      if (matches.length === 0) {
        showToast(`No file matching "${cmd.query}".`);
      } else if (matches.length === 1) {
        showCode();
        await code.openFile(matches[0].path);
      } else {
        leaveFocus();
        showFiles();
        filterEl.value = cmd.query;
        filterEl.dispatchEvent(new Event("input"));
        filterEl.focus();
      }
      break;
    }
    case "goto":
      changes.close();
      tray.closeSend();
      showCode();
      await code.jumpToName(cmd.name);
      break;
    case "show-map":
      showMap();
      break;
    case "show-code":
      changes.close();
      tray.closeSend();
      showCode();
      break;
    case "show-changes": {
      const run = runs.run();
      if (run) showChangesAt(run.id, run.id, null);
      break;
    }
    case "resolve":
      if (!noteDetail.resolveOpen()) showToast("Nothing to resolve.");
      break;
    case "send":
      if (cmd.confirm) tray.sendNotes();
      else tray.openSend();
      break;
    case "stop":
      runs.stop();
      break;
    default:
      break;
  }
}

function finishVoiceCommandSession() {
  if (voiceUnsub) {
    voiceUnsub();
    voiceUnsub = null;
  }
  if (holdMode === "command") holdMode = null;
  if (!voiceHandled && voiceTranscript.trim()) {
    voiceHandled = true;
    void runVoiceCommand(voiceTranscript);
  }
  voiceTranscript = "";
}

function startVoiceCommandSession() {
  const bridge = window.postil?.dictation;
  if (!bridge) return;
  if (voiceUnsub) {
    // Cleanly cancel any previous stopping session before starting a new one so listeners never leak.
    voiceUnsub();
    voiceUnsub = null;
    bridge.cancel();
  }
  voiceSessionId += 1;
  const session = voiceSessionId;
  voiceTranscript = "";
  voiceHandled = false;
  voiceUnsub = bridge.subscribe((event) => {
    if (session !== voiceSessionId) return;
    if (event.type === "transcript") {
      voiceTranscript = event.text ?? "";
      if (event.isFinal && !voiceHandled) {
        voiceHandled = true;
        const final = voiceTranscript;
        finishVoiceCommandSession();
        void runVoiceCommand(final);
      }
    } else if (event.type === "state") {
      if (event.state === "listening") {
        showToast("Listening…");
      } else if (event.state === "denied" || event.state === "restricted") {
        if (event.message) showToast(event.message);
        finishVoiceCommandSession();
      } else if (event.state === "idle") {
        finishVoiceCommandSession();
      }
    } else if (event.type === "error") {
      if (event.message) showToast(event.message);
      finishVoiceCommandSession();
    }
  });
  const context = getDictationContext();
  bridge.start(context.length > 0 ? { context } : undefined);
}

function endHoldToTalk() {
  if (holdTimer !== null) {
    clearTimeout(holdTimer);
    holdTimer = null;
  }
  if (!holdMode) return;
  const mode = holdMode;
  holdMode = null;
  if (mode === "box") {
    stopActiveDictation();
  } else if (mode === "command") {
    window.postil?.dictation?.stop();
  }
}

window.addEventListener("keydown", (event) => {
  if (event.code === HOLD_TO_TALK_CODE) {
    if (event.repeat || holdTimer !== null || holdMode !== null) return;
    holdTimer = setTimeout(() => {
      holdTimer = null;
      const focused = document.activeElement;
      const inTextInput = Boolean(
        focused?.matches("textarea, input[type='text'], input:not([type]), [contenteditable]"),
      );
      if (inTextInput) {
        const inComposer = Boolean(state.composer?.node?.contains(focused));
        const options = inComposer
          ? {
              onFinal: (text) => {
                if (normalizeVoice(text) !== "save") return false;
                showToast("Save");
                code.closeComposer({ save: true });
                return true;
              },
            }
          : undefined;
        if (startDictationFor(focused, options)) {
          holdMode = "box";
        }
        return;
      }
      if (!isBoxDictating()) {
        holdMode = "command";
        startVoiceCommandSession();
      }
    }, HOLD_DELAY_MS);
    return;
  }
  // Any other key while waiting for the 200 ms hold cancels it (e.g. Option + letter for accents).
  if (holdTimer !== null) {
    clearTimeout(holdTimer);
    holdTimer = null;
  }
});

window.addEventListener("keyup", (event) => {
  if (event.code === HOLD_TO_TALK_CODE) endHoldToTalk();
});

window.addEventListener("blur", () => {
  endHoldToTalk();
});

// A draft note lives only in the page, so anything that would end the page (quitting,
// switching projects) first sends the engineer back to it. Returns whether there is one.
function guardDraft(action) {
  // A draft in an open dialog (a note's text being edited, a follow-up) keeps its dialog,
  // which says why.
  if ([noteDetail, pathNote, runs, remember].some((dialog) => dialog.holdDraft(action))) {
    return true;
  }
  // A flow's question in the map's box: the box must be in view to be asked or cleared. It
  // takes the focus last: `welcome.hide` gives it to the code, which would blur the box.
  if (map.hasDraft()) {
    for (const dialog of document.querySelectorAll("dialog[open]")) dialog.close();
    welcome.hide();
    showMap();
    map.holdDraft(action);
    return true;
  }
  if (!state.composer?.text.trim()) return false;
  // The note must be in view to be saved or discarded: a dialog over it would make it inert.
  for (const dialog of document.querySelectorAll("dialog[open]")) dialog.close();
  welcome.hide();
  const label = state.composer.node.querySelector(".composer-label");
  const draft =
    state.composer.intent === "ask" ? "Ask or discard this question" : "Save or discard this note";
  label.textContent = `${draft} before ${action}.`;
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
// the composer is open: reading the file again would move its lines under the text. The
// analysis is read again either way, and then compared with HEAD, which may start the forced
// update (`map.check`).
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
  if (!state.projectOpen) return;
  // The definitions index looks at the files again on its own thread (src/definitions.rs);
  // nothing here waits for it. Even with the composer open: the index moves no line.
  void fetch("api/definitions/refresh", { method: "POST" });
  // Another window may have analyzed the repository, or the code under a node moved. With the
  // composer open the file is not read again, but the map is.
  if (state.composer) {
    map
      .load()
      .then(() => map.check())
      .catch((error) => showToast(String(error)));
    return;
  }
  const path = state.wanted;
  // A later refresh may answer first, as a run's edits come one after another: only the
  // latest refresh draws, or an older file would be painted over a newer one.
  const generation = ++refreshes;
  const [repo, notes, analysis, file] = await Promise.all([
    fetch("api/repo").then(async (response) => (response.ok ? response.json() : null)),
    fetchNotes(),
    map.fetchAnalysis(),
    path ? code.fetchFile(path) : null,
  ]);
  // Before the check below, and before the file, which indexes the stripes and chips with the
  // notes, so both are drawn on the new text. Even when the engineer moved on: a file opened
  // meanwhile is indexed against the analysis the map holds, and dropping this reply would
  // leave it without its stripes until the next focus. Its number keeps a newer reply's.
  void map
    .applyAnalysis(analysis)
    .then(() => map.check())
    .catch((error) => showToast(String(error)));
  // The engineer moved on while the replies were in flight: their action wins.
  if (state.composer || state.wanted !== path || generation !== refreshes) return;

  const prevFiles = state.repo?.files ?? null;
  const prevCounts = state.noteCountByPath;
  const filesChanged = repo ? !sameStrings(prevFiles, repo.files) : false;
  if (repo) showRepo(repo);
  applyNotes(notes);
  const countsChanged = !sameCounts(prevCounts, state.noteCountByPath);
  if (file) code.showFile(path, file, { keepView: true });
  else indexNotes();
  tray.render();
  if (filesChanged || countsChanged) files.render();
}

function sameStrings(a, b) {
  if (!a || !b || a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) if (a[i] !== b[i]) return false;
  return true;
}

function sameCounts(a, b) {
  if (a.size !== b.size) return false;
  for (const [k, v] of a) if (b.get(k) !== v) return false;
  return true;
}

window.addEventListener("focus", () => {
  // Before `refresh`, whose `forget` drops the threads read: the engineer may have merged a
  // pull request on GitHub while away.
  runs.checkPullRequests();
  liveQuotas.refresh();
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
  const prevFiles = state.repo?.files ?? null;
  state.repo = repo;
  if (!sameStrings(prevFiles, repo.files)) state.tree = buildTree(repo.files);
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
// - `dictation`, the bridge to native on-device speech recognition (`src/dictation.rs`).
setDictationContext(() => {
  const words = new Set();
  if (state.file?.path) {
    words.add(state.file.path);
    const base = state.file.path.split("/").at(-1);
    if (base) words.add(base);
  }
  for (const label of map.nodeLabels()) {
    if (label) words.add(label);
  }
  return [...words];
});
const dictationListeners = new Set();
window.postil = {
  openFolder: (path) => {
    menu.close();
    liveQuotas.closePopover();
    welcome.requestProject("api/open", path);
  },
  menu: (command) => {
    // A click in the menu bar reaches the page as no event at all, so nothing else closes
    // the right-click menu: it would stay drawn over the welcome screen or the filter.
    menu.close();
    liveQuotas.closePopover();
    if (command === "open-folder") welcome.chooseFolder();
    else if (command === "switch-project") welcome.show();
    else if (command === "go-to-file") goToFile();
    else if (command === "settings") {
      wizard.close();
      settings.open();
    } else if (command === "setup-wizard") {
      settings.close();
      wizard.open();
    } else if (command === "usage") usage.open();
    else if (command === "focus-mode") toggleFocus();
    else if (command === "files") toggleFiles();
    else if (command === "map") toggleMap();
    else if (command === "map-markers") toggleMapMarkers();
    else if (command === "close-window" || command === "quit") quit();
  },
  // The run changed (src/run.rs); fetch what is new.
  runChanged: () => runs.sync(),
  dictation: {
    start(options) {
      const payload = options ? JSON.stringify(options) : "";
      window.ipc?.postMessage(payload ? `dictation:start:${payload}` : "dictation:start");
    },
    stop() {
      window.ipc?.postMessage("dictation:stop");
    },
    cancel() {
      window.ipc?.postMessage("dictation:cancel");
    },
    subscribe(listener) {
      dictationListeners.add(listener);
      return () => dictationListeners.delete(listener);
    },
    emit(event) {
      for (const listener of Array.from(dictationListeners)) {
        try {
          listener(event);
        } catch (error) {
          console.error(error);
        }
      }
    },
  },
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
  liveQuotas.closePopover();
  tray.closeSend();
  movePanes(() => document.body.classList.add("focus-mode"));
  drawMapMode();
  // A control in a hidden pane would keep the keys.
  codeEl.focus();
}

function leaveFocus() {
  if (!inFocus()) return;
  movePanes(() => document.body.classList.remove("focus-mode"));
  drawMapMode();
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

// ---- Files drawer -------------------------------------------------------------------------

function setFilesOpen(open, { animate = true } = {}) {
  if (filesOpen === open) return;
  filesOpen = open;
  if (!open) liveQuotas.closePopover();
  const apply = () => {
    document.body.classList.toggle("files-collapsed", !open);
    $("toggle-files").setAttribute("aria-pressed", String(open));
    layout.setFilesOpen(open);
  };
  if (animate) movePanes(apply);
  else apply();
  if (!open && $("sidebar").contains(document.activeElement)) codeEl.focus();
}

function showFiles() {
  leaveFocus();
  setFilesOpen(true, { animate: false });
}

function toggleFiles() {
  if (welcome.isShown()) return;
  const hidden = inFocus();
  leaveFocus();
  if (hidden) setFilesOpen(true);
  else setFilesOpen(!filesOpen);
}

$("toggle-files").addEventListener("click", toggleFiles);

// ---- The map ------------------------------------------------------------------------------

// Cmd-Shift-M: opens the map, or closes it; in a window too narrow for the map beside the code,
// switches between the two, and the sidebar's Map button closes it. Focus mode hides the map,
// so it gives way, and an open map shows again rather than closing unseen.
function toggleMap() {
  if (welcome.isShown()) return;
  const hidden = inFocus();
  // Out of focus mode onto the map, which is what was asked for, in a narrow window too.
  if (hidden) mapMode.shows = "map";
  leaveFocus();
  if (!mapMode.open) setMapOpen(true);
  else if (hidden) map.focus();
  else if (layout.isNarrow()) {
    mapMode.shows = mapMode.shows === "map" ? "code" : "map";
    drawMapMode();
    if (mapMode.shows === "map") map.focus();
    else codeEl.focus();
  } else setMapOpen(false);
}

function setMapOpen(open) {
  mapMode.open = open;
  mapMode.shows = "map";
  // The panes are fitted again for the map, and draw the mode through `drawMapMode`.
  layout.setMapOpen(open);
  if (open) map.focus();
}

$("toggle-map").addEventListener("click", () => {
  if (welcome.isShown()) return;
  leaveFocus();
  setMapOpen(!mapMode.open);
});

// Shows the map beside the code, or, in a narrow window, the one of them `mapMode.shows`.
// Focus mode shows the code alone, whatever the window: the map waits, open, for its end.
function drawMapMode(narrow = layout.isNarrow()) {
  const open = mapMode.open && !inFocus();
  const shown = open && (!narrow || mapMode.shows === "map");
  $("map").hidden = !shown;
  $("code-pane").classList.toggle("map-full", open && narrow && mapMode.shows === "map");
  $("toggle-map").setAttribute("aria-pressed", String(mapMode.open));
  map.setShown(shown);
  syncCodeToMap();
}

// Shows the map: out of focus mode, opened, and in a narrow window in place of the code.
function showMap() {
  leaveFocus();
  if (!mapMode.open) setMapOpen(true);
  else if (layout.isNarrow() && mapMode.shows !== "map") {
    mapMode.shows = "map";
    drawMapMode();
  }
}

// Opens the map at node `nodeId` of diagram `diagramId`, a click on its stripe or chip in the
// code.
function openNodeInMap(diagramId, nodeId) {
  showMap();
  map.revealNode(diagramId, nodeId);
}

// The map follows the code pane's line while it shows.
function syncCodeToMap() {
  if (!map.isShown()) return;
  const active = code.activeLine();
  map.followCode(active?.path ?? null, active?.line ?? null);
}

// View > Map Markers: the stripes and chips go, or come back.
function toggleMapMarkers() {
  state.mapMarkers = !state.mapMarkers;
  code.redecorate();
}

// A narrow window showing the map alone gives the pane to the code, where a node's code shows.
function showCode() {
  if (!mapMode.open || !layout.isNarrow() || mapMode.shows === "code") return;
  mapMode.shows = "code";
  drawMapMode();
}

// Writes a note on the code a map node's reference `anchor` points at, or asks about it
// (`intent` "ask"): on its lines in the composer, or on its whole file or folder.
async function writeOnAnchor(anchor, intent) {
  if (anchor.target !== "lines") {
    pathNote.open(anchor.path, anchor.target, { intent });
    return;
  }
  showCode();
  if (state.file?.path !== anchor.path && !(await code.openFile(anchor.path))) return;
  const range = { start: anchor.lines.start - 1, end: anchor.lines.end - 1 };
  code.openComposer(range, { intent });
}

// Focuses the file filter, as Cmd-P does. Nothing to find on the welcome screen.
function goToFile() {
  if (welcome.isShown()) return;
  // The filter is in the file list, which focus mode or a collapsed drawer hides.
  showFiles();
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
  liveQuotas.setSettings(settings);
  tray.renderSendBar();
  // Another Analyzer may start the forced update a missing one held back.
  if (state.projectOpen) void map.check().catch((error) => showToast(String(error)));
}

const liveQuotas = usageBar({
  stripEl: $("quota-strip"),
  popoverEl: $("quota-popover"),
  warnDotEl: $("usage-warn-dot"),
  openUsage: () => usage.open(),
});

const settings = settingsDialog($("settings"), {
  apply: applySettings,
  openWizard: () => wizard.open(),
  quotaBadge: (name) => liveQuotas.inlineBadge(name),
});
const wizard = wizardDialog($("wizard"), {
  apply: applySettings,
  onSaved: (view) => settings.sync(view),
  openFolder: () => welcome.chooseFolder(),
  canOpenFolder: () => !state.projectOpen,
  openSettings: () => settings.open(),
  quotaBadge: (name) => liveQuotas.inlineBadge(name),
});
$("open-settings").addEventListener("click", () => {
  liveQuotas.closePopover();
  wizard.close();
  settings.open();
});

const usage = usageDialog($("usage"), {
  openSettings: () => {
    $("usage").close();
    settings.open();
  },
  onView: (view) => liveQuotas.syncView(view),
});
$("usage").addEventListener("close", () => liveQuotas.refresh());
$("open-usage").addEventListener("click", () => {
  liveQuotas.closePopover();
  usage.open();
});
$("repo-name").addEventListener("click", () => liveQuotas.closePopover());

async function start() {
  // The icons load beside the repository, not before it: the tree can draw without them.
  loadIcons();
  // Fetch settings, layout, and repo info in parallel to eliminate the startup waterfall.
  const [settingsView, , response] = await Promise.all([
    settings.load().catch(() => null),
    layout.load().catch(() => {}),
    fetch("api/repo"),
  ]);
  if (!response.ok) {
    $("code-empty").textContent = await response.text();
    return;
  }
  const repo = await response.json();
  if (repo === null) {
    welcome.show();
    liveQuotas.start();
    if (settingsView?.firstRun) wizard.open({ firstRun: true, view: settingsView });
    return;
  }
  state.projectOpen = true;
  showRepo(repo);
  // Load open folders, notes, analysis, and latest run in parallel, then render `#files` once.
  const [, notes, analysis] = await Promise.all([
    files.loadFolders().catch(() => {}),
    fetchNotes().catch((error) => {
      notesEl.replaceChildren(el("div", "empty", String(error)));
      return null;
    }),
    map.fetchAnalysis().catch((error) => {
      showToast(String(error));
      return null;
    }),
  ]);
  if (notes) applyNotes(notes);
  files.render();
  tray.render();
  if (analysis) {
    await map.applyAnalysis(analysis).catch((error) => showToast(String(error)));
  }
  // The latest run, this session's or one from before a restart.
  await runs.sync();

  if (state.repo.bench) {
    await runBench(state.repo.bench, { code, codeEl });
    return;
  }
  liveQuotas.start();
  // Commits made while Postil was closed are checked once it opens.
  void map.check().catch((error) => showToast(String(error)));
  if (settingsView?.firstRun) wizard.open({ firstRun: true, view: settingsView });
}

// A promise nobody awaits, such as an event handler's async call, would otherwise fail
// without a word: its error is shown as any other failure is. One listener rather than a
// `.catch` at each call site, so a call added later is covered too (review item JS7).
addEventListener("unhandledrejection", (event) => {
  showToast(String(event.reason?.message ?? event.reason));
});

start();
