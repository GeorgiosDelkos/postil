// The map's rules, kept apart from the document so `node --test` runs them: a diagram of
// `GET api/analysis` (src/analysis.rs) laid out by dagre, a node's state from its code
// references, search, the breadcrumb, and the pan and zoom arithmetic.
//
// A diagram is `{ id, kind, title, parent, run, nodes, edges, dropped }`; a node `{ id, kind,
// label, summary, detail, group, anchors }`; an edge `{ from, to, label, anchor }`; a code
// reference `{ path, target, lines, snippet, state, reason }`, `state` "found", "outdated" or
// "unverified".

// A node's card in CSS px, at zoom 1: a label and a summary line at the page's 12 px. Fixed,
// so a change of the code font never lays the diagram out again. Arbitrary.
export const NODE_WIDTH = 184;
export const NODE_HEIGHT = 54;

// How far the map zooms out and in: 25% shows a 200-node diagram whole in a 1440 px window;
// past 200% a card's text is larger than anything else on the page.
export const ZOOM_MIN = 0.25;
export const ZOOM_MAX = 2;

// dagre's spacing, in CSS px: between nodes of one rank, between ranks, and around the
// diagram. Arbitrary, read by eye on the fake agent's diagrams.
const SPACING = { nodesep: 28, ranksep: 52, marginx: 24, marginy: 24 };

// The layout of `diagram` by `dagre` (the vendored window.dagre, ui/vendor/). The coordinates
// are kept in `cache` by the diagram's id and the run that made it, while its shape (its
// nodes, their groups, and its edges in order) stays as it was laid out: an Update keeps the
// diagram's run (src/analysis/merge.rs, `update_one`) and may add, delete or regroup nodes and
// edges, and a node's box has a fixed size. The nodes and edges themselves are this call's:
// every read of `api/analysis` places their code references again, so a reference that moved
// has new lines in the same shape.
// Returns `{ width, height, nodes, groups, edges, byId }`: `nodes` the cards and `groups` the
// boundaries with children, each `{ id, node, x, y, width, height }` with (x, y) its top left;
// `edges` each `{ edge, points }`; `byId` every node and group by id.
export function layoutDiagram(diagram, dagre, cache) {
  const key = `${diagram.id}:${diagram.run}`;
  const shape = JSON.stringify([
    diagram.nodes.map((node) => [node.id, node.group ?? null]),
    diagram.edges.map((edge) => [edge.from, edge.to]),
  ]);
  let geometry = cache.get(key);
  if (geometry?.shape !== shape) {
    geometry = { ...layoutWith(diagram, dagre), shape };
    cache.set(key, geometry);
  }

  const parents = boundaries(diagram);
  const byId = new Map();
  const nodes = [];
  const groups = [];
  for (const node of diagram.nodes) {
    const placed = { id: node.id, node, ...geometry.boxes.get(node.id) };
    byId.set(node.id, placed);
    (parents.has(node.id) ? groups : nodes).push(placed);
  }
  const edges = diagram.edges.map((edge, index) => ({
    edge,
    points: geometry.points[index] ?? straight(byId.get(edge.from), byId.get(edge.to)),
  }));
  return { width: geometry.width, height: geometry.height, nodes, groups, edges, byId };
}

// The ids of the boundaries with children: drawn around them, not as cards.
function boundaries(diagram) {
  return new Set(diagram.nodes.map((node) => node.group).filter(Boolean));
}

