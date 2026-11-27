// The map: the analysis's diagrams (`GET api/analysis`, src/analysis.rs) drawn as SVG in the
// pane left of the code, with pan, zoom, search, the keyboard, and the dialog a node opens
// (docs/review-assist.md, "What the engineer sees"). ui/diagram.js has the rules.
//
// The layout library, dagre (ui/vendor/dagre.LICENSE), is loaded by a script element the first
// time the map opens, so a window that never opens it never loads it. Every label, summary and
// detail is an agent's text: it is set as text, and the detail through the Markdown subset
// (dialog-parts.js, `fillMarkdown`), never as HTML.

import { fillMarkdown } from "./dialog-parts.js";
import { attachDictation } from "./dictation.js";
import {
  anchorPlace,
  arrowTarget,
  breadcrumb,
  centreOn,
  checkFlowQuestion,
  childDiagram,
  clampTransform,
  diagramCounts,
  firstFound,
  fitTransform,
  layoutDiagram,
  MAX_UPDATE_FILES,
  nodeAtLine,
  nodeState,
  nodeTouchesHunk,
  notesOn,
  runProblem,
  searchNodes,
  touched,
  zoomAt,
} from "./diagram.js";
import { actOnSecondClick, el } from "./dom.js";
import { parsePatch } from "./patch.js";
import { isLive } from "./run.js";
import { formatSlot } from "./settings.js";

const SVG = "http://www.w3.org/2000/svg";

// The served path of the layout library (src/site.rs, `PAGE`).
const DAGRE_SRC = "vendor/dagre.min.js";

// A click's pan to a node, in ms: as the side panes move (docs/design.md, "Motion").
const GLIDE_MS = 160;

// How much one notch of a pinch zooms: e^(-deltaY / 100). Arbitrary, read by feel on a
// trackpad.
const PINCH_RATE = 0.01;

// The most changed files not on the map that the change map lists; it counts them all. As
// many as the repository's map lists (src/analysis.rs, `MAX_UNMAPPED_SHOWN`).
const CHANGE_UNMAPPED_SHOWN = 20;

// The kinds a diagram is, as the switcher names them.
const KINDS = [
  ["architecture", "Architecture"],
  ["user_flow", "User Flow"],
  ["flow", "Traces"],
];

