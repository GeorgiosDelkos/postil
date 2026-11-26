// The map: the analysis's diagrams (`GET api/analysis`, src/analysis.rs) drawn as SVG in the
// pane left of the code, with pan, zoom, search, the keyboard, and the dialog a node opens
// (docs/review-assist.md, "What the engineer sees"). ui/diagram.js has the rules.
//
// The layout library, dagre (ui/vendor/dagre.LICENSE), is loaded by a script element the first
// time the map opens, so a window that never opens it never loads it. Every label, summary and
// detail is an agent's text: it is set as text, and the detail through the Markdown subset
// (dialog-parts.js, `fillMarkdown`), never as HTML.

import { fillMarkdown } from "./dialog-parts.js";
import {
  anchorPlace,
  arrowTarget,
  breadcrumb,
  centreOn,
  childDiagram,
  clampTransform,
  diagramCounts,
  firstFound,
  fitTransform,
  layoutDiagram,
  nodeState,
  notesOn,
  runProblem,
  searchNodes,
  zoomAt,
} from "./diagram.js";
import { el } from "./dom.js";
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

// The kinds a diagram is, as the switcher names them.
const KINDS = [
  ["architecture", "Architecture"],
  ["flow", "Flows"],
  ["data", "Data"],
];

// `mapEl` is the map's pane and `dialogEl` the node's `<dialog>`. `runs` is the run panel
// (run-panel.js), which follows the analysis runs. The callbacks:
// - `settings()` gives the settings in effect, for the Analyzer's name;
// - `notes()` gives the notes, for those on a node's code;
// - `noteCard(note)` builds a note's card, as the tray draws it;
// - `showAnchor(anchor)` shows a code reference in the code pane;
// - `writeNote(anchor, intent)` writes a note ("change") or asks a question ("ask") on it;
// - `alone()` says whether the map takes the whole code pane (a narrow window): a click then
//   opens the node's dialog, whose Show in code gives the pane to the code;
// - `onError(text)` shows a failure.
export function diagramView({
  mapEl,
  dialogEl,
  runs,
  settings,
  notes,
  noteCard,
  showAnchor,
  writeNote,
  alone,
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
    // Uncommitted changes, for the empty state (`GET api/run/uncommitted`).
    uncommitted: null,
  };
  // Layouts by diagram id and run, for the session (diagram.js, `layoutDiagram`).
  const layouts = new Map();
  // The drawn diagram: its layout, the SVG's viewport group, and the node elements by id.
  let drawn = null;
  let glide = null;
  let loading = null;

  const header = el("header", "pane-header map-header");
  const tools = el("div", "map-tools");
  const strip = el("div", "map-run");
  const body = el("div", "map-body");
  body.tabIndex = -1;
  mapEl.replaceChildren(header, tools, strip, body);

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
    const response = await fetch("api/analysis");
    if (!response.ok) {
      view.analysis = { status: "error", data: null, error: await response.text() };
    } else {
      const data = await response.json();
      view.analysis = { status: data ? "ready" : "none", data, error: null };
    }
    if (view.analysis.status !== "ready") await loadUncommitted();
    render();
  }

  async function loadUncommitted() {
    const response = await fetch("api/run/uncommitted").catch(() => null);
    view.uncommitted = response?.ok ? await response.json() : null;
  }

  // ---- Choosing the diagram ----------------------------------------------------------------

  function diagrams() {
    return view.analysis.data?.diagrams ?? [];
  }

  // The diagrams the switcher's `kind` shows at the top: those of that kind with no parent.
  function topOf(kind) {
    return diagrams().filter((diagram) => diagram.kind === kind && !diagram.parent);
  }

  // The diagram to draw: the one chosen while it is still there, else the first at the top of
  // the chosen kind, else the first of any kind.
  function current() {
    const chosen = diagrams().find((diagram) => diagram.id === view.diagramId);
    const shown = chosen ?? topOf(view.kind)[0] ?? diagrams()[0] ?? null;
    if (shown) {
      view.diagramId = shown.id;
      view.kind = shown.kind;
    }
    return shown;
  }

  function openDiagram(id) {
    view.diagramId = id;
    view.selected = null;
    render();
  }

  // ---- Drawing -----------------------------------------------------------------------------

  function render() {
    if (!view.shown) return;
    const diagram = view.analysis.status === "ready" ? current() : null;
    renderHeader(diagram);
    renderTools(diagram);
    renderStrip();
    if (view.analysis.status === "loading") return showCard(el("div", "map-card", "Loading…"));
    if (view.analysis.status === "error") return showCard(errorCard());
    if (!diagram) return showCard(emptyCard());
    if (view.dagre === "failed") return showCard(dagreCard());
    if (view.dagre !== "ready") return showCard(el("div", "map-card", "Loading the map…"));
    drawDiagram(diagram);
  }

  function renderHeader(diagram) {
    const crumbs = el("div", "crumbs map-crumbs");
    if (diagram) {
      const trail = breadcrumb(view.analysis.data, diagram);
      trail.forEach((step, index) => {
        if (index > 0) crumbs.append(" › ");
        if (index === trail.length - 1) crumbs.append(el("b", "", step.title));
        else {
          const up = el("button", "map-crumb", step.title);
          up.type = "button";
          up.addEventListener("click", () => openDiagram(step.id));
          crumbs.append(up);
        }
      });
    } else crumbs.append(el("b", "", "Map"));
    const meta = el("div", "meta map-counts");
    if (diagram) {
      const counts = diagramCounts(diagram);
      const words = [`${counts.total} node${counts.total === 1 ? "" : "s"}`];
      if (counts.unverified) words.push(`${counts.unverified} unverified`);
      if (counts.outdated) words.push(`${counts.outdated} outdated`);
      meta.textContent = words.join(" · ");
      if (counts.dropped) {
        meta.title = counts.dropped;
        meta.append(el("span", "map-dropped", " · some left out"));
      }
    }
    header.replaceChildren(crumbs, meta);
  }

  function renderTools(diagram) {
    tools.hidden = !diagram;
    if (!diagram) return;
    const kinds = el("div", "map-kinds");
    kinds.setAttribute("role", "group");
    kinds.setAttribute("aria-label", "Diagram");
    for (const [kind, name] of KINDS) {
      const button = el("button", "map-kind", name);
      button.type = "button";
      button.setAttribute("aria-pressed", String(view.kind === kind));
      button.disabled = topOf(kind).length === 0;
      button.addEventListener("click", () => {
        view.kind = kind;
        openDiagram(topOf(kind)[0].id);
      });
      kinds.append(button);
    }
    const parts = [kinds];
    const flows = topOf("flow");
    if (view.kind === "flow" && flows.length > 1) parts.push(flowPicker(flows, diagram));

    const search = el("input", "field map-search");
    search.type = "search";
    search.placeholder = "Find node  /";
    search.setAttribute("aria-label", "Find a node");
    search.value = view.search;
    search.addEventListener("input", () => {
      view.search = search.value;
      ringMatches();
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
        ringMatches();
      }
    });
    parts.push(search);

    if (diagramCounts(diagram).outdated > 0) {
      const update = el("button", "button map-update", "Update");
      update.type = "button";
      update.title = "Asks the Analyzer again for the outdated nodes of this diagram";
      update.disabled = Boolean(liveRun());
      update.addEventListener("click", () =>
        startRun("api/analysis/update", { diagram: diagram.id }),
      );
      parts.push(update);
    }
    tools.replaceChildren(...parts);
  }

  function flowPicker(flows, diagram) {
    const picker = el("select", "field map-flow");
    picker.setAttribute("aria-label", "Flow");
    for (const flow of flows) {
      const option = el("option", "", flow.title);
      option.value = flow.id;
      option.selected = flow.id === diagram.id;
      picker.append(option);
    }
    picker.addEventListener("change", () => openDiagram(picker.value));
    return picker;
  }

  function showCard(card) {
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
  function emptyCard() {
    const card = el("div", "map-card map-empty");
    const slot = settings()?.analyzer ?? settings()?.agent;
    const analyzer = slot ? formatSlot(slot) : "the agent";
    card.append(
      el("h2", "map-empty-title", "No map of this repository yet"),
      el(
        "p",
        "",
        `${analyzer} reads the last commit (HEAD), read-only, and draws its parts and how they connect.`,
      ),
    );
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

  async function startRun(route, request) {
    const response = await fetch(route, { method: "POST", body: JSON.stringify(request) });
    if (!response.ok) {
      onError(await response.text());
      return;
    }
    view.runId = (await response.json()).run;
    view.retry = { route, request };
    await runs.track();
    render();
  }

  // The strip under the tools: the run under way, with Open run and Stop, or why the last one
  // stored nothing, with Open run and Retry.
  function renderStrip() {
    const live = liveRun();
    const ended = !live && view.runId ? runs.byId(view.runId) : null;
    const parts = [];
    if (live) {
      parts.push(el("span", "map-run-status", `Analyzing with ${live.agent}…`));
      parts.push(stripButton("Open run", () => runs.openRun(live.id)));
      parts.push(stripButton("Stop", () => runs.stopRun(live.id)));
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
  // well wrote the analysis, which is read again.
  function runChanged() {
    if (!view.shown) return;
    const tracked = view.runId && runs.byId(view.runId);
    if (tracked && !isLive(tracked) && view.settled !== tracked.id) {
      view.settled = tracked.id;
      load().catch((error) => onError(String(error)));
      return;
    }
    renderStrip();
  }

  // ---- The diagram -------------------------------------------------------------------------

  function viewport() {
    return { width: body.clientWidth, height: body.clientHeight };
  }

  function drawDiagram(diagram) {
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
    for (const box of positioned.nodes) {
      const card = nodeCard(box);
      cards.set(box.id, card);
      pane.append(card);
    }
    svg.append(pane);
    body.replaceChildren(svg);
    drawn = { diagram, positioned, pane, cards };
    if (!view.transforms.has(diagram.id) && viewport().width > 0) {
      view.transforms.set(diagram.id, fitTransform(positioned, viewport()));
    }
    applyTransform();
    ringMatches();
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
      refX: "7",
      refY: "4",
      markerWidth: "8",
      markerHeight: "8",
      orient: "auto-start-reverse",
    });
    marker.append(svgEl("path", "map-arrow", { d: "M0 0 L8 4 L0 8 z" }));
    defs.append(marker);
    return defs;
  }

  function groupBox(box) {
    const group = svgEl("g", "map-group");
    const head = svgEl("g", "map-group-head", {
      role: "button",
      tabindex: "0",
      "data-node-id": box.id,
      "aria-label": `${box.node.label}, boundary`,
    });
    head.append(svgText("map-group-label", box.x + 12, box.y + 18, box.node.label));
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

  function edgeLine({ edge, points }) {
    const line = svgEl("g", "map-edge");
    const d = points.map((point, index) => `${index ? "L" : "M"}${point.x} ${point.y}`).join(" ");
    line.append(svgEl("path", "map-edge-line", { d, "marker-end": "url(#map-arrow)" }));
    if (edge.label) {
      const middle = points[Math.floor(points.length / 2)];
      line.append(svgText("map-edge-label", middle.x, middle.y - 4, shorten(edge.label, 28)));
    }
    return line;
  }

  // A card: the node's label and summary, or "not found in code"; its shape by its kind; a
  // mark when a diagram deepens it. A button for the keyboard and assistive technology.
  function nodeCard(box) {
    const { node } = box;
    const state = nodeState(node);
    const card = svgEl("g", `map-node kind-${node.kind} ${state}`, {
      transform: `translate(${box.x} ${box.y})`,
      role: "button",
      tabindex: "0",
      "data-node-id": node.id,
    });
    card.setAttribute(
      "aria-label",
      `${node.label}, ${node.kind}${state === "found" ? "" : `, ${state}`}`,
    );
    const title = svgEl("title");
    title.textContent = node.summary ? `${node.label}\n${node.summary}` : node.label;
    card.append(
      title,
      svgEl("rect", "map-node-box", { width: box.width, height: box.height, rx: 8 }),
    );
    if (node.kind === "store")
      card.append(svgEl("path", "map-node-band", { d: `M0 10 H${box.width}` }));
    if (node.kind === "external") {
      const inset = { x: 3, y: 3, width: box.width - 6, height: box.height - 6, rx: 6 };
      card.append(svgEl("rect", "map-node-inset", inset));
    }
    const sub = { unverified: "not found in code", outdated: `outdated · ${node.summary}` }[state];
    card.append(
      svgText("map-node-label", 12, 22, shorten(node.label, 24)),
      svgText("map-node-summary", 12, 40, shorten(sub ?? node.summary, 30)),
    );
    if (childDiagram(view.analysis.data, node.id)) {
      card.append(svgText("map-node-deeper", box.width - 18, 22, "›"));
    }
    card.classList.toggle("selected", node.id === view.selected);
    return card;
  }

  function shorten(text, length) {
    return text.length > length ? `${text.slice(0, length - 1)}…` : text;
  }

  function ringMatches() {
    if (!drawn) return;
    const matches = new Set(view.search ? searchNodes(drawn.diagram, view.search) : []);
    for (const [id, card] of drawn.cards) card.classList.toggle("ringed", matches.has(id));
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

  // A drag on the background pans; one that starts on a card leaves the click to the card.
  let panning = null;
  body.addEventListener("pointerdown", (event) => {
    if (!drawn || event.button !== 0 || event.target.closest("[data-node-id]")) return;
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
  // a diagram drawn while the pane had no size is fitted once it has one.
  new ResizeObserver(() => {
    if (!drawn || viewport().width === 0) return;
    if (!view.transforms.has(drawn.diagram.id)) {
      view.transforms.set(drawn.diagram.id, fitTransform(drawn.positioned, viewport()));
    }
    setTransform(transform());
  }).observe(body);

  // ---- Selection and the keyboard ----------------------------------------------------------

  // Selects node `id`, glides to it, and focuses its card. With `show`, a click's or Space's,
  // its code is shown, or its dialog opens when none of its code was found or the map takes
  // the whole pane (`alone`).
  function select(id, { show }) {
    if (!drawn) return;
    view.selected = id;
    for (const [each, card] of drawn.cards) card.classList.toggle("selected", each === id);
    const box = drawn.positioned.byId.get(id);
    glideTo(centreOn(box, viewport(), transform()));
    drawn.cards.get(id)?.focus({ preventScroll: true });
    if (!show) return;
    const anchor = firstFound(box.node);
    if (anchor && !alone()) showAnchor(anchor);
    else openNode(box.node);
  }

  body.addEventListener("click", (event) => {
    const card = event.target.closest("[data-node-id]");
    if (card) select(card.dataset.nodeId, { show: true });
  });
  body.addEventListener("dblclick", (event) => {
    const card = event.target.closest("[data-node-id]");
    if (card) openNode(drawn.positioned.byId.get(card.dataset.nodeId).node);
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
      event.preventDefault();
      openNode(drawn.positioned.byId.get(id).node);
    } else if (event.key.startsWith("Arrow")) {
      event.preventDefault();
      const next = arrowTarget(drawn.positioned, id, event.key);
      if (next) select(next, { show: false });
    }
  });

  // ---- The node's dialog -------------------------------------------------------------------

  function openNode(node) {
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
    const child = childDiagram(view.analysis.data, node.id);
    if (child) {
      const deeper = el("button", "button node-deeper", `Open ${child.title}`);
      deeper.type = "button";
      deeper.addEventListener("click", () => {
        dialogEl.close();
        openDiagram(child.id);
      });
      main.append(deeper);
    }
    return main;
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
    const diagram = drawn?.diagram ?? current();
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
    // Shows or hides the map. The first show loads the layout library and the analysis.
    setShown(shown) {
      if (view.shown === shown) return;
      view.shown = shown;
      if (!shown) return;
      render();
      ensureDagre();
      load().catch((error) => onError(String(error)));
    },
    runChanged,
    // Reads the analysis again, as on the window's focus: another window may have analyzed.
    refresh: () => (view.shown ? load() : Promise.resolve()),
    focus: () => (drawn?.cards.get(view.selected) ?? body).focus({ preventScroll: true }),
  };
}