// `diagram`'s coordinates by dagre: `{ width, height, boxes, points }`, `boxes` each node's
// `{ x, y, width, height }` by id, `points` each edge's route by its index in
// `diagram.edges`, or null for one dagre was not given, which is drawn straight.
function layoutWith(diagram, dagre) {
  const parents = boundaries(diagram);
  const group = new Map(diagram.nodes.map((node) => [node.id, node.group ?? null]));
  // dagre 3.1.3 throws "Not possible to find intersection inside of the rectangle" on compound
  // graphs with an edge between cards in two different boundaries: a bend point it makes for
  // the edge can land on a card's centre. It did on a 200-node diagram in boundaries of ten
  // with a third of its edges crossing them, after about 290 ms of work. So dagre is given
  // only the edges inside one boundary, and those between cards in none; an edge it is not
  // given is drawn straight between its two boxes. The downside accepted (main session,
  // 2026-11-26): an edge across boundaries does not order them left to right.
  const keeps = (from, to) => group.get(from) === group.get(to);
  const card = (id) => !parents.has(id);

  const graph = new dagre.graphlib.Graph({ compound: true, multigraph: true });
  graph.setGraph({ rankdir: "LR", ...SPACING });
  for (const node of diagram.nodes) {
    // dagre sizes a boundary with children to hold them; one without is a card, or dagre would
    // give it no size at all.
    graph.setNode(node.id, card(node.id) ? { width: NODE_WIDTH, height: NODE_HEIGHT } : {});
  }
  for (const node of diagram.nodes) {
    if (node.group) graph.setParent(node.id, node.group);
  }
  // dagre lays out edges between cards only: one to or from a boundary with children is drawn
  // straight between the two boxes, after the layout.
  const laid = new Map();
  diagram.edges.forEach((edge, index) => {
    if (!card(edge.from) || !card(edge.to) || !keeps(edge.from, edge.to)) return;
    laid.set(index, String(index));
    graph.setEdge(edge.from, edge.to, {}, String(index));
  });
  if (diagram.kind === "flow") {
    // A flow's nodes are in step order: each step leans right of the one before, by an edge
    // only the layout sees, so steps the agent did not join still read left to right. Of the
    // default weight: one of weight 0 beside a real edge between the same two cards made dagre
    // throw the error above (a card, two boundaries and a nested one, 2026-11-27).
    const cards = diagram.nodes.filter((node) => card(node.id));
    for (let index = 1; index < cards.length; index += 1) {
      const [from, to] = [cards[index - 1].id, cards[index].id];
      if (keeps(from, to)) graph.setEdge(from, to, {}, `step${index}`);
    }
  }
  dagre.layout(graph);

  const boxes = new Map();
  for (const node of diagram.nodes) {
    const box = graph.node(node.id);
    boxes.set(node.id, {
      x: box.x - box.width / 2,
      y: box.y - box.height / 2,
      width: box.width,
      height: box.height,
    });
  }
  const points = diagram.edges.map((edge, index) => {
    const name = laid.get(index);
    return name === undefined ? null : graph.edge({ v: edge.from, w: edge.to, name }).points;
  });
  const { width, height } = graph.graph();
  return { width, height, boxes, points };
}

// The line between the centres of boxes `from` and `to`, cut where it leaves each box.
function straight(from, to) {
  const centre = (box) => ({ x: box.x + box.width / 2, y: box.y + box.height / 2 });
  const a = centre(from);
  const b = centre(to);
  return [border(from, a, b), border(to, b, a)];
}

// Where the ray from `inside`, the centre of `box`, towards `toward` crosses the box's border.
function border(box, inside, toward) {
  const dx = toward.x - inside.x;
  const dy = toward.y - inside.y;
  if (dx === 0 && dy === 0) return inside;
  const sx = dx === 0 ? Infinity : box.width / 2 / Math.abs(dx);
  const sy = dy === 0 ? Infinity : box.height / 2 / Math.abs(dy);
  const s = Math.min(sx, sy, 1);
  return { x: inside.x + dx * s, y: inside.y + dy * s };
}

// Whether the code a node stands for was found: "unverified" when none of its references
// ever matched the code (docs/review-assist.md, "Code references"), "outdated" when one that
// matched no longer does, else "found".
export function nodeState(node) {
  const states = node.anchors.map((anchor) => anchor.state);
  if (!states.some((state) => state !== "unverified")) return "unverified";
  return states.includes("outdated") ? "outdated" : "found";
}