// `mapEl` is the map's pane and `dialogEl` the node's `<dialog>`. `runs` is the run panel
// (run-panel.js), which follows the analysis runs, and `menu` the context menu
// (context-menu.js), which a right-click on a node opens. The callbacks:
// - `settings()` gives the settings in effect, for the Analyzer's name;
// - `notes()` gives the notes, for those on a node's code;
// - `noteCard(note)` builds a note's card, as the tray draws it;
// - `showAnchor(anchor)` shows a code reference in the code pane, and a file not on the map as
//   a reference to the whole file;
// - `writeNote(anchor, intent)` writes a note ("change") or asks a question ("ask") on it;
// - `showChangesAt(from, to, match)` shows what runs `from` to `to` changed, scrolled to the
//   first hunk `match(file, hunk)` holds (changes-view.js, `show`);
// - `alone()` says whether the map takes the whole code pane (a narrow window): a click then
//   opens the node's dialog, whose Show in code gives the pane to the code;
// - `onChange()` follows a new analysis, another diagram opened, and the map shown or hidden,
//   which the stripes and chips in the code show (code-view.js, `decorateRow`);
// - `onError(text)` shows a failure.
export function diagramView({
  mapEl,
  dialogEl,
  runs,
  menu,
  settings,
  notes,
  noteCard,
  showAnchor,
  writeNote,
  showChangesAt,
  alone,
  onChange,
  onError,
}) {
  const view = {
    shown: false,
    // `GET api/analysis`: "loading" before the first read, "none" when there is no
    // analysis, "ready" with `data`, or "error" with the reason the file cannot be used.
    analysis: { status: "loading", data: null, error: null },
    // The layout library: "idle", "loading", "ready", or "failed" until Retry.
    dagre: window.dagre ? "ready" : "idle",
    kind: "architecture",
    diagramId: null,
    selected: null,
    search: "",
    // Pan and zoom per diagram id, for the session.
    transforms: new Map(),
    // The analysis run this page started, followed until it ends, and the last one whose end
    // was read, so its analysis is read once.
    runId: null,
    settled: null,
    // What started that run, `{ route, request }`, for Retry.
    retry: null,
    // What the last check of HEAD found that the strip says (`check`): null, `{ state:
    // "diverged" }`, or `{ state: "blocked", reason }`. And the forced update the page was told
    // of, `{ run, commit }` with `commit` the short id it started at, or null: kept apart, so a
    // later check that finds nothing to do leaves "Updating after" on the strip while it runs.
    check: null,
    forced: null,
    // Uncommitted changes, for the empty state (`GET api/run/uncommitted`).
    uncommitted: null,
    // The code pane's line the map follows, `{ path, line }` with `line` 1-based, or null, and
    // the node of the drawn diagram that stands for it, ringed (`followCode`).
    code: null,
    codeNode: null,
    // The Flows list's box (`renderFlows`): where the flow asked for starts, `{ of, label }`
    // with `of` a node id or a path as `POST api/analysis` takes it, or null; why the page
    // refused the question, or null; and whether `focus` gives the box the focus, until it
    // loses it.
    flowOf: null,
    flowProblem: null,
    focusFlow: false,
    // A run's change map (`showChangeMap`), drawn in place of the repository's map: null, or
    // `{ status, from, to, error, analysis, touched }`, `status` "loading", "ready" or "error",
    // `analysis` placed at the run's fork (`GET api/analysis?at=`), and `touched` what the run's
    // diff touches there (diagram.js, `touched`). `analysis` above stays the checkout's, which
    // the code's stripes and chips show, and which the code pane's lines are compared with.
    change: null,
  };
  // Every change map asked for is numbered, and a reply to an older one, or to one left since,
  // is dropped.
  let changeAsked = 0;
  // A node to centre once its diagram is drawn: `revealNode` asked for it while the layout
  // library or the analysis was still loading.
  let pendingCentre = null;
  // Every read of the analysis is numbered as it is asked for, and one older than a reply
  // already applied is dropped, as the notes are (app.js, `fetchNotes`).
  let analysisAsked = 0;
  let analysisShown = 0;
  // Every check of HEAD is numbered too, and a reply older than one applied is dropped.
  let checkAsked = 0;
  let checkShown = 0;
  // Layouts by diagram id and run, for the session (diagram.js, `layoutDiagram`).
  const layouts = new Map();
  // The drawn diagram: its layout, the SVG's viewport group, and the node elements by id.
  let drawn = null;
  let glide = null;
  let loading = null;

  const header = el("header", "pane-header map-header");
  const tools = el("div", "map-tools");
  const flows = el("section", "map-flows");
  const unmapped = el("details", "map-unmapped");
  const strip = el("div", "map-run");
  const body = el("div", "map-body");
  body.tabIndex = -1;
  mapEl.replaceChildren(header, tools, flows, unmapped, strip, body);

  // The Flows list (`renderFlows`): the box that asks for a flow, and a row per flow. Built once
  // and changed in place, so a redraw while a run streams keeps what is typed and the caret,
  // and a row is built again only when the flows change, which keeps a Delete its first click
  // armed.
  const flowForm = el("form", "map-flow-form");
  const flowFrom = el("span", "map-flow-chip");
  const flowFromLabel = el("span", "map-flow-chip-label");
  const flowFromClear = el("button", "map-flow-chip-clear", "×");
  flowFromClear.type = "button";
  flowFromClear.setAttribute("aria-label", "Trace from anywhere");
  flowFrom.append(flowFromLabel, flowFromClear);
  const flowInput = el("input", "field map-flow-question");
  flowInput.type = "text";
  flowInput.placeholder = "Trace: how is a note sent?";
  flowInput.setAttribute("aria-label", "A behavior to trace");
  const flowTrace = el("button", "button map-flow-trace", "Trace");
  flowTrace.type = "submit";
  const flowDictation = attachDictation(flowInput);
  flowForm.append(flowFrom, flowInput, flowDictation.button, flowTrace);
  const flowProblem = el("p", "map-flow-problem");
  const flowList = el("div", "map-flow-list");
  flows.append(flowForm, flowProblem, flowList);
  let flowRows = null;
  let unmappedShown = null;

  // ---- Loading -----------------------------------------------------------------------------

  // Loads the layout library once. A failed load removes its script element, so Retry adds a
  // fresh one: WebKit does not run an element whose load failed again.
  function ensureDagre() {
    if (window.dagre) {
      view.dagre = "ready";
      return Promise.resolve();
    }
    if (loading) return loading;
    view.dagre = "loading";
    loading = new Promise((resolve) => {
      const script = document.createElement("script");
      script.src = DAGRE_SRC;
      const done = () => {
        view.dagre = window.dagre ? "ready" : "failed";
        if (!window.dagre) script.remove();
        loading = null;
        resolve();
        render();
      };
      script.addEventListener("load", done);
      script.addEventListener("error", done);
      document.head.append(script);
    });
    render();
    return loading;
  }

  // Reads the analysis again and draws it. The selection stays while its node is still there.
  async function load() {
    await applyAnalysis(await fetchAnalysis());
  }

  // The reply to `api/analysis`, numbered: `{ number, analysis }`, `analysis` as `view.analysis`
  // holds it.
  async function fetchAnalysis() {
    return readAnalysis(fetch("api/analysis"));
  }

  // The reply `asked` (a fetch whose body is the analysis, as `GET api/analysis` gives it),
  // numbered when it was asked for, as `fetchAnalysis` returns it.
  async function readAnalysis(asked) {
    const number = ++analysisAsked;
    const response = await asked;
    if (!response.ok) {
      return { number, analysis: { status: "error", data: null, error: await response.text() } };
    }
    const data = await response.json();
    return { number, analysis: { status: data ? "ready" : "none", data, error: null } };
  }

  // Takes a reply of `fetchAnalysis`, for the code's stripes and chips even while the map is
  // hidden, and draws it when the map shows.
  async function applyAnalysis(reply) {
    if (reply.number < analysisShown) return;
    analysisShown = reply.number;
    view.analysis = reply.analysis;
    onChange();
    if (!view.shown) return;
    if (view.analysis.status !== "ready") await loadUncommitted();
    render();
  }

  // Compares the analysis with HEAD (`POST api/analysis/check`, src/analysis/check.rs), which
  // starts the forced update when commits broke the Architecture or added a file no node
  // covers: on start, on each refresh, when a run ends, and when the settings change. Only
  // with an analysis that reads.
  async function check() {
    if (view.analysis.status !== "ready") {
      view.check = null;
      return;
    }
    const number = ++checkAsked;
    const response = await fetch("api/analysis/check", { method: "POST" });
    if (!response.ok) {
      const problem = await response.text();
      // An analysis that cannot be read says so in the pane already.
      if (view.analysis.status !== "error") onError(problem);
      return;
    }
    const reply = await response.json();
    // A run that started is followed whatever the order of the replies: no later reply names it.
    if (reply.state === "updated") {
      checkShown = Math.max(checkShown, number);
      view.forced = { run: reply.run, commit: reply.commit };
      view.runId = reply.run;
      view.retry = { route: "api/analysis/update", request: {} };
      view.check = null;
      await runs.track();
      render();
      return;
    }
    if (number < checkShown) return;
    checkShown = number;
    view.check = reply.state === "diverged" || reply.state === "blocked" ? reply : null;
    render();
  }

  async function loadUncommitted() {
    const response = await fetch("api/run/uncommitted").catch(() => null);
    view.uncommitted = response?.ok ? await response.json() : null;
  }

  // ---- Choosing the diagram ----------------------------------------------------------------

  // The analysis drawn: a change map's, else the checkout's.
  function shownAnalysis() {
    return view.change ? view.change.analysis : view.analysis.data;
  }

  function diagrams() {
    return shownAnalysis()?.diagrams ?? [];
  }

  // The diagrams the switcher's `kind` shows at the top: those of that kind with no parent.
  function topOf(kind) {
    return diagrams().filter((diagram) => diagram.kind === kind && !diagram.parent);
  }

  // The diagram to draw: the one chosen while it is still there, whose kind the switcher then
  // shows, else the first at the top of the chosen kind, else none: that kind's empty card,
  // which offers to make one.
  // A change map draws Architecture diagrams only (diagram.js, `touched`).
  function current() {
    const allowed = (diagram) => !view.change || diagram.kind === "architecture";
    const chosen = diagrams().find((diagram) => diagram.id === view.diagramId && allowed(diagram));
    if (chosen) {
      view.kind = chosen.kind;
      return chosen;
    }
    const shown = topOf(view.kind)[0] ?? null;
    view.diagramId = shown?.id ?? null;
    return shown;
  }

  // The switcher's `kind`, at its first diagram at the top: of the repository's map, which a
  // change map gives way to.
  function showKind(kind) {
    leaveChange();
    view.kind = kind;
    if (kind !== "flow") view.focusFlow = false;
    openDiagram(topOf(kind)[0]?.id ?? null);
  }

  function openDiagram(id) {
    view.diagramId = id;
    view.selected = null;
    // A node `revealNode` asked for is not in the diagram chosen instead.
    pendingCentre = null;
    render();
    onChange();
  }

  // ---- Drawing -----------------------------------------------------------------------------

  function render() {
    if (!view.shown) return;
    const ready = (view.change ?? view.analysis).status === "ready";
    const diagram = ready ? current() : null;
    renderHeader(diagram);
    renderTools(diagram);
    renderFlows();
    renderUnmapped(diagram);
    renderStrip();
    if (view.change) {
      const { status, error } = view.change;
      if (status === "loading")
        return showCard(el("div", "map-card", "Reading the run's changes…"));
      if (status === "error") return showCard(el("div", "map-card map-error", error));
      if (!diagram) return showCard(el("div", "map-card", "The map has no Architecture diagram."));
    } else if (view.analysis.status === "loading") {
      return showCard(el("div", "map-card", "Loading…"));
    } else if (view.analysis.status === "error") return showCard(errorCard());
    else if (!diagram) return showCard(view.kind === "flow" ? noFlowCard() : emptyCard());
    if (view.dagre === "failed") return showCard(dagreCard());
    if (view.dagre !== "ready") return showCard(el("div", "map-card", "Loading the map…"));
    drawDiagram(diagram);
  }

  function renderHeader(diagram) {
    const crumbs = el("div", "crumbs map-crumbs");
    // The way back is the header's first item, outside the crumbs: they clip what overflows
    // them, and a long trail in a narrow map would hide a button among them.
    const first = [];
    if (view.change) {
      const { from, to } = view.change;
      const run = from === to ? from : `${from} to ${to}`;
      const back = el("button", "button map-change-exit", "esc");
      back.type = "button";
      back.title = "Back to the repository's map";
      back.addEventListener("click", leaveChangeMap);
      first.push(back);
      crumbs.append(el("b", "map-change-label", `Change map · ${run}`));
      if (diagram) crumbs.append(el("span", "map-crumb-sep", "›"));
    }
    if (diagram) {
      const trail = breadcrumb(shownAnalysis(), diagram);
      trail.forEach((step, index) => {
        if (index > 0) crumbs.append(el("span", "map-crumb-sep", "›"));
        if (index === trail.length - 1) crumbs.append(el("b", "", step.title));
        else {
          const up = el("button", "map-crumb", step.title);
          up.type = "button";
          up.addEventListener("click", () => openDiagram(step.id));
          crumbs.append(up);
        }
      });
    } else if (!view.change) crumbs.append(el("b", "", "Map"));
    const meta = el("div", "meta map-counts");
    if (diagram) {
      const counts = diagramCounts(diagram);
      const words = [`${counts.total} node${counts.total === 1 ? "" : "s"}`];
      if (counts.unverified) words.push(`${counts.unverified} unverified`);
      if (counts.outdated) words.push(`${counts.outdated} outdated`);
      if (view.change) {
        const marked = diagram.nodes.filter((node) => view.change.touched.nodes.has(node.id));
        words.push(`${marked.length} touched`);
      }
      meta.textContent = words.join(" · ");
      if (counts.dropped) {
        meta.title = counts.dropped;
        meta.append(el("span", "map-dropped", " · some left out"));
      }
    }
    header.replaceChildren(...first, crumbs, meta);
  }

  // Whether the analysis can be added to: there is none yet, or one that reads.
  function usable() {
    return view.analysis.status === "none" || view.analysis.status === "ready";
  }

  // The switcher, whose every kind can be chosen, with nothing of it yet too: a flow needs no
  // Architecture, and an empty kind offers to make one. Then, with a diagram drawn, the node
  // search and Update.
  function renderTools(diagram) {
    tools.hidden = !view.change && !usable();
    if (tools.hidden) return;
    const kinds = el("div", "map-kinds");
    kinds.setAttribute("role", "group");
    kinds.setAttribute("aria-label", "Diagram");
    for (const [kind, name] of KINDS) {
      const button = el("button", "map-kind", name);
      button.type = "button";
      // A change map is none of the repository's kinds: each goes back to that map.
      button.setAttribute("aria-pressed", String(!view.change && view.kind === kind));
      button.addEventListener("click", () => showKind(kind));
      kinds.append(button);
    }
    const parts = [kinds];
    if (!diagram) {
      tools.replaceChildren(...parts);
      return;
    }
    // Analyze deeper of a path no node covers adds an Architecture at the top: there may be
    // several. The Flows list chooses among flows.
    const tops = topOf(view.kind);
    if (view.kind !== "flow" && tops.length > 1) parts.push(topPicker(tops, diagram));

    const search = el("input", "field map-search");
    search.type = "search";
    search.placeholder = "Find node  /";
    search.setAttribute("aria-label", "Find a node");
    search.value = view.search;
    search.addEventListener("input", () => {
      view.search = search.value;
      syncRings();
    });
    search.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        const [first] = searchNodes(diagram, view.search);
        if (first) select(first, { show: false });
      } else if (event.key === "Escape" && search.value) {
        // Clears the search; an empty search lets Escape act on the page as elsewhere.
        event.stopPropagation();
        search.value = "";
        view.search = "";
        syncRings();
      }
    });
    parts.push(search);

    // Update changes the repository's map, which a change map is not. While the strip offers
    // the Update of a checkout on another history, it is the one Update.
    const stale = diagramCounts(diagram).outdated > 0 || unmappedOf(diagram);
    if (!view.change && stale && !divergedShown()) {
      const update = el("button", "button map-update", "Update");
      update.type = "button";
      update.title =
        "Asks the Analyzer again for the outdated nodes of this diagram, and to place the files not on it";
      update.disabled = Boolean(liveRun());
      update.addEventListener("click", () =>
        startRun("api/analysis/update", { diagram: diagram.id }),
      );
      parts.push(update);
    }
    tools.replaceChildren(...parts);
  }

  // A choice among the diagrams `tops` at the top of the shown kind, the one `diagram` is in
  // selected.
  function topPicker(tops, diagram) {
    const top = breadcrumb(shownAnalysis(), diagram)[0].id;
    const picker = el("select", "field map-top");
    picker.setAttribute("aria-label", "Diagram at the top");
    for (const each of tops) {
      const option = el("option", "", each.title);
      option.value = each.id;
      option.selected = each.id === top;
      picker.append(option);
    }
    picker.addEventListener("change", () => openDiagram(picker.value));
    return picker;
  }

  // ---- Flows -------------------------------------------------------------------------------

  // The Flows list, on Flows: the box that asks for a flow, from where Trace from here said,
  // and a row per flow, which opens it, with a Delete that takes two clicks, as a note's does.
  function renderFlows() {
    flows.hidden = !usable() || view.kind !== "flow";
    if (flows.hidden) return;
    flowFrom.hidden = !view.flowOf;
    flowFromLabel.textContent = view.flowOf ? `From ${view.flowOf.label}` : "";
    flowFromLabel.title = flowFromLabel.textContent;
    flowTrace.disabled = Boolean(liveRun());
    flowProblem.hidden = !view.flowProblem;
    flowProblem.textContent = view.flowProblem ?? "";

    const list = topOf("flow");
    const key = JSON.stringify([list.map((flow) => [flow.id, flow.title]), view.diagramId]);
    if (key === flowRows) return;
    flowRows = key;
    flowList.replaceChildren(...list.map(flowRow));
  }

  function flowRow(flow) {
    const row = el("div", "map-flow-item");
    const open = el("button", "map-flow-open", flow.title);
    open.type = "button";
    if (flow.id === view.diagramId) open.setAttribute("aria-current", "true");
    open.addEventListener("click", () => openDiagram(flow.id));
    const remove = el("button", "map-flow-delete", "×");
    remove.type = "button";
    remove.setAttribute("aria-label", `Delete the trace ${flow.title}`);
    actOnSecondClick(remove, {
      area: row,
      arm: () => {
        row.classList.add("confirming");
        remove.textContent = "Delete";
      },
      disarm: () => {
        row.classList.remove("confirming");
        remove.textContent = "×";
      },
      act: () => deleteDiagram(flow.id).catch((error) => onError(String(error))),
    });
    row.append(open, remove);
    return row;
  }

  flowForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    view.flowProblem = checkFlowQuestion(flowInput.value);
    if (view.flowProblem) {
      renderFlows();
      return;
    }
    const request = { kind: "flow", question: flowInput.value.trim() };
    if (view.flowOf) request.of = view.flowOf.of;
    if (!(await startRun("api/analysis", request))) return;
    flowInput.value = "";
    view.flowOf = null;
    renderFlows();
  });
  flowInput.addEventListener("input", () => {
    if (!view.flowProblem) return;
    view.flowProblem = null;
    renderFlows();
  });
  flowFromClear.addEventListener("click", () => {
    view.flowOf = null;
    renderFlows();
    flowInput.focus();
  });
  flowInput.addEventListener("blur", () => {
    view.focusFlow = false;
  });

  // Deletes diagram `id`, with the diagrams below its nodes, and draws the analysis left,
  // which the reply holds as a read would.
  async function deleteDiagram(id) {
    const route = `api/analysis?diagram=${encodeURIComponent(id)}`;
    const reply = await readAnalysis(fetch(route, { method: "DELETE" }));
    if (reply.analysis.status === "error") {
      onError(reply.analysis.error);
      return;
    }
    await applyAnalysis(reply);
  }

  // Opens the Flows list's box, the flow it asks for starting at `from` (`{ of, label }`), and
  // gives it the focus: Trace from here, on a node or in the code.
  function traceFrom(from) {
    view.flowOf = from;
    view.flowProblem = null;
    openFlowBox();
  }

  function openFlowBox() {
    view.focusFlow = true;
    if (view.kind === "flow") render();
    else showKind("flow");
    focus();
  }

  // Whether the box holds a question not asked yet. A box that cannot show (the analysis cannot
  // be read) holds none.
  function hasDraft() {
    return usable() && Boolean(flowInput.value.trim());
  }

  // Whether the box holds a question not asked yet, which `action` would lose: the box then
  // says so and takes the focus, as a note's draft holds the page (app.js, `guardDraft`).
  function holdDraft(action) {
    if (!hasDraft()) return false;
    view.flowProblem = `Trace or clear this question before ${action}.`;
    openFlowBox();
    return true;
  }

  // The map takes the focus: the Flows list's box while it is asked for, until it loses the
  // focus, else the selected node, else the diagram.
  function focus() {
    if (view.focusFlow && !flows.hidden) flowInput.focus();
    else (drawn?.cards.get(view.selected) ?? body).focus({ preventScroll: true });
  }

  // ---- Not on the map ----------------------------------------------------------------------

  // The files not on `diagram`, a top-level Architecture (src/analysis.rs, `view`):
  // `{ files, total }`, the first 20 and their count, or null when there are none. On a change
  // map, the files the run changed that no Architecture node covers, whichever is drawn, with
  // `change` true.
  function unmappedOf(diagram) {
    if (view.change) {
      const all = view.change.touched.unmapped;
      if (!all.length) return null;
      return { files: all.slice(0, CHANGE_UNMAPPED_SHOWN), total: all.length, change: true };
    }
    return view.analysis.data?.unmapped?.[diagram.id] ?? null;
  }

  // "Not on the map": the files added since the diagram's run that no node covers, each a
  // button to the file, and how many more there are. Built again only when they change, so
  // it stays open or closed as the engineer left it.
  function renderUnmapped(diagram) {
    const off = diagram && unmappedOf(diagram);
    unmapped.hidden = !off;
    // A change map's buttons show its run's changes: another run's, of the same files, are
    // built again.
    const key = off && JSON.stringify([off, view.change?.from, view.change?.to]);
    if (!off || key === unmappedShown) return;
    unmappedShown = key;
    const summary = el("summary", "map-unmapped-title", `Not on the map · ${off.total}`);
    summary.title = off.change
      ? "Files the run changed that no node covers"
      : "Files added since this diagram's analysis that no node covers";
    const list = el("div", "map-unmapped-list");
    for (const path of off.files) {
      const file = el("button", "map-unmapped-file", path);
      file.type = "button";
      // A changed file shows in the run's changes: one the run added may not be in the
      // checkout until the thread is applied.
      const { from, to } = view.change ?? {};
      const show = off.change
        ? () => showChangesAt(from, to, (changed) => changed.path === path)
        : () => showAnchor({ path, target: "file", state: "found" });
      file.addEventListener("click", show);
      list.append(file);
    }
    const parts = [summary, list];
    const more = off.total - off.files.length;
    if (more > 0) parts.push(el("p", "map-unmapped-more", `and ${more} more`));
    if (!off.change && off.total > MAX_UPDATE_FILES) {
      parts.push(el("p", "map-unmapped-note", `Update sends at most ${MAX_UPDATE_FILES} paths.`));
    }
    unmapped.replaceChildren(...parts);
  }

  function showCard(card) {
    // A glide under way pans the drawn diagram, which this card replaces.
    stopGlide();
    drawn = null;
    body.replaceChildren(card);
  }

  // The analysis file cannot be used: the server's reason, and Start over, which moves the
  // file to the Trash, so the next analysis starts from nothing (docs/review-assist-plan.md,
  // 8.1, "Edge cases"). Refused while an analysis runs here, whose merge would write anew.
  function errorCard() {
    const card = el("div", "map-card map-error");
    card.append(el("p", "", view.analysis.error));
    const over = el("button", "button map-start-over", "Start over");
    over.type = "button";
    over.title = "Moves the analysis file to the Trash, where you can get it back";
    over.disabled = Boolean(liveRun());
    over.addEventListener("click", startOver);
    card.append(over);
    return card;
  }

  async function startOver() {
    const response = await fetch("api/analysis/start-over", { method: "POST" });
    if (!response.ok) {
      onError(await response.text());
      return;
    }
    await load();
  }

  // No analysis yet: who would make one, what it reads, and the one button that starts it.
  // With other diagrams there, the kind shown has none yet: Analyze draws it, and the
  // Architecture's top level again, in place of the one there is.
  function emptyGlyph() {
    const svg = svgEl("svg", "map-empty-glyph", { viewBox: "0 0 24 24", "aria-hidden": "true" });
    svg.append(
      svgEl("rect", "", { x: "3", y: "3.5", width: "7", height: "5.5", rx: "1.5" }),
      svgEl("rect", "", { x: "14", y: "3.5", width: "7", height: "5.5", rx: "1.5" }),
      svgEl("rect", "", { x: "8.5", y: "15", width: "7", height: "5.5", rx: "1.5" }),
      svgEl("path", "", {
        d: "M10 6.25h4M6.5 9v2.25c0 1 .8 1.75 1.75 1.75h3.75V15M17.5 9v2.25c0 1-.8 1.75-1.75 1.75H12",
      }),
    );
    return svg;
  }

  function emptyCard() {
    const card = el("div", "map-card map-empty");
    const slot = settings()?.analyzer ?? settings()?.agent;
    const analyzer = slot ? formatSlot(slot) : "the agent";
    card.append(emptyGlyph());
    if (diagrams().length) {
      const name = KINDS.find(([kind]) => kind === view.kind)[1];
      card.append(
        el("h2", "map-empty-title", `No ${name} diagram yet`),
        el(
          "p",
          "",
          `Analyze has ${analyzer} read the last commit (HEAD), read-only, and draw the high-level Architecture and User Flow diagrams again, in place of those there are.`,
        ),
      );
    } else {
      card.append(
        el("h2", "map-empty-title", "No map of this repository yet"),
        el(
          "p",
          "",
          `${analyzer} reads the last commit (HEAD), read-only, and draws its systems architecture and end-to-end user flow.`,
        ),
      );
    }
    const count = view.uncommitted?.uncommitted;
    if (count) {
      const changes = count === 1 ? "1 uncommitted change" : `${count} uncommitted changes`;
      card.append(el("p", "map-empty-note", `It does not see your ${changes}.`));
    }
    const analyze = el("button", "button primary map-analyze", "Analyze");
    analyze.type = "button";
    analyze.disabled = Boolean(liveRun());
    analyze.addEventListener("click", () => startRun("api/analysis", { kind: "analyze" }));
    card.append(analyze);
    return card;
  }

  // Traces, with none yet: the box above asks for one.
  function noFlowCard() {
    const card = el("div", "map-card map-empty");
    card.append(
      emptyGlyph(),
      el("h2", "map-empty-title", "No trace yet"),
      el(
        "p",
        "",
        "Ask above how something happens, or choose Trace from here on a node or in the code. The Analyzer traces it through the last commit, read-only, as ordered steps.",
      ),
    );
    return card;
  }

  function dagreCard() {
    const card = el("div", "map-card map-error");
    card.append(el("p", "", "The map's layout library did not load."));
    const retry = el("button", "button map-retry", "Retry");
    retry.type = "button";
    retry.addEventListener("click", () => ensureDagre());
    card.append(retry);
    return card;
  }

  // ---- The analysis run --------------------------------------------------------------------

  // The analysis run under way in this window: the one this page started, or one found among
  // the live runs after a reload.
  function liveRun() {
    const tracked = view.runId && runs.byId(view.runId);
    if (tracked && isLive(tracked)) return tracked;
    return runs.liveRuns().find((run) => run.analysis) ?? null;
  }

  // Whether the strip offers the Update of a checkout on another history: the last check found
  // `checked` gone or no ancestor of HEAD, no change map is shown, no analysis runs, and there
  // is an Architecture.
  function divergedShown() {
    return (
      !view.change &&
      !liveRun() &&
      view.check?.state === "diverged" &&
      diagrams().some((diagram) => diagram.kind === "architecture")
    );
  }

  // Starts an analysis run as `request` asks at `route`, and resolves to whether it started.
  // An Update with nothing to send on another history starts none: it moved `checked` (src/
  // run.rs, `Runner::analyze`), and the analysis is read again.
  async function startRun(route, request) {
    const response = await fetch(route, { method: "POST", body: JSON.stringify(request) });
    if (!response.ok) {
      onError(await response.text());
      return false;
    }
    const { run } = await response.json();
    // A check sent before the start answers for the commit before: its reply is dropped.
    checkShown = ++checkAsked;
    view.check = null;
    view.forced = null;
    if (run === null) {
      await load();
      render();
      return true;
    }
    view.runId = run;
    view.retry = { route, request };
    await runs.track();
    render();
    return true;
  }

  // Analyze deeper on `of`, a node id or a path.
  function analyzeDeeper(of) {
    return startRun("api/analysis", { kind: "deeper", of });
  }

  // The strip under the tools: the run under way, with Open run and Stop; the checkout on
  // another history, with Update; why the forced update could not start; or why the last run
  // stored nothing, with Open run and Retry.
  function renderStrip() {
    const live = liveRun();
    const ended = !live && view.runId ? runs.byId(view.runId) : null;
    const parts = [];
    if (live) {
      const status =
        view.forced?.run === live.id
          ? `Updating after ${view.forced.commit}…`
          : `Analyzing with ${live.agent}…`;
      parts.push(el("span", "map-run-dot"));
      parts.push(el("span", "map-run-status", status));
      parts.push(stripButton("Open run", () => runs.openRun(live.id)));
      parts.push(stripButton("Stop", () => runs.stopRun(live.id)));
    } else if (divergedShown()) {
      parts.push(el("span", "map-run-status", "The checkout moved to another history."));
      const update = stripButton("Update", () => startRun("api/analysis/update", {}));
      update.classList.add("map-update");
      update.title = "Asks the Analyzer again for what is outdated here, and compares from here on";
      parts.push(update);
    } else if (view.check?.state === "blocked") {
      parts.push(el("span", "map-run-status map-run-problem", view.check.reason));
    } else if (ended && runProblem(ended)) {
      parts.push(el("span", "map-run-status map-run-problem", runProblem(ended)));
      parts.push(stripButton("Open run", () => runs.openRun(ended.id)));
      if (view.retry) {
        const { route, request } = view.retry;
        parts.push(stripButton("Retry", () => startRun(route, request)));
      }
    }
    strip.hidden = parts.length === 0;
    strip.replaceChildren(...parts);
  }

  function stripButton(text, act) {
    const button = el("button", "button", text);
    button.type = "button";
    button.addEventListener("click", act);
    return button;
  }

  // Called after every change of the runs: the strip follows the run, and a run that ended
  // well wrote the analysis, which is read again: also with the map closed since, for the code's
  // stripes and chips. Then HEAD is checked again: commits may have landed while it ran.
  function runChanged() {
    const tracked = view.runId && runs.byId(view.runId);
    if (tracked && !isLive(tracked) && view.settled !== tracked.id) {
      view.settled = tracked.id;
      load()
        .then(() => {
          openMade(tracked);
          return check();
        })
        .catch((error) => onError(String(error)));
      return;
    }
    if (!view.shown) return;
    renderStrip();
    // Trace waits while a run is under way; the box and the rows are kept as they are.
    renderFlows();
  }

  // A deeper analysis or a flow that stored its diagram opens it: it is what was asked for.
  // Analyze and Update change what is drawn already.
  function openMade(run) {
    if (run.analysis?.kind !== "deeper" && run.analysis?.kind !== "flow") return;
    // In the checkout's analysis, which a change map placed earlier does not hold.
    const made = (view.analysis.data?.diagrams ?? []).find((diagram) => diagram.run === run.id);
    if (!made) return;
    leaveChange();
    openDiagram(made.id);
  }

  // ---- The change map ----------------------------------------------------------------------

  // Whether the map can show what a run touched (run-dialog.js): "none" with no analysis to
  // read, "no-architecture" when it has no Architecture diagram, else "ready".
  function changeMapState() {
    if (view.analysis.status !== "ready") return "none";
    const top = (diagram) => diagram.kind === "architecture" && !diagram.parent;
    return view.analysis.data.diagrams.some(top) ? "ready" : "no-architecture";
  }

  // Draws what runs `from` to `to` changed on the Architecture, placed where its code was
  // before the first of them (docs/review-assist-plan.md, 8.6): touched nodes filled, those
  // one edge away outlined, and the changed files no node covers under "Not on the map".
  async function showChangeMap(from, to) {
    const asked = ++changeAsked;
    // A glide under way pans the diagram this replaces.
    stopGlide();
    view.change = {
      status: "loading",
      from,
      to,
      error: null,
      analysis: null,
      touched: null,
    };
    view.kind = "architecture";
    view.selected = null;
    ensureDagre();
    render();
    let read;
    try {
      read = await readChange(from, to);
    } catch (error) {
      read = { status: "error", error: String(error.message ?? error) };
    }
    if (asked !== changeAsked) return;
    // `render` draws the diagram chosen while it is an Architecture, else the first one.
    view.change = { ...view.change, ...read };
    render();
    onChange();
  }

  // The change map's state once read: `{ status: "ready", analysis, touched }`, or
  // `{ status: "error", error }` with the server's reason.
  async function readChange(from, to) {
    const query = `from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`;
    const changes = await fetch(`api/run/changes?${query}`);
    if (!changes.ok) return { status: "error", error: await changes.text() };
    const { before, patch } = await changes.json();
    const placed = await fetch(`api/analysis?at=${encodeURIComponent(before)}`);
    if (!placed.ok) return { status: "error", error: await placed.text() };
    const analysis = await placed.json();
    if (!analysis) return { status: "error", error: "There is no analysis to draw the change on." };
    const files = parsePatch(patch);
    return { status: "ready", error: null, analysis, touched: touched(analysis, files) };
  }

  // Leaves the change map for the repository's map, and drops a reply still to come. Returns
  // whether a change map was shown.
  function leaveChange() {
    if (!view.change) return false;
    view.change = null;
    changeAsked += 1;
    return true;
  }

  // Escape on a change map (app.js): back to the repository's map.
  function leaveChangeMap() {
    if (!leaveChange()) return;
    render();
    onChange();
  }

  // `diagram` as the checkout's analysis places it, for what compares it with the code pane,
  // which shows the files as they are now (`checkoutNode`): the drawn one when there is no
  // change map, or the checkout's analysis no longer has it.
  function checkoutDiagram(diagram) {
    if (!view.change) return diagram;
    const all = view.analysis.data?.diagrams ?? [];
    return all.find((each) => each.id === diagram.id) ?? diagram;
  }

  // "touched" when the change map's run touched node `id`, "near" when it is one edge from one
  // that is, else null.
  function changeMark(id) {
    const marks = view.change?.touched;
    if (marks?.nodes.has(id)) return "touched";
    if (marks?.near.has(id)) return "near";
    return null;
  }

  // ---- The diagram -------------------------------------------------------------------------

  function viewport() {
    return { width: body.clientWidth, height: body.clientHeight };
  }

  function drawDiagram(diagram) {
    // A glide under way is towards a node of the diagram drawn before: it would pan this one.
    stopGlide();
    let positioned;
    try {
      positioned = layoutDiagram(diagram, window.dagre, layouts);
    } catch (error) {
      // dagre has thrown on shapes of graph no test foresaw (diagram.js, `layoutDiagram`): the
      // pane says so, and the rest of the page goes on.
      const card = el("div", "map-card map-error");
      card.append(el("p", "", `The map could not lay out ${diagram.title}: ${error.message}`));
      return showCard(card);
    }
    if (positioned.byId.has(view.selected) === false) view.selected = null;
    const svg = document.createElementNS(SVG, "svg");
    svg.classList.add("map-canvas");
    svg.setAttribute("role", "group");
    svg.setAttribute("aria-label", diagram.title);
    svg.append(arrowMarker());
    const pane = document.createElementNS(SVG, "g");
    pane.classList.add("map-viewport");
    // The cards, and the boundaries' labels, which select and open a boundary as a card does.
    const cards = new Map();
    for (const group of positioned.groups) {
      const { box, head } = groupBox(group);
      cards.set(group.id, head);
      pane.append(box);
    }
    for (const edge of positioned.edges) pane.append(edgeLine(edge));
    // A flow's cards are its steps, in order, numbered as the code's gutter numbers them
    // (diagram.js, `fileMarks`).
    const flow = diagram.kind === "flow";
    for (const [index, box] of positioned.nodes.entries()) {
      const card = nodeCard(box, flow ? index + 1 : null);
      cards.set(box.id, card);
      pane.append(card);
    }
    svg.append(pane);
    const zoom = zoomControls();
    // A card that had the focus goes with the old SVG, and the focus with it to the page's
    // body: the selected card takes it back, as an analysis read again redraws under the
    // keyboard.
    const focused = body.contains(document.activeElement);
    body.replaceChildren(svg, zoom.bar);
    drawn = { diagram, positioned, svg, pane, cards, zoomReadout: zoom.readout };
    if (focused) (cards.get(view.selected) ?? body).focus({ preventScroll: true });
    if (!view.transforms.has(diagram.id) && viewport().width > 0) {
      view.transforms.set(diagram.id, fitTransform(positioned, viewport()));
    }
    applyTransform();
    const placed = checkoutDiagram(diagram);
    view.codeNode = view.code && nodeAtLine(placed, view.code.path, view.code.line);
    syncRings();
    centrePending();
  }

  // Quiet floating zoom/fit controls in the bottom-right corner of the canvas.
  function zoomControls() {
    const bar = el("div", "map-zoom");
    bar.setAttribute("role", "group");
    bar.setAttribute("aria-label", "Zoom");
    const zoomBy = (factor) => {
      if (!drawn) return;
      stopGlide();
      const { width, height } = viewport();
      setTransform(zoomAt(transform(), factor, { x: width / 2, y: height / 2 }));
    };
    const out = el("button", "map-zoom-btn", "−");
    out.type = "button";
    out.title = "Zoom out";
    out.setAttribute("aria-label", "Zoom out");
    out.addEventListener("click", () => zoomBy(1 / 1.25));
    const readout = el("button", "map-zoom-btn map-zoom-fit", "100%");
    readout.type = "button";
    readout.title = "Fit diagram (0)";
    readout.setAttribute("aria-label", "Fit diagram");
    readout.addEventListener("click", () => {
      if (!drawn) return;
      glideTo(fitTransform(drawn.positioned, viewport()));
    });
    const zoomIn = el("button", "map-zoom-btn", "+");
    zoomIn.type = "button";
    zoomIn.title = "Zoom in";
    zoomIn.setAttribute("aria-label", "Zoom in");
    zoomIn.addEventListener("click", () => zoomBy(1.25));
    bar.append(out, readout, zoomIn);
    return { bar, readout };
  }

  // Centres the node `revealNode` asked for, once its diagram is drawn in a pane with a size.
  function centrePending() {
    const box = pendingCentre && drawn?.positioned.byId.get(pendingCentre);
    if (!box || viewport().width === 0) return;
    pendingCentre = null;
    setTransform(centreOn(box, viewport(), transform()));
    drawn.cards.get(box.id)?.focus({ preventScroll: true });
  }

  function svgEl(tag, className, attributes = {}) {
    const node = document.createElementNS(SVG, tag);
    if (className) node.setAttribute("class", className);
    for (const [name, value] of Object.entries(attributes)) node.setAttribute(name, value);
    return node;
  }

  function svgText(className, x, y, text) {
    const node = svgEl("text", className, { x, y });
    node.textContent = text;
    return node;
  }

  function arrowMarker() {
    const defs = svgEl("defs");
    const marker = svgEl("marker", "", {
      id: "map-arrow",
      viewBox: "0 0 8 8",
      refX: "6.5",
      refY: "4",
      markerWidth: "7",
      markerHeight: "7",
      orient: "auto-start-reverse",
    });
    marker.append(svgEl("path", "map-arrow", { d: "M1 1.5 L7 4 L1 6.5 L2.4 4 z" }));
    defs.append(marker);
    return defs;
  }

  // A boundary with its cards in it: drawn dashed, as a card is, when its code is outdated or
  // was never found.
  function groupBox(box) {
    const group = svgEl("g", `map-group ${nodeState(box.node)}`);
    const mark = changeMark(box.id);
    if (mark) group.classList.add(mark);
    const head = svgEl("g", "map-group-head", {
      role: "button",
      tabindex: "0",
      "data-node-id": box.id,
      "aria-label": `${box.node.label}, boundary${mark ? `, ${mark}` : ""}`,
    });
    head.append(svgText("map-group-label", box.x + 12, box.y + 17, box.node.label));
    head.classList.toggle("selected", box.id === view.selected);
    group.append(
      svgEl("rect", "map-group-box", {
        x: box.x,
        y: box.y,
        width: box.width,
        height: box.height,
        rx: 10,
      }),
      head,
    );
    return { box: group, head };
  }

  // Smooths multi-point dagre routes with quadratic curves through segment midpoints;
  // two-point edges stay straight.
  function edgePath(points) {
    if (points.length <= 2) {
      return points.map((point, index) => `${index ? "L" : "M"}${point.x} ${point.y}`).join(" ");
    }
    const parts = [`M${points[0].x} ${points[0].y}`];
    for (let index = 1; index < points.length - 1; index += 1) {
      const current = points[index];
      const next = points[index + 1];
      const midX = (current.x + next.x) / 2;
      const midY = (current.y + next.y) / 2;
      parts.push(`Q${current.x} ${current.y} ${midX} ${midY}`);
    }
    const last = points[points.length - 1];
    parts.push(`L${last.x} ${last.y}`);
    return parts.join(" ");
  }

  function edgeLine({ edge, points }) {
    const line = svgEl("g", "map-edge");
    const d = edgePath(points);
    line.append(svgEl("path", "map-edge-line", { d, "marker-end": "url(#map-arrow)" }));
    if (edge.label) {
      const middle =
        points.length === 2
          ? { x: (points[0].x + points[1].x) / 2, y: (points[0].y + points[1].y) / 2 }
          : points[Math.floor(points.length / 2)];
      line.append(svgText("map-edge-label", middle.x, middle.y - 5, shorten(edge.label, 28)));
    }
    return line;
  }

  // A card: the node's label and summary, or "not found in code"; its shape by its kind; a
  // mark when a diagram deepens it; in a flow, its `step` number before the label. A button
  // for the keyboard and assistive technology.
  function nodeCard(box, step) {
    const { node } = box;
    const state = nodeState(node);
    const card = svgEl("g", `map-node kind-${node.kind} ${state}`, {
      transform: `translate(${box.x} ${box.y})`,
      role: "button",
      tabindex: "0",
      "data-node-id": node.id,
    });
    const named = step ? `Step ${step}, ${node.label}` : node.label;
    const mark = changeMark(node.id);
    if (mark) card.classList.add(mark);
    card.setAttribute(
      "aria-label",
      `${named}, ${node.kind}${state === "found" ? "" : `, ${state}`}${mark ? `, ${mark}` : ""}`,
    );
    const title = svgEl("title");
    title.textContent = node.summary ? `${node.label}\n${node.summary}` : node.label;
    card.append(
      title,
      svgEl("rect", "map-node-box", { width: box.width, height: box.height, rx: 8 }),
    );
    if (node.kind === "store")
      card.append(svgEl("path", "map-node-band", { d: `M0.5 9 H${box.width - 0.5}` }));
    if (node.kind === "external") {
      const inset = { x: 3.5, y: 3.5, width: box.width - 7, height: box.height - 7, rx: 5.5 };
      card.append(svgEl("rect", "map-node-inset", inset));
    }
    const outdated = node.summary ? `outdated · ${node.summary}` : "outdated";
    const sub = { unverified: "not found in code", outdated }[state];
    // The step number sits in a small pill badge before the label.
    let left = 12;
    let labelMax = 24;
    if (step) {
      const digits = String(step);
      const badgeW = Math.max(18, digits.length * 7 + 8);
      card.append(
        svgEl("rect", "map-node-step-bg", { x: 10, y: 11, width: badgeW, height: 15, rx: 4 }),
        svgText("map-node-step", 10 + badgeW / 2, 22, digits),
      );
      left = 10 + badgeW + 6;
      labelMax = digits.length > 1 ? 17 : 20;
    }
    const hasChild = Boolean(childDiagram(shownAnalysis(), node.id));
    if (hasChild) labelMax -= 2;
    card.append(
      svgText("map-node-label", left, 23, shorten(node.label, labelMax)),
      svgText("map-node-summary", 12, 41, shorten(sub ?? node.summary, 30)),
    );
    if (hasChild) {
      card.append(svgText("map-node-deeper", box.width - 16, 23, "›"));
    }
    card.classList.toggle("selected", node.id === view.selected);
    return card;
  }

  function shorten(text, length) {
    return text.length > length ? `${text.slice(0, length - 1)}…` : text;
  }

  // Rings the nodes the search matches and the node of the code pane's line.
  function syncRings() {
    if (!drawn) return;
    const matches = new Set(view.search ? searchNodes(drawn.diagram, view.search) : []);
    drawn.svg.classList.toggle("map-searching", matches.size > 0);
    for (const [id, card] of drawn.cards) {
      const matched = matches.has(id);
      card.classList.toggle("matched", matched);
      card.classList.toggle("ringed", matched || id === view.codeNode);
    }
  }

  // ---- Pan and zoom ------------------------------------------------------------------------

  function transform() {
    return view.transforms.get(drawn.diagram.id) ?? { x: 0, y: 0, scale: 1 };
  }

  function setTransform(next) {
    view.transforms.set(drawn.diagram.id, clampTransform(drawn.positioned, viewport(), next));
    applyTransform();
  }

  function applyTransform() {
    if (!drawn) return;
    const { x, y, scale } = transform();
    drawn.pane.setAttribute("transform", `translate(${x} ${y}) scale(${scale})`);
    if (drawn.zoomReadout) drawn.zoomReadout.textContent = `${Math.round(scale * 100)}%`;
  }

  function stopGlide() {
    if (glide) cancelAnimationFrame(glide);
    glide = null;
  }

  // Pans to `target` over GLIDE_MS, easing out, or at once when motion is reduced. Any pan or
  // zoom by hand stops it; once it lands no frame is asked for.
  function glideTo(target) {
    stopGlide();
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return setTransform(target);
    const from = transform();
    const started = performance.now();
    const frame = (now) => {
      const t = Math.min(1, (now - started) / GLIDE_MS);
      const eased = 1 - (1 - t) ** 3;
      const at = (a, b) => a + (b - a) * eased;
      setTransform({ x: at(from.x, target.x), y: at(from.y, target.y), scale: target.scale });
      glide = t < 1 ? requestAnimationFrame(frame) : null;
    };
    glide = requestAnimationFrame(frame);
  }

  // Set from gesturestart to gestureend (below): the pinch's ctrl+wheel events then zoom
  // nothing.
  let gesturing = false;

  body.addEventListener(
    "wheel",
    (event) => {
      if (!drawn) return;
      event.preventDefault();
      stopGlide();
      const now = transform();
      if (event.ctrlKey) {
        // The gesture events below zoom a pinch already: a second zoom here would double it.
        if (gesturing) return;
        // A trackpad pinch arrives as a wheel with the control key (WebKit).
        const bounds = body.getBoundingClientRect();
        const at = { x: event.clientX - bounds.left, y: event.clientY - bounds.top };
        setTransform(zoomAt(now, Math.exp(-event.deltaY * PINCH_RATE), at));
      } else setTransform({ ...now, x: now.x - event.deltaX, y: now.y - event.deltaY });
    },
    { passive: false },
  );

  // WebKit reports a trackpad pinch as gesture events, each with the scale since the gesture
  // began: the zoom follows the change from the last one, about the pointer.
  let pinched = 1;
  body.addEventListener("gesturestart", (event) => {
    event.preventDefault();
    pinched = 1;
    gesturing = true;
  });
  body.addEventListener("gestureend", () => {
    gesturing = false;
  });
  body.addEventListener("gesturechange", (event) => {
    if (!drawn) return;
    event.preventDefault();
    stopGlide();
    const bounds = body.getBoundingClientRect();
    const at = { x: event.clientX - bounds.left, y: event.clientY - bounds.top };
    setTransform(zoomAt(transform(), event.scale / pinched, at));
    pinched = event.scale;
  });

  // A drag on the background pans; one that starts on a card or zoom control leaves the click to it.
  let panning = null;
  body.addEventListener("pointerdown", (event) => {
    if (!drawn || event.button !== 0 || event.target.closest("[data-node-id], .map-zoom")) return;
    stopGlide();
    body.setPointerCapture(event.pointerId);
    panning = { x: event.clientX, y: event.clientY, from: transform() };
    body.classList.add("panning");
  });
  body.addEventListener("pointermove", (event) => {
    if (!panning) return;
    const { from } = panning;
    setTransform({
      ...from,
      x: from.x + event.clientX - panning.x,
      y: from.y + event.clientY - panning.y,
    });
  });
  body.addEventListener("lostpointercapture", () => {
    panning = null;
    body.classList.remove("panning");
  });

  // A pane resized (a divider, the window) keeps the pan, brought back if it lost every node;
  // a diagram drawn while the pane had no size is fitted once it has one, or centred on the
  // node `revealNode` asked for.
  new ResizeObserver(() => {
    if (!drawn || viewport().width === 0) return;
    if (!view.transforms.has(drawn.diagram.id)) {
      view.transforms.set(drawn.diagram.id, fitTransform(drawn.positioned, viewport()));
    }
    setTransform(transform());
    centrePending();
  }).observe(body);

  // ---- Selection and the keyboard ----------------------------------------------------------

  // Selects node `id`, glides to it, and focuses its card. With `show`, a click's or Space's,
  // its code is shown, or its dialog opens when none of its code was found or the map takes
  // the whole pane (`alone`). On a change map, a touched node shows the run's changes at its
  // first hunk instead.
  function select(id, { show }) {
    if (!drawn) return;
    view.selected = id;
    for (const [each, card] of drawn.cards) card.classList.toggle("selected", each === id);
    const box = drawn.positioned.byId.get(id);
    glideTo(centreOn(box, viewport(), transform()));
    drawn.cards.get(id)?.focus({ preventScroll: true });
    if (!show) return;
    if (view.change?.touched.nodes.has(id)) {
      const { from, to } = view.change;
      showChangesAt(from, to, (file, hunk) => nodeTouchesHunk(box.node, file, hunk));
      return;
    }
    const node = checkoutNode(box.node);
    const anchor = firstFound(node);
    if (anchor && !alone()) showAnchor(anchor);
    else openNode(node);
  }

  // `node` as the checkout's analysis places it: a change map's nodes are placed at the run's
  // fork, and their lines would open the wrong lines of the files as they are now. The node
  // itself when the checkout's analysis no longer has it.
  function checkoutNode(node) {
    if (!view.change) return node;
    const all = (view.analysis.data?.diagrams ?? []).flatMap((diagram) => diagram.nodes);
    return all.find((each) => each.id === node.id) ?? node;
  }

  body.addEventListener("click", (event) => {
    const card = event.target.closest("[data-node-id]");
    if (card) select(card.dataset.nodeId, { show: true });
  });

  // Selects node `nodeId` of diagram `diagramId`, a click on its stripe or chip in the code: at
  // once when that diagram is drawn, else once it is, opened if need be.
  function revealNode(diagramId, nodeId) {
    // A stripe or chip of the code is the checkout's: shown on the repository's map.
    const left = leaveChange();
    if (!left && drawn?.diagram.id === diagramId && drawn.cards.has(nodeId)) {
      select(nodeId, { show: false });
      return;
    }
    const switched = view.diagramId !== diagramId;
    view.diagramId = diagramId;
    view.selected = nodeId;
    pendingCentre = nodeId;
    render();
    if (switched) onChange();
  }

  // Follows the code pane's line `line` (1-based) of the file at `path`, or nothing with null:
  // rings the drawn diagram's node for it, and pans to that node when it is wholly off screen.
  // It never opens another diagram, and never pans while the pane is dragged.
  function followCode(path, line) {
    view.code = path && line ? { path, line } : null;
    if (!view.shown || !drawn) return;
    const next = view.code && nodeAtLine(checkoutDiagram(drawn.diagram), path, line);
    if (next === view.codeNode) return;
    view.codeNode = next;
    syncRings();
    if (next === null || panning || viewport().width === 0) return;
    // The checkout's node may be one a change map does not draw.
    const box = drawn.positioned.byId.get(next);
    if (!box) return;
    const { x, y, scale } = transform();
    const { width, height } = viewport();
    const left = x + box.x * scale;
    const top = y + box.y * scale;
    const off =
      left + box.width * scale < 0 || left > width || top + box.height * scale < 0 || top > height;
    if (off) glideTo(centreOn(box, viewport(), transform()));
  }
  body.addEventListener("dblclick", (event) => {
    const card = event.target.closest("[data-node-id]");
    if (card) openNode(drawn.positioned.byId.get(card.dataset.nodeId).node);
  });

  // Right-click on a node: it is selected, and the menu offers what its dialog does to make
  // more of the map.
  body.addEventListener("contextmenu", (event) => {
    const card = event.target.closest("[data-node-id]");
    if (!card || !drawn) return;
    event.preventDefault();
    const { node } = drawn.positioned.byId.get(card.dataset.nodeId);
    select(node.id, { show: false });
    // The context-menu key gives no pointer position: the menu opens at the card.
    const box = card.getBoundingClientRect();
    menu.open(event.clientX || box.left + 24, event.clientY || box.bottom, [
      {
        label: "Analyze Deeper",
        detail: node.label,
        disabled: Boolean(liveRun()),
        run: () => analyzeDeeper(node.id),
      },
      {
        label: "Trace from Here",
        detail: node.label,
        run: () => traceFrom({ of: node.id, label: node.label }),
      },
    ]);
  });

  mapEl.addEventListener("keydown", (event) => {
    if (event.target.matches("input, select") || event.metaKey || event.ctrlKey) return;
    if (event.key === "/") {
      event.preventDefault();
      tools.querySelector(".map-search")?.focus();
      return;
    }
    if (!drawn) return;
    if (event.key === "0") {
      event.preventDefault();
      glideTo(fitTransform(drawn.positioned, viewport()));
      return;
    }
    const card = event.target.closest?.("[data-node-id]");
    if (!card) return;
    const id = card.dataset.nodeId;
    if (event.key === " ") {
      event.preventDefault();
      select(id, { show: true });
    } else if (event.key === "Enter") {
      // Into the node's deeper diagram, else its dialog, which offers Analyze deeper.
      event.preventDefault();
      const child = childDiagram(shownAnalysis(), id);
      if (child) openDiagram(child.id);
      else openNode(drawn.positioned.byId.get(id).node);
    } else if (event.key.startsWith("Arrow")) {
      event.preventDefault();
      const next = arrowTarget(drawn.positioned, id, event.key);
      if (next) select(next, { show: false });
    }
  });

  // ---- The node's dialog -------------------------------------------------------------------

  function openNode(shownNode) {
    const node = checkoutNode(shownNode);
    const head = el("header", "note-dialog-header");
    const title = el("h2", "note-dialog-title", node.label);
    title.id = "node-dialog-title";
    const close = el("button", "button note-dialog-close", "esc");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => dialogEl.close());
    const actions = el("span", "note-dialog-actions");
    actions.append(close);
    const state = nodeState(node);
    const where = { found: node.kind, outdated: `${node.kind} · outdated` };
    head.append(
      title,
      el("span", "note-dialog-where", where[state] ?? `${node.kind} · not found in code`),
    );
    head.append(actions);
    dialogEl.replaceChildren(head, nodeBody(node), nodeFooter(node));
    if (!dialogEl.open) dialogEl.showModal();
  }

  function section(label, ...content) {
    const part = el("section", "note-dialog-section");
    const heading = el("div", "note-dialog-label");
    heading.append(el("span", "", label));
    part.append(heading, ...content);
    return part;
  }

  function nodeBody(node) {
    const main = el("div", "note-dialog-body");
    if (node.summary) main.append(el("div", "note-dialog-note", node.summary));
    const found = firstFound(node);
    const show = el("button", "button node-show", "Show in code");
    show.type = "button";
    show.disabled = !found;
    show.title = found ? anchorPlace(found) : notFound(node);
    show.addEventListener("click", () => {
      dialogEl.close();
      showAnchor(found);
    });
    main.append(show);
    if (node.detail.trim()) {
      const detail = el("div", "run-message node-detail");
      fillMarkdown(detail, node.detail);
      main.append(section("Detail", detail));
    }
    main.append(section("Code", anchorList(node.anchors)));
    const links = edgeList(node);
    if (links) main.append(links);
    const noted = notesOn(notes(), node.anchors);
    if (noted.length) {
      const cards = el("div", "node-notes");
      for (const note of noted) cards.append(noteCard(note));
      main.append(section(noted.length === 1 ? "Note" : `Notes · ${noted.length}`, cards));
    }
    const child = childDiagram(shownAnalysis(), node.id);
    if (child) {
      const deeper = el("button", "button node-deeper", `Open ${child.title}`);
      deeper.type = "button";
      deeper.addEventListener("click", () => {
        dialogEl.close();
        openDiagram(child.id);
      });
      main.append(deeper);
    }
    main.append(nodeActions(node));
    return main;
  }

  // What makes more of the map from `node`: a diagram of its inside, which replaces the one it
  // has, and a flow that starts there.
  function nodeActions(node) {
    const actions = el("div", "node-actions");
    const action = (text, className, act) => {
      const button = el("button", `button ${className}`, text);
      button.type = "button";
      button.addEventListener("click", () => {
        dialogEl.close();
        act();
      });
      actions.append(button);
      return button;
    };
    const deeper = action("Analyze deeper", "node-analyze-deeper", () => analyzeDeeper(node.id));
    deeper.disabled = Boolean(liveRun());
    if (view.analysis.data && childDiagram(view.analysis.data, node.id)) {
      deeper.title = "Draws its inside again, in place of the diagram it has";
    }
    action("Trace from here", "node-trace", () => traceFrom({ of: node.id, label: node.label }));
    return actions;
  }

  // Each reference, a button to its code; an unverified one says so, and why when known.
  function anchorList(anchors) {
    const list = el("div", "node-anchors");
    if (!anchors.length) list.append(el("div", "note-dialog-why", "The agent cited no code."));
    for (const anchor of anchors) {
      const row = el("button", `node-anchor ${anchor.state}`);
      row.type = "button";
      row.append(el("span", "node-anchor-place", anchorPlace(anchor)));
      if (anchor.state !== "found") {
        const why = anchor.state === "outdated" ? "outdated" : "not found in code";
        row.append(
          el("span", "node-anchor-state", anchor.reason ? `${why}: ${anchor.reason}` : why),
        );
      }
      row.disabled = anchor.state === "unverified";
      row.addEventListener("click", () => {
        dialogEl.close();
        showAnchor(anchor);
      });
      list.append(row);
    }
    return list;
  }

  // The edges into and out of `node`, each to the node at its other end, with the code that
  // justifies it when the agent cited one.
  function edgeList(node) {
    // Each edge's code is shown in the files as they are now.
    const diagram = checkoutDiagram(drawn?.diagram ?? current());
    const byId = new Map(diagram.nodes.map((each) => [each.id, each]));
    const touching = diagram.edges.filter((edge) => edge.from === node.id || edge.to === node.id);
    if (!touching.length) return null;
    const list = el("div", "node-edges");
    for (const edge of touching) {
      const out = edge.from === node.id;
      const other = byId.get(out ? edge.to : edge.from);
      const row = el("div", "node-edge");
      const go = el("button", "node-edge-node", `${out ? "→" : "←"} ${other.label}`);
      go.type = "button";
      go.addEventListener("click", () => openNode(other));
      row.append(go, el("span", "node-edge-label", edge.label));
      if (edge.anchor && edge.anchor.state !== "unverified") {
        const code = el("button", "node-edge-code", anchorPlace(edge.anchor));
        code.type = "button";
        code.addEventListener("click", () => {
          dialogEl.close();
          showAnchor(edge.anchor);
        });
        row.append(code);
      }
      list.append(row);
    }
    return section("Connections", list);
  }

  // Why a node offers no code to show or note: its code moved or went since the analysis, or
  // it was never found.
  function notFound(node) {
    return nodeState(node) === "outdated"
      ? "Its code changed since the analysis: Update looks for it again."
      : "None of its code was found.";
  }

  function nodeFooter(node) {
    const foot = el("footer", "note-dialog-footer");
    const found = firstFound(node);
    const why = el("span", "note-dialog-why", found ? anchorPlace(found) : notFound(node));
    const button = (text, intent) => {
      const act = el("button", `button node-${intent}`, text);
      act.type = "button";
      act.disabled = !found;
      act.addEventListener("click", () => {
        dialogEl.close();
        writeNote(found, intent);
      });
      return act;
    };
    foot.append(why, el("span", "spacer"), button("Ask", "ask"), button("Add note", "change"));
    return foot;
  }

  // A click on the backdrop lands on the dialog element itself, outside its panel.
  dialogEl.addEventListener("click", (event) => {
    if (event.target === dialogEl) dialogEl.close();
  });

  return {
    // Shows or hides the map. The first show loads the layout library; every show reads the
    // analysis again.
    setShown(shown) {
      if (view.shown === shown) return;
      view.shown = shown;
      onChange();
      if (!shown) {
        flowDictation.cancel();
        return;
      }
      render();
      ensureDagre();
      load().catch((error) => onError(String(error)));
    },
    runChanged,
    // Reads the analysis again, as at start and on the window's focus: another window may have
    // analyzed, or the code under a node moved. `fetchAnalysis` and `applyAnalysis` are its two
    // halves, for a caller that draws the reply with others.
    load,
    fetchAnalysis,
    applyAnalysis,
    // Compares the analysis with HEAD, which may start the forced update (`check`).
    check,
    // The analysis read last, or null when there is none or it cannot be used.
    analysis: () => view.analysis.data,
    isShown: () => view.shown,
    // The id of the diagram the map shows, or null while it is hidden.
    openDiagramId: () => (view.shown ? (current()?.id ?? null) : null),
    // Labels of the nodes in the currently open diagram, for dictation contextualStrings.
    nodeLabels: () => (view.shown ? (current()?.nodes?.map((node) => node.label) ?? []) : []),
    revealNode,
    followCode,
    focus,
    // Analyze deeper on a node id or a path, Trace from here at `{ of, label }`, as the file
    // list and the code's menus ask; and whether an analysis of this window is under way, when
    // Analyze Deeper waits.
    analyzeDeeper,
    traceFrom,
    isAnalyzing: () => Boolean(liveRun()),
    hasDraft,
    holdDraft,
    // The run dialog's Change map (run-dialog.js): whether it is offered, and what it shows.
    changeMapState,
    showChangeMap,
    // Whether the map shows a change map, which Escape leaves, and leaving it.
    showsChangeMap: () => view.shown && Boolean(view.change),
    leaveChangeMap,
  };
}
