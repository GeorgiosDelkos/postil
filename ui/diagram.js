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
// diagram. Top-to-bottom fits the narrow map pane beside the code without forcing heavy zoom-out.
const SPACING = { nodesep: 28, ranksep: 48, marginx: 24, marginy: 24 };
// Padding inside a boundary box around its laid-out children, with room at the top for its label.
const GROUP_PAD = 16;
const GROUP_HEADER = 30;

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
// `diagram.edges`, or null for one crossing boundaries, which is drawn straight.
//
// Laid out bottom-up with plain (non-compound) dagre graphs, one per boundary and one at the
// top level:
// 1. dagre 3.1.3 throws "Not possible to find intersection inside of the rectangle" on compound
//    graphs with cross-boundary edges. Non-compound graphs never hit that bug.
// 2. Lifting cross-boundary edges to their boundary ancestors lets dagre rank boundaries and
//    external nodes/stores properly top-to-bottom instead of piling unranked nodes to one side.
// 3. Each boundary gets dedicated header padding so its label never collides with its cards.
function layoutWith(diagram, dagre) {
  const parents = boundaries(diagram);
  const group = new Map(diagram.nodes.map((node) => [node.id, node.group ?? null]));
  const childrenOf = new Map([[null, []]]);
  for (const node of diagram.nodes) {
    const parent = node.group ?? null;
    if (!childrenOf.has(parent)) childrenOf.set(parent, []);
    childrenOf.get(parent).push(node);
  }
  const card = (id) => !parents.has(id);

  // Returns the direct child of `container` that is `id` or contains `id`, or null if `id` is
  // outside `container`.
  const childAt = (id, container) => {
    let at = id;
    while (at != null) {
      const parent = group.get(at) ?? null;
      if (parent === container) return at;
      at = parent;
    }
    return null;
  };

  // Recursively lays out `container` (null for the diagram root) and returns
  // `{ width, height, boxes, routed }`, where `boxes` maps every descendant node id to its
  // box relative to `container`'s top-left, and `routed` maps edge indices laid out inside
  // `container` to their point arrays.
  function layoutLevel(container) {
    const members = childrenOf.get(container) ?? [];
    if (members.length === 0) {
      return { width: 0, height: 0, boxes: new Map(), routed: new Map() };
    }

    // First lay out any child boundaries so we know their exact dimensions.
    const subLayouts = new Map();
    const graph = new dagre.graphlib.Graph({ compound: false, multigraph: true });
    const margins = container === null ? SPACING : { ...SPACING, marginx: 0, marginy: 0 };
    graph.setGraph({ rankdir: "TB", ...margins });

    for (const node of members) {
      if (parents.has(node.id)) {
        const sub = layoutLevel(node.id);
        subLayouts.set(node.id, sub);
        graph.setNode(node.id, {
          width: Math.max(NODE_WIDTH, sub.width + GROUP_PAD * 2),
          height: Math.max(NODE_HEIGHT, sub.height + GROUP_HEADER + GROUP_PAD),
        });
      } else {
        graph.setNode(node.id, { width: NODE_WIDTH, height: NODE_HEIGHT });
      }
    }

    const laid = new Map();
    const lifted = new Set();
    diagram.edges.forEach((edge, index) => {
      const from = childAt(edge.from, container);
      const to = childAt(edge.to, container);
      if (!from || !to || from === to) return;
      if (from === edge.from && to === edge.to && card(from) && card(to)) {
        laid.set(index, String(index));
        graph.setEdge(from, to, {}, String(index));
      } else {
        // Cross-boundary edge (or edge touching a boundary): add one proxy edge per ordered pair
        // so dagre ranks the boundaries and external nodes cleanly relative to each other.
        const key = `${from}\0${to}`;
        if (!lifted.has(key)) {
          lifted.add(key);
          graph.setEdge(from, to, {}, `lift:${key}`);
        }
      }
    });

    if (diagram.kind === "flow") {
      // A trace's steps are in step order: each step sits below the one before, by an edge only
      // the layout sees, so steps the agent did not join still read top to bottom — including
      // when consecutive steps sit in different boundaries.
      const allCards = diagram.nodes.filter((node) => card(node.id));
      const stepped = new Set();
      for (let index = 1; index < allCards.length; index += 1) {
        const from = childAt(allCards[index - 1].id, container);
        const to = childAt(allCards[index].id, container);
        if (!from || !to || from === to) continue;
        const key = `${from}\0${to}`;
        if (!stepped.has(key)) {
          stepped.add(key);
          graph.setEdge(from, to, {}, `step:${key}`);
        }
      }
    }

    dagre.layout(graph);

    const boxes = new Map();
    const routed = new Map();
    for (const node of members) {
      const box = graph.node(node.id);
      const x = box.x - box.width / 2;
      const y = box.y - box.height / 2;
      boxes.set(node.id, { x, y, width: box.width, height: box.height });
      const sub = subLayouts.get(node.id);
      if (sub) {
        const ox = x + GROUP_PAD;
        const oy = y + GROUP_HEADER;
        for (const [childId, childBox] of sub.boxes) {
          boxes.set(childId, {
            x: childBox.x + ox,
            y: childBox.y + oy,
            width: childBox.width,
            height: childBox.height,
          });
        }
        for (const [edgeIndex, pts] of sub.routed) {
          routed.set(
            edgeIndex,
            pts.map((pt) => ({ x: pt.x + ox, y: pt.y + oy })),
          );
        }
      }
    }

    for (const [edgeIndex, name] of laid) {
      const edge = diagram.edges[edgeIndex];
      routed.set(edgeIndex, graph.edge({ v: edge.from, w: edge.to, name }).points);
    }

    const { width, height } = graph.graph();
    return { width, height, boxes, routed };
  }

  const root = layoutLevel(null);
  const points = diagram.edges.map((_, index) => root.routed.get(index) ?? null);
  return { width: root.width, height: root.height, boxes: root.boxes, points };
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
    return `A trace's question is at most ${MAX_FLOW_QUESTION} characters, not ${length}.`;
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
  let bestDistSq = Infinity;
  const check = (list) => {
    for (const box of list) {
      const cx = transform.x + (box.x + box.width / 2) * scale;
      const cy = transform.y + (box.y + box.height / 2) * scale;
      const dx = clamp(cx, 0, viewport.width) - cx;
      const dy = clamp(cy, 0, viewport.height) - cy;
      if (dx === 0 && dy === 0) return true;
      const distSq = dx * dx + dy * dy;
      if (distSq < bestDistSq) {
        bestDistSq = distSq;
        best = { dx, dy };
      }
    }
    return false;
  };
  if (check(positioned.nodes) || check(positioned.groups) || !best) return { ...transform, scale };
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

// ---- The change map (docs/review-assist-plan.md, 8.6) -------------------------------------

// Whether code reference `anchor`, placed at the run's fork (`GET api/analysis?at=`), is in
// the change `hunk` (ui/patch.js) of the changed `file`, or in the file's change when `hunk` is
// null (a binary file, or one whose mode alone changed). The anchor's lines are of the old
// side, so only the hunk's old side is compared. A deleted file is a change of every
// reference placed in it, wherever its lines were; an added file had none. A folder is not
// touched by the files in it: every change under `src` would light up its boundary.
function anchorTouchesHunk(anchor, file, hunk) {
  if (file.status === "added" || anchor.state === "unverified") return false;
  if (anchor.target === "folder" || anchor.path !== file.path) return false;
  if (file.status === "deleted") return true;
  // An outdated reference's lines are where its code last was, not where it is at the fork.
  if (anchor.state !== "found") return false;
  if (anchor.target === "file") return true;
  if (!hunk || hunk.oldCount === 0) return false;
  const last = hunk.oldStart + hunk.oldCount - 1;
  return anchor.lines.start <= last && hunk.oldStart <= anchor.lines.end;
}

// Whether a reference of `node` is in the change `hunk` of `file` (`anchorTouchesHunk`): what a
// click on a touched node scrolls the changes view to.
export function nodeTouchesHunk(node, file, hunk) {
  return node.anchors.some((anchor) => anchorTouchesHunk(anchor, file, hunk));
}

function nodeTouchesFile(node, file) {
  if (file.hunks.length === 0) return nodeTouchesHunk(node, file, null);
  return file.hunks.some((hunk) => nodeTouchesHunk(node, file, hunk));
}

// What the changed `files` of a run (ui/patch.js, `parsePatch`) touch on the Architecture
// diagrams of `analysis`, placed at the run's fork: `{ nodes, near, unmapped }`, `nodes` the
// ids of the touched nodes, `near` those one edge from a touched node and not touched
// themselves, `unmapped` the paths, in the diff's order, that no Architecture node covers (one
// a file the run added, which no node can have cited). Traces and User Flow diagrams are left
// out: the change map is drawn on the Architecture.
export function touched(analysis, files) {
  const diagrams = (analysis?.diagrams ?? []).filter((diagram) => diagram.kind === "architecture");
  const nodes = new Set();
  for (const diagram of diagrams) {
    for (const node of diagram.nodes) {
      if (files.some((file) => nodeTouchesFile(node, file))) nodes.add(node.id);
    }
  }
  const near = new Set();
  for (const diagram of diagrams) {
    for (const { from, to } of diagram.edges) {
      if (nodes.has(from) && !nodes.has(to)) near.add(to);
      if (nodes.has(to) && !nodes.has(from)) near.add(from);
    }
  }
  const placedAnchors = diagrams.flatMap((diagram) =>
    diagram.nodes.flatMap((node) => node.anchors.filter((anchor) => anchor.state !== "unverified")),
  );
  const covers = (anchor, path) =>
    anchor.path === path || (anchor.target === "folder" && path.startsWith(`${anchor.path}/`));
  const unmapped = files
    .filter(
      (file) =>
        file.status === "added" || !placedAnchors.some((anchor) => covers(anchor, file.path)),
    )
    .map((file) => file.path);
  return { nodes, near, unmapped };
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

  // Precompute diagram depths once (O(D)) instead of rebuilding the diagram map per diagram.
  const diagramById = new Map(analysis.diagrams.map((d) => [d.id, d]));
  const depthOf = (diagram) => {
    let depth = 0;
    const seen = new Set();
    for (
      let at = diagram;
      at && !seen.has(at.id);
      at = at.parent ? diagramById.get(diagramOf(at.parent)) : null
    ) {
      seen.add(at.id);
      depth += 1;
    }
    return depth - 1;
  };

  // The references on each line, each with its node, its diagram and the diagram's depth.
  const byLine = new Map();
  const steps = new Map();
  for (const diagram of analysis.diagrams) {
    const depth = depthOf(diagram);
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
    const others = chip?.others.map((other) => other.nodeId).join(",") ?? "";
    mark.key = `${mark.nodeId}\0${mark.label}\0${mark.outdated ? 1 : 0}\0${mark.step ?? ""}\0${others}`;
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