// The first reference of `node` whose code is where it says, the one a click shows; null when
// it has none.
export function firstFound(node) {
  return node.anchors.find((anchor) => anchor.state === "found") ?? null;
}

// The longest question a flow takes, in characters (src/analysis/prompt.rs, `MAX_QUESTION`),
// and the most files not on the map an update sends (`MAX_NEW_FILES` there).
export const MAX_FLOW_QUESTION = 300;
export const MAX_UPDATE_FILES = 200;

// Why `text` is no flow's question, in words, or null when it is one: 1 to 300 characters once
// trimmed, counted as the route counts them (`chars`, Unicode scalar values), so an emoji is one
// character where `length` would count two.
export function checkFlowQuestion(text) {
  const length = [...text.trim()].length;
  if (length === 0) return "Ask a question: how does something happen?";
  if (length > MAX_FLOW_QUESTION) {
    return `A flow's question is at most ${MAX_FLOW_QUESTION} characters, not ${length}.`;
  }
  return null;
}

// What the map's header says of `diagram`: its node count, how many are unverified and
// outdated, and what its runs left out, in words, or null when nothing was.
export function diagramCounts(diagram) {
  const cards = diagram.nodes.map(nodeState);
  return {
    total: cards.length,
    unverified: cards.filter((state) => state === "unverified").length,
    outdated: cards.filter((state) => state === "outdated").length,
    dropped: droppedText(diagram.dropped),
  };
}

const plural = (count, word) => `${count} ${word}${count === 1 ? "" : "s"}`;

// `dropped` (src/analysis.rs, `Dropped`) in words: "Left out 2 nodes and 1 edge; 3 texts
// shortened; the reply was cut", or null when it is absent or all zero.
export function droppedText(dropped) {
  if (!dropped) return null;
  const left = [
    dropped.nodes && plural(dropped.nodes, "node"),
    dropped.edges && plural(dropped.edges, "edge"),
    dropped.groups && plural(dropped.groups, "group"),
  ].filter(Boolean);
  const parts = [];
  if (left.length) parts.push(`Left out ${left.join(", ").replace(/, ([^,]*)$/, " and $1")}`);
  if (dropped.shortened) parts.push(`${plural(dropped.shortened, "text")} shortened`);
  if (dropped.replyCut) parts.push("the reply was cut to its last 2 MiB");
  return parts.length ? parts.join("; ") : null;
}

// The ids of `diagram`'s nodes that `query` matches, case-insensitively, best first: those
// whose label starts with it, then those whose label holds it, then those whose summary does.
// Each group keeps the diagram's order.
export function searchNodes(diagram, query) {
  const wanted = query.trim().toLowerCase();
  if (!wanted) return [];
  const ranked = [[], [], []];
  for (const node of diagram.nodes) {
    const label = node.label.toLowerCase();
    if (label.startsWith(wanted)) ranked[0].push(node.id);
    else if (label.includes(wanted)) ranked[1].push(node.id);
    else if (node.summary.toLowerCase().includes(wanted)) ranked[2].push(node.id);
  }
  return ranked.flat();
}

// The id of the diagram node `nodeId` (`D2.3`) is in (`D2`).
export const diagramOf = (nodeId) => nodeId.slice(0, nodeId.indexOf("."));

// The diagrams from the top level down to `diagram`, through each one's parent node, as
// `[{ id, title }]`. A parent missing from `analysis` ends the walk; the file's check rules out
// loops (src/analysis.rs, "Invariants"), and a seen diagram ends it too.
export function breadcrumb(analysis, diagram) {
  const byId = new Map(analysis.diagrams.map((each) => [each.id, each]));
  const trail = [];
  const seen = new Set();
  for (let at = diagram; at && !seen.has(at.id); at = at.parent && byId.get(diagramOf(at.parent))) {
    seen.add(at.id);
    trail.unshift({ id: at.id, title: at.title });
  }
  return trail;
}

// The diagram under node `nodeId`, the one its dialog's Open button opens, or null.
export function childDiagram(analysis, nodeId) {
  return analysis.diagrams.find((diagram) => diagram.parent === nodeId) ?? null;
}

const clamp = (value, min, max) => Math.min(Math.max(value, min), max);

// The pan and zoom that shows all of `positioned` centred in `viewport` (`{ width, height }`
// in CSS px), as `{ x, y, scale }`: the diagram's (0, 0) drawn at (x, y). Never above zoom 1,
// so a small diagram is not blown up.
export function fitTransform(positioned, viewport) {
  const fits = Math.min(viewport.width / positioned.width, viewport.height / positioned.height);
  const scale = clamp(fits, ZOOM_MIN, 1);
  return {
    x: (viewport.width - positioned.width * scale) / 2,
    y: (viewport.height - positioned.height * scale) / 2,
    scale,
  };
}

// `transform` with its zoom within ZOOM_MIN to ZOOM_MAX and its pan moved, if need be, so a
// node's centre is in `viewport`: no pan or zoom loses the whole diagram off screen. The node
// brought back is the nearest one, to its nearest edge of the viewport.
export function clampTransform(positioned, viewport, transform) {
  const scale = clamp(transform.scale, ZOOM_MIN, ZOOM_MAX);
  let best = null;
  for (const box of [...positioned.nodes, ...positioned.groups]) {
    const cx = transform.x + (box.x + box.width / 2) * scale;
    const cy = transform.y + (box.y + box.height / 2) * scale;
    const dx = clamp(cx, 0, viewport.width) - cx;
    const dy = clamp(cy, 0, viewport.height) - cy;
    if (dx === 0 && dy === 0) return { ...transform, scale };
    if (!best || Math.hypot(dx, dy) < Math.hypot(best.dx, best.dy)) best = { dx, dy };
  }
  if (!best) return { ...transform, scale };
  return { x: transform.x + best.dx, y: transform.y + best.dy, scale };
}

// `transform` zoomed by `factor` about the viewport point `at` (`{ x, y }`), which stays where
// it is on screen, within the zoom limits.
export function zoomAt(transform, factor, at) {
  const scale = clamp(transform.scale * factor, ZOOM_MIN, ZOOM_MAX);
  const ratio = scale / transform.scale;
  return { x: at.x - (at.x - transform.x) * ratio, y: at.y - (at.y - transform.y) * ratio, scale };
}

// The pan that puts the centre of `box` at the centre of `viewport`, at `transform`'s zoom.
export function centreOn(box, viewport, transform) {
  return {
    x: viewport.width / 2 - (box.x + box.width / 2) * transform.scale,
    y: viewport.height / 2 - (box.y + box.height / 2) * transform.scale,
    scale: transform.scale,
  };
}

// Where code reference `anchor` points, whole: "src/a.rs:3-5", "src/a.rs", or "src/" for a
// folder; an unverified one, which has no target, by its path alone.
export function anchorPlace(anchor) {
  if (anchor.target === "folder") return `${anchor.path}/`;
  if (anchor.target !== "lines") return anchor.path;
  const { start, end } = anchor.lines;
  return `${anchor.path}:${start === end ? start : `${start}-${end}`}`;
}

// The notes of `notes` (`api/notes`) on the code `anchors` point at: on lines that overlap a
// reference's lines, anywhere in a file or folder a reference names whole, or on a whole file
// or folder a reference is in. Unverified references point at nothing.
export function notesOn(notes, anchors) {
  const placed = anchors.filter((anchor) => anchor.state !== "unverified");
  const within = (path, folder) => path === folder || path.startsWith(`${folder}/`);
  const touches = (note, anchor) => {
    if (anchor.target === "folder") return within(note.path, anchor.path);
    if (note.target === "folder") return within(anchor.path, note.path);
    if (note.path !== anchor.path) return false;
    if (anchor.target !== "lines" || note.target !== "lines") return true;
    return note.lines.start <= anchor.lines.end && anchor.lines.start <= note.lines.end;
  };
  return notes.filter((note) => placed.some((anchor) => touches(note, anchor)));
}

// ---- The link shown in the code (docs/review-assist.md) ------------------------------------

// Whether code reference `anchor` was placed in the code: found, or outdated at its last lines
// (src/analysis.rs, `place`). An unverified one points at nothing.
const placed = (anchor) => anchor.state === "found" || anchor.state === "outdated";

// The one of `anchors` that 1-based `line` shows: of the placed references to lines that
// cover it, the smallest range; of equal ranges, the deepest under the top level (`depth`,
// 0 when absent); of those, the first. Null when none covers it.
export function innermost(anchors, line) {
  let best = null;
  for (const anchor of anchors) {
    if (anchor.target !== "lines" || !placed(anchor)) continue;
    const { start, end } = anchor.lines;
    if (line < start || end < line) continue;
    const span = end - start;
    const depth = anchor.depth ?? 0;
    if (!best || span < best.span || (span === best.span && depth > best.depth)) {
      best = { anchor, span, depth };
    }
  }
  return best?.anchor ?? null;
}

// The stripes and chips of the file at `path`, `lineCount` lines long, from `analysis` (`GET
// api/analysis`), as a Map from each covered 0-based line to `{ diagramId, nodeId, label,
// outdated, step, chip, key }`:
// - the node is the innermost (`innermost`) of the Architecture kind on that line, else of any
//   kind, and `outdated` says its reference is;
// - `chip`, on the first line a reference of the node shows, unless the line before shows the
//   same node, is `{ diagramId, nodeId, label, others }`, `others` the innermost node on that
//   line of each other diagram, `{ diagramId, diagramTitle, nodeId, label }`, else null;
// - `step` is the step number of a card of the flow `openDiagramId` on the first line of its
//   first reference in the file, else null;
// - `key` changes whenever anything above does, so a row compares it to redraw.
// A reference past the end of a file that shrank draws only up to its end.
export function fileMarks(analysis, path, lineCount, { openDiagramId = null } = {}) {
  const marks = new Map();
  if (!analysis || !path || lineCount === 0) return marks;

  // The references on each line, each with its node, its diagram and the diagram's depth.
  const byLine = new Map();
  const steps = new Map();
  for (const diagram of analysis.diagrams) {
    const depth = breadcrumb(analysis, diagram).length - 1;
    const open = diagram.id === openDiagramId && diagram.kind === "flow";
    const parents = boundaries(diagram);
    let step = 0;
    for (const node of diagram.nodes) {
      if (open && !parents.has(node.id)) step += 1;
      let first = true;
      for (const anchor of node.anchors) {
        if (anchor.path !== path || anchor.target !== "lines" || !placed(anchor)) continue;
        // Past the end of a file that shrank: nothing to draw, and no step to number.
        if (anchor.lines.start > lineCount) continue;
        if (open && first && !parents.has(node.id)) steps.set(anchor.lines.start - 1, step);
        first = false;
        const cited = { ...anchor, depth, diagram, node };
        const last = Math.min(anchor.lines.end, lineCount);
        for (let line = anchor.lines.start; line <= last; line += 1) {
          if (!byLine.has(line)) byLine.set(line, []);
          byLine.get(line).push(cited);
        }
      }
    }
  }

  // The references that won a line already: each is chipped the first time it wins one, which
  // for an outer node whose first lines an inner one wins is below them.
  const shown = new Set();
  const lines = [...byLine.keys()].sort((a, b) => a - b);
  for (const line of lines) {
    const cited = byLine.get(line);
    const architecture = cited.filter((each) => each.diagram.kind === "architecture");
    const winner = innermost(architecture, line) ?? innermost(cited, line);
    const before = marks.get(line - 2);
    // A node's second reference right after its first continues its stripe, unchipped.
    const opens = !shown.has(winner) && before?.nodeId !== winner.node.id;
    shown.add(winner);
    const chip = opens ? chipOf(winner, cited, line) : null;
    const mark = {
      diagramId: winner.diagram.id,
      nodeId: winner.node.id,
      label: winner.node.label,
      outdated: winner.state === "outdated",
      step: steps.get(line - 1) ?? null,
      chip,
    };
    const others = chip?.others.map((other) => other.nodeId) ?? null;
    mark.key = JSON.stringify([mark.nodeId, mark.label, mark.outdated, mark.step, others]);
    marks.set(line - 1, mark);
  }
  return marks;
}

// The chip of `winner` on `line`: its node, and the innermost node of each other diagram among
// the references `cited` there, in the file's order of diagrams.
function chipOf(winner, cited, line) {
  const others = [];
  const seen = new Set([winner.diagram.id]);
  for (const { diagram } of cited) {
    if (seen.has(diagram.id)) continue;
    seen.add(diagram.id);
    const inner = innermost(
      cited.filter((each) => each.diagram === diagram),
      line,
    );
    others.push({
      diagramId: diagram.id,
      diagramTitle: diagram.title,
      nodeId: inner.node.id,
      label: inner.node.label,
    });
  }
  return { diagramId: winner.diagram.id, nodeId: winner.node.id, label: winner.node.label, others };
}

// The id of the node of `diagram` that 1-based `line` of the file at `path` stands in: the one
// whose referenced lines are innermost there, else the first that references the whole file,
// else null. A folder's node rings nothing: every line of every file in it would ring it.
export function nodeAtLine(diagram, path, line) {
  const cited = diagram.nodes.flatMap((node) =>
    node.anchors.filter((anchor) => anchor.path === path).map((anchor) => ({ ...anchor, node })),
  );
  const inner = innermost(cited, line);
  if (inner) return inner.node.id;
  const whole = cited.find((anchor) => anchor.target === "file" && placed(anchor));
  return whole?.node.id ?? null;
}

// The arrow keys' directions, as the unit step along x and y.
const DIRECTIONS = {
  ArrowLeft: { x: -1, y: 0 },
  ArrowRight: { x: 1, y: 0 },
  ArrowUp: { x: 0, y: -1 },
  ArrowDown: { x: 0, y: 1 },
};

// The id of the node of `boxes` an arrow `key` moves to from node `fromId`: of those whose
// centre lies beyond it that way, the one nearest along the arrow, a step aside counting
// twice. Null when none lies that way, or for another key. From no node, the first.
export function nearestNode(boxes, fromId, key) {
  const direction = DIRECTIONS[key];
  if (!direction) return null;
  const from = boxes.find((box) => box.id === fromId);
  if (!from) return boxes[0]?.id ?? null;
  const centre = (box) => ({ x: box.x + box.width / 2, y: box.y + box.height / 2 });
  const origin = centre(from);
  let best = null;
  for (const box of boxes) {
    if (box === from) continue;
    const at = centre(box);
    const along = (at.x - origin.x) * direction.x + (at.y - origin.y) * direction.y;
    if (along <= 0) continue;
    const aside = Math.abs((at.x - origin.x) * direction.y + (at.y - origin.y) * direction.x);
    const cost = along + 2 * aside;
    if (!best || cost < best.cost) best = { id: box.id, cost };
  }
  return best?.id ?? null;
}

// The id of the node an arrow `key` moves to from `fromId` in `positioned` (`layoutDiagram`):
// among the cards and the boundaries, whose labels take focus as cards do, so an arrow from a
// boundary goes to what lies that way of it.
export function arrowTarget(positioned, fromId, key) {
  return nearestNode([...positioned.nodes, ...positioned.groups], fromId, key);
}

// Why analysis run `run` (`GET api/run`) stored nothing, in words, or null when it did or is
// still under way: the problem the server read from its reply (`noAnalysis`), or, for a run
// that failed (the agent did not start, refused, or exited), its `failed` entry's error.
export function runProblem(run) {
  if (run.problem) return run.problem;
  if (run.status !== "failed") return null;
  const failed = run.entries.findLast((entry) => entry.kind === "failed");
  return failed?.error ?? "The analysis run failed.";
}
