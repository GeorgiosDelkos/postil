import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { runInNewContext } from "node:vm";

import {
  anchorPlace,
  arrowTarget,
  NODE_HEIGHT,
  NODE_WIDTH,
  ZOOM_MAX,
  ZOOM_MIN,
  breadcrumb,
  checkFlowQuestion,
  clampTransform,
  diagramCounts,
  fileMarks,
  fitTransform,
  innermost,
  layoutDiagram,
  nearestNode,
  nodeAtLine,
  nodeState,
  notesOn,
  runProblem,
  searchNodes,
  zoomAt,
} from "./diagram.js";

// The vendored dagre, as the page's script element runs it: it defines `dagre`.
const dagre = runInNewContext(
  `${readFileSync(new URL("vendor/dagre.min.js", import.meta.url), "utf8")};dagre`,
  { structuredClone },
);

const found = { path: "src/a.rs", target: "file", state: "found" };
const node = (id, extra = {}) => ({
  id,
  kind: "component",
  label: id,
  summary: "",
  detail: "",
  anchors: [found],
  ...extra,
});
const diagram = (nodes, edges = [], extra = {}) => ({
  id: "D1",
  kind: "architecture",
  title: "System",
  run: "R1",
  nodes,
  edges,
  ...extra,
});
const edge = (from, to) => ({ from, to, label: "", anchor: null });
const finite = (box) => [box.x, box.y, box.width, box.height].every(Number.isFinite);
const inside = (inner, outer) =>
  inner.x >= outer.x &&
  inner.y >= outer.y &&
  inner.x + inner.width <= outer.x + outer.width &&
  inner.y + inner.height <= outer.y + outer.height;

test("one node with no edges is a card of the fixed size", () => {
  const laid = layoutDiagram(diagram([node("D1.1")]), dagre, new Map());
  assert.equal(laid.nodes.length, 1);
  assert.equal(laid.nodes[0].width, NODE_WIDTH);
  assert.equal(laid.nodes[0].height, NODE_HEIGHT);
  assert.ok(finite(laid.nodes[0]));
  assert.ok(laid.width >= NODE_WIDTH && laid.height >= NODE_HEIGHT);
});

test("a boundary with children is drawn around them, and one without is a card", () => {
  const boundary = (id) => node(id, { kind: "boundary" });
  const nodes = [
    boundary("D1.1"),
    node("D1.2", { group: "D1.1" }),
    node("D1.3", { group: "D1.1" }),
    boundary("D1.4"),
    node("D1.5"),
  ];
  const laid = layoutDiagram(
    diagram(nodes, [edge("D1.2", "D1.5"), edge("D1.5", "D1.1")]),
    dagre,
    new Map(),
  );
  assert.deepEqual(
    laid.groups.map((group) => group.id),
    ["D1.1"],
  );
  const group = laid.byId.get("D1.1");
  assert.ok(inside(laid.byId.get("D1.2"), group) && inside(laid.byId.get("D1.3"), group));
  // dagre gives a childless compound node no size: it must be a card here.
  const lone = laid.byId.get("D1.4");
  assert.ok(laid.nodes.includes(lone));
  assert.equal(lone.width, NODE_WIDTH);
  assert.ok(finite(lone));
  // The edge to the boundary is drawn too, from the card's border to the boundary's.
  const toGroup = laid.edges.find(({ edge }) => edge.to === "D1.1");
  assert.equal(toGroup.points.length, 2);
  assert.ok(toGroup.points.every((point) => Number.isFinite(point.x) && Number.isFinite(point.y)));
});

test("200 nodes and 260 edges lay out, with edges across boundaries drawn straight", () => {
  const nodes = Array.from({ length: 200 }, (_, index) => node(`D1.${index + 1}`));
  // Groups of ten, each in a boundary of its own, as a large repository's processes. Most
  // edges cross boundaries: given to dagre 3.1.3, they make it throw (diagram.js).
  for (let index = 0; index < 200; index += 1) {
    if (index % 10 !== 0) nodes[index].group = `D1.${index - (index % 10) + 1}`;
    else nodes[index].kind = "boundary";
  }
  const leaves = nodes.filter((each) => each.kind !== "boundary").map((each) => each.id);
  const edges = Array.from({ length: 260 }, (_, index) =>
    edge(leaves[index % leaves.length], leaves[(index * 7 + 3) % leaves.length]),
  ).filter(({ from, to }) => from !== to);
  const cache = new Map();
  const big = diagram(nodes, edges);

  const laid = layoutDiagram(big, dagre, cache);
  assert.equal(laid.nodes.length + laid.groups.length, 200);
  assert.ok([...laid.nodes, ...laid.groups].every(finite));
  assert.equal(laid.edges.length, edges.length);
  assert.ok(laid.edges.every(({ points }) => points.length >= 2));
  // An edge across boundaries is drawn straight; one inside a boundary follows dagre's route,
  // which starts and ends on the cards' borders with points between.
  const group = new Map(nodes.map((each) => [each.id, each.group]));
  const across = laid.edges.filter(({ edge }) => group.get(edge.from) !== group.get(edge.to));
  const inside = laid.edges.filter(({ edge }) => group.get(edge.from) === group.get(edge.to));
  assert.ok(across.length > 100 && inside.length > 0, `${across.length} and ${inside.length}`);
  assert.ok(across.every(({ points }) => points.length === 2));
  assert.ok(inside.every(({ points }) => points.length > 2));
});

test("a flow's steps read left to right even where no edge joins them", () => {
  const steps = ["D1.1", "D1.2", "D1.3"].map((id) => node(id));
  const laid = layoutDiagram(diagram(steps, [], { kind: "flow" }), dagre, new Map());
  const xs = steps.map(({ id }) => laid.byId.get(id).x);
  assert.ok(xs[0] < xs[1] && xs[1] < xs[2], `${xs}`);
  assert.equal(laid.edges.length, 0, "the layout's own edges are not drawn");
});

test("search ranks a label that starts with the words, then one that holds them, then a summary", () => {
  const nodes = [
    node("D1.1", { label: "Store", summary: "keeps the run log" }),
    node("D1.2", { label: "Runner", summary: "starts agents" }),
    node("D1.3", { label: "Run log", summary: "" }),
    node("D1.4", { label: "Page", summary: "draws the run" }),
  ];
  assert.deepEqual(searchNodes(diagram(nodes), "RUN"), ["D1.2", "D1.3", "D1.1", "D1.4"]);
  assert.deepEqual(searchNodes(diagram(nodes), "log"), ["D1.3", "D1.1"]);
  assert.deepEqual(searchNodes(diagram(nodes), "  "), []);
});

test("a node no reference of which was found is unverified, and one gone is outdated", () => {
  const unverified = { path: "x.rs", state: "unverified" };
  const outdated = { ...found, state: "outdated" };
  assert.equal(nodeState(node("D1.1", { anchors: [] })), "unverified");
  assert.equal(nodeState(node("D1.1", { anchors: [unverified] })), "unverified");
  assert.equal(nodeState(node("D1.1", { anchors: [unverified, found] })), "found");
  assert.equal(nodeState(node("D1.1", { anchors: [found, outdated] })), "outdated");

  const nodes = [
    node("D1.1", { anchors: [unverified] }),
    node("D1.2", { anchors: [outdated] }),
    node("D1.3"),
  ];
  const dropped = { nodes: 2, edges: 1, groups: 0, shortened: 3, replyCut: true };
  assert.deepEqual(diagramCounts(diagram(nodes, [], { dropped })), {
    total: 3,
    unverified: 1,
    outdated: 1,
    dropped: "Left out 2 nodes and 1 edge; 3 texts shortened; the reply was cut to its last 2 MiB",
  });
  assert.equal(diagramCounts(diagram(nodes)).dropped, null, "a version 1 diagram has none");
});

test("the breadcrumb follows parent nodes up to the top level", () => {
  const top = diagram([node("D1.1")]);
  const middle = { ...diagram([node("D2.1")]), id: "D2", title: "Inside", parent: "D1.1" };
  const deep = { ...diagram([]), id: "D3", title: "Deeper", parent: "D2.1" };
  const analysis = { diagrams: [top, middle, deep] };
  assert.deepEqual(breadcrumb(analysis, deep), [
    { id: "D1", title: "System" },
    { id: "D2", title: "Inside" },
    { id: "D3", title: "Deeper" },
  ]);
  const orphan = { ...deep, parent: "D9.1" };
  assert.deepEqual(breadcrumb(analysis, orphan), [{ id: "D3", title: "Deeper" }]);
});

test("zoom stays within 25% and 200%, and no pan loses every node", () => {
  const laid = layoutDiagram(
    diagram([node("D1.1"), node("D1.2")], [edge("D1.1", "D1.2")]),
    dagre,
    new Map(),
  );
  const viewport = { width: 400, height: 300 };
  assert.equal(clampTransform(laid, viewport, { x: 0, y: 0, scale: 0.1 }).scale, ZOOM_MIN);
  assert.equal(clampTransform(laid, viewport, { x: 0, y: 0, scale: 9 }).scale, ZOOM_MAX);
  assert.equal(zoomAt({ x: 0, y: 0, scale: 1.5 }, 4, { x: 0, y: 0 }).scale, ZOOM_MAX);

  for (const scale of [ZOOM_MIN, 1, ZOOM_MAX]) {
    for (const [x, y] of [
      [-1e5, 0],
      [1e5, 1e5],
      [0, -1e5],
      [-1e5, -1e5],
    ]) {
      const kept = clampTransform(laid, viewport, { x, y, scale });
      const shown = laid.nodes.some((box) => {
        const cx = kept.x + (box.x + box.width / 2) * kept.scale;
        const cy = kept.y + (box.y + box.height / 2) * kept.scale;
        return cx >= 0 && cx <= viewport.width && cy >= 0 && cy <= viewport.height;
      });
      assert.ok(shown, `${scale} at ${x}, ${y}`);
    }
  }
  // A pan that keeps a node in view is left alone.
  const fitted = fitTransform(laid, viewport);
  assert.deepEqual(clampTransform(laid, viewport, fitted), fitted);
  assert.ok(fitted.scale <= 1, "a small diagram is not blown up");
});

test("zooming about a point keeps that point still", () => {
  const before = { x: 10, y: 20, scale: 1 };
  const after = zoomAt(before, 2, { x: 110, y: 70 });
  // The diagram point under (110, 70) was (100, 50); at zoom 2 it is drawn at the same place.
  assert.deepEqual(after, { x: -90, y: -30, scale: 2 });
});

test("the arrows move to the nearest node that way, and nowhere past the last", () => {
  const box = (id, x, y) => ({ id, x, y, width: 10, height: 10 });
  // B right of A, C below A, D far right and a little down.
  const boxes = [box("A", 0, 0), box("B", 100, 0), box("C", 0, 100), box("D", 300, 40)];
  assert.equal(nearestNode(boxes, "A", "ArrowRight"), "B");
  assert.equal(nearestNode(boxes, "A", "ArrowDown"), "C");
  assert.equal(nearestNode(boxes, "B", "ArrowRight"), "D");
  assert.equal(nearestNode(boxes, "B", "ArrowLeft"), "A");
  assert.equal(nearestNode(boxes, "A", "ArrowLeft"), null);
  assert.equal(nearestNode(boxes, "A", "ArrowUp"), null);
  assert.equal(nearestNode(boxes, null, "ArrowRight"), "A", "from no node, the first");
});

test("a node's notes are those on the code its found references point at", () => {
  const lines = (start, end) => ({ path: "src/a.rs", target: "lines", lines: { start, end } });
  const notes = [
    { id: "N1", ...lines(1, 2) },
    { id: "N2", ...lines(5, 6) },
    { id: "N3", path: "src/a.rs", target: "file" },
    { id: "N4", path: "src", target: "folder" },
    { id: "N5", path: "src/ab.rs", target: "file" },
  ];
  const ids = (anchors) => notesOn(notes, anchors).map((note) => note.id);
  assert.deepEqual(ids([{ ...lines(2, 4), state: "found" }]), ["N1", "N3", "N4"]);
  assert.deepEqual(ids([{ path: "src/a.rs", target: "file", state: "outdated" }]), [
    "N1",
    "N2",
    "N3",
    "N4",
  ]);
  assert.deepEqual(ids([{ path: "src", target: "folder", state: "found" }]), [
    "N1",
    "N2",
    "N3",
    "N4",
    "N5",
  ]);
  assert.deepEqual(
    ids([{ path: "src/a.rs", state: "unverified" }]),
    [],
    "an unverified one points at nothing",
  );
  assert.equal(anchorPlace(lines(3, 3)), "src/a.rs:3");
  assert.equal(anchorPlace({ path: "src", target: "folder" }), "src/");
});

test("a layout is kept per diagram and run, and each call gives that call's nodes", () => {
  // A dagre that counts its layouts, so the test sees whether one ran again.
  let layouts = 0;
  const counting = { ...dagre, layout: (graph) => (layouts += 1) && dagre.layout(graph) };
  const at = (line) => ({ path: "src/a.rs", target: "lines", lines: { start: line, end: line } });
  const before = diagram([node("D1.1", { anchors: [{ ...at(2), state: "found" }] })]);
  const cache = new Map();
  const first = layoutDiagram(before, counting, cache);

  // The next read placed the reference where its code moved: same run, new lines.
  const after = diagram([node("D1.1", { anchors: [{ ...at(9), state: "found" }] })]);
  const second = layoutDiagram(after, counting, cache);
  assert.equal(layouts, 1, "not laid out again");
  assert.equal(second.byId.get("D1.1").node.anchors[0].lines.start, 9, "the new node");
  assert.equal(second.byId.get("D1.1").x, first.byId.get("D1.1").x, "where it was");

  layoutDiagram({ ...after, run: "R2" }, counting, cache);
  assert.equal(layouts, 2, "a new run is laid out");
});

test("an update under the same run lays the diagram out again when its shape changed", () => {
  let layouts = 0;
  const counting = { ...dagre, layout: (graph) => (layouts += 1) && dagre.layout(graph) };
  const cache = new Map();
  const three = [node("D1.1"), node("D1.2"), node("D1.3")];
  layoutDiagram(diagram(three, [edge("D1.1", "D1.2")]), counting, cache);

  // An update keeps the run: it adds a node, and swaps one edge for another, so the count of
  // edges stays and a cached route would be drawn for the wrong edge.
  const updated = diagram([...three, node("D1.4")], [edge("D1.1", "D1.3")]);
  const laid = layoutDiagram(updated, counting, cache);
  assert.equal(layouts, 2, "laid out again");
  assert.ok(laid.byId.get("D1.4").width > 0, "the added node has a box");
  const same = diagram([...three, node("D1.4")], [edge("D1.1", "D1.3")]);
  layoutDiagram(same, counting, cache);
  assert.equal(layouts, 2, "the same shape is not laid out again");

  // The same nodes and as many edges, one of them another: its route is not the old one's.
  const swapped = diagram([...three, node("D1.4")], [edge("D1.2", "D1.4")]);
  layoutDiagram(swapped, counting, cache);
  assert.equal(layouts, 3, "an edge changed");
});

test("a flow with nested boundaries and an edge beside a step lays out", () => {
  // The case where a weightless step edge beside the real one made dagre 3.1.3 throw.
  const boundary = (id, group) => node(id, { kind: "boundary", group });
  const nodes = [
    node("D1.1"),
    boundary("D1.2"),
    node("D1.3", { group: "D1.2" }),
    node("D1.4", { group: "D1.2" }),
    boundary("D1.5"),
    boundary("D1.6", "D1.5"),
    node("D1.7", { group: "D1.6" }),
  ];
  const edges = [edge("D1.7", "D1.4"), edge("D1.3", "D1.4")];
  const laid = layoutDiagram(diagram(nodes, edges, { kind: "flow" }), dagre, new Map());
  assert.ok([...laid.nodes, ...laid.groups].every(finite));
  assert.ok(laid.byId.get("D1.3").x < laid.byId.get("D1.4").x, "the steps in order");
});

test("an arrow from a boundary goes to what lies that way of it, not to the first card", () => {
  const box = (id, x) => ({ id, x, y: 0, width: 100, height: 50 });
  // Card A far left, boundary G holding card C, card B right of G.
  const positioned = {
    nodes: [box("A", -400), box("C", 20), box("B", 400)],
    groups: [{ ...box("G", 0), width: 300, height: 120 }],
  };
  assert.equal(arrowTarget(positioned, "G", "ArrowRight"), "B");
  assert.equal(arrowTarget(positioned, "G", "ArrowLeft"), "C", "its own card, nearer than A");
  assert.equal(arrowTarget(positioned, "B", "ArrowLeft"), "G", "a boundary is reached too");
});

test("a failed analysis run says why, and one that stored its diagrams says nothing", () => {
  const run = (status, entries, problem = null) => ({ status, entries, problem });
  assert.equal(runProblem(run("ended", [], "the analysis was empty")), "the analysis was empty");
  const failed = [{ kind: "started" }, { kind: "failed", error: "the agent exited: no key" }];
  assert.equal(runProblem(run("failed", failed)), "the agent exited: no key");
  assert.equal(runProblem(run("failed", [])), "The analysis run failed.");
  assert.equal(runProblem(run("ended", [{ kind: "ended" }])), null);
  assert.equal(runProblem(run("running", [])), null);
});

// A reference to lines `start` to `end` of `path`, found unless `state` says otherwise.
const cite = (start, end, state = "found", path = "src/a.rs") => ({
  path,
  target: "lines",
  lines: { start, end },
  state,
});

test("innermost picks the smallest range on the line, then the deepest, then the first", () => {
  const at = (depth, start, end, name) => ({ ...cite(start, end), depth, name });
  const name = (anchors, line) => innermost(anchors, line)?.name ?? null;
  const wide = at(2, 1, 10, "wide and deep");
  const narrow = at(0, 3, 4, "narrow at the top");
  assert.equal(name([wide, narrow], 3), "narrow at the top", "depth never beats a smaller range");
  assert.equal(name([wide, narrow], 5), "wide and deep");
  assert.equal(name([wide, narrow], 11), null, "past every range");

  const equal = [at(0, 3, 4, "top"), at(2, 3, 4, "deepest"), at(1, 3, 4, "deeper")];
  assert.equal(name(equal, 3), "deepest", "equal ranges: the diagram deepest under the top");
  assert.equal(name([at(1, 3, 4, "first"), at(1, 3, 4, "second")], 4), "first");

  const disjoint = [at(0, 1, 2, "one"), at(0, 4, 5, "two")];
  assert.equal(name(disjoint, 2), "one");
  assert.equal(name(disjoint, 4), "two");
  assert.equal(name(disjoint, 3), null, "between two ranges");

  const unverified = { ...at(0, 3, 3, "unverified"), state: "unverified" };
  const file = { path: "src/a.rs", target: "file", state: "found", depth: 3, name: "file" };
  assert.equal(name([wide, unverified, file], 3), "wide and deep", "only placed line references");
  const outdated = { ...at(0, 3, 3, "outdated"), state: "outdated" };
  assert.equal(name([wide, outdated], 3), "outdated", "an outdated range is at its last lines");
});

// An analysis of src/a.rs: an Architecture diagram with a boundary around a component, a Data
// diagram whose one node cites line 2 twice, and a flow of two steps.
function markedAnalysis() {
  const arch = diagram([
    node("D1.1", { label: "Server", anchors: [cite(1, 8)] }),
    node("D1.2", { label: "Parser", group: "D1.1", anchors: [cite(2, 4)] }),
    node("D1.3", { label: "Gone", anchors: [cite(6, 6, "unverified")] }),
    node("D1.4", { label: "Old", anchors: [cite(9, 12, "outdated")] }),
    node("D1.5", { label: "Elsewhere", anchors: [cite(1, 20, "found", "src/b.rs")] }),
  ]);
  const data = {
    ...diagram([node("D2.1", { label: "Token", anchors: [cite(2, 3), cite(2, 2)] })]),
    id: "D2",
    kind: "data",
    title: "Types",
  };
  const flow = {
    ...diagram([
      node("D3.1", { label: "Read", anchors: [cite(5, 5)] }),
      node("D3.2", { label: "Parse", anchors: [cite(2, 2), cite(7, 7)] }),
    ]),
    id: "D3",
    kind: "flow",
    title: "Load",
  };
  return { diagrams: [arch, data, flow] };
}

test("a file's marks: the innermost Architecture node per line, its chip on its first line", () => {
  const marks = fileMarks(markedAnalysis(), "src/a.rs", 10);
  const shown = (index) => {
    const mark = marks.get(index);
    const chip = mark.chip && [mark.chip.label, ...mark.chip.others.map((other) => other.label)];
    return [mark.nodeId, mark.outdated, chip];
  };
  assert.deepEqual(shown(0), ["D1.1", false, ["Server"]]);
  // The Data node is smaller on lines 2 and 3, but the Architecture node wins them. The chip
  // names each other diagram once, the Data one though it cites line 2 twice.
  assert.deepEqual(shown(1), ["D1.2", false, ["Parser", "Token", "Parse"]]);
  assert.deepEqual(shown(2), ["D1.2", false, null], "no chip after the first line");
  assert.deepEqual(shown(4), ["D1.1", false, null], "back in the boundary: no second chip");
  assert.deepEqual(shown(5), ["D1.1", false, null], "an unverified reference draws nothing");
  assert.deepEqual(shown(8), ["D1.4", true, ["Old"]], "outdated, at its last lines");
  assert.equal(marks.has(10), false, "nothing past the end of a shortened file");
  assert.equal(marks.size, 10);
  assert.equal(marks.get(1).step, null, "no step numbers while no flow is open");

  assert.deepEqual([...fileMarks(markedAnalysis(), "src/a.rs", 1).keys()], [0]);
  assert.equal(fileMarks(markedAnalysis(), "src/a.rs", 0).size, 0, "an empty file");
  assert.equal(fileMarks(null, "src/a.rs", 10).size, 0, "no analysis");
  assert.equal(fileMarks(markedAnalysis(), "src/c.rs", 10).size, 0, "a file no node cites");
});

test("an outer node whose first line an inner one wins has its chip where it first shows", () => {
  const analysis = {
    diagrams: [
      diagram([
        node("D1.1", { label: "Server", anchors: [cite(2, 10)] }),
        node("D1.2", { label: "Header", anchors: [cite(2, 3)] }),
      ]),
    ],
  };
  const marks = fileMarks(analysis, "src/a.rs", 12);
  assert.equal(marks.get(1).chip.label, "Header");
  assert.equal(marks.get(3).nodeId, "D1.1");
  assert.equal(marks.get(3).chip?.label, "Server", "on line 4, the first it shows");
  assert.equal(marks.get(4).chip, null);
});

test("of equal ranges, the node of the diagram deeper under the top shows", () => {
  const analysis = markedAnalysis();
  // A diagram under Server, whose one node cites exactly Server's lines.
  const child = {
    ...diagram([node("D4.1", { label: "Inside", anchors: [cite(1, 8)] })]),
    id: "D4",
    title: "Server, deeper",
    parent: "D1.1",
  };
  analysis.diagrams.push(child);
  const marks = fileMarks(analysis, "src/a.rs", 10);
  assert.equal(marks.get(0).nodeId, "D4.1");
  assert.deepEqual(
    marks.get(0).chip.others.map((other) => other.nodeId),
    ["D1.1"],
    "the top diagram's node is behind +1",
  );
  assert.equal(marks.get(1).nodeId, "D1.2", "the smaller range still wins");
});

test("a line only another kind covers shows that node, and an open flow numbers its steps", () => {
  const analysis = markedAnalysis();
  analysis.diagrams[0].nodes = [];
  const marks = fileMarks(analysis, "src/a.rs", 10);
  // Line 2: the Data node's and the flow's one-line references tie, and the first in the file
  // wins; the flow is the other diagram.
  assert.equal(marks.get(1).nodeId, "D2.1");
  assert.deepEqual(
    marks.get(1).chip.others.map((other) => other.diagramId),
    ["D3"],
  );
  assert.equal(marks.get(4).nodeId, "D3.1", "line 5: only the flow covers it");
  assert.equal(marks.get(4).chip.label, "Read");

  const open = fileMarks(analysis, "src/a.rs", 10, { openDiagramId: "D3" });
  const steps = [...open].filter(([, mark]) => mark.step !== null);
  assert.deepEqual(
    steps.map(([index, mark]) => [index, mark.step]),
    [
      [1, 2],
      [4, 1],
    ],
    "each step on its first reference's first line; Parse's second reference has none",
  );
  assert.notEqual(open.get(1).key, marks.get(1).key, "a step changes the row's key");
  const other = fileMarks(analysis, "src/a.rs", 10, { openDiagramId: "D2" });
  assert.equal(other.get(1).step, null, "no step numbers while another diagram is open");
});

test("a step whose first reference is past the end of a shortened file is numbered at its next", () => {
  const flow = {
    ...diagram([node("D3.1", { label: "Read", anchors: [cite(30, 31), cite(4, 4)] })]),
    id: "D3",
    kind: "flow",
  };
  const marks = fileMarks({ diagrams: [flow] }, "src/a.rs", 10, { openDiagramId: "D3" });
  assert.equal(marks.get(3).step, 1);
});

test("the node at a code line is in the diagram given, by its lines, else by its whole file", () => {
  const [arch, data] = markedAnalysis().diagrams;
  assert.equal(nodeAtLine(arch, "src/a.rs", 3), "D1.2");
  assert.equal(nodeAtLine(arch, "src/a.rs", 6), "D1.1");
  assert.equal(nodeAtLine(data, "src/a.rs", 6), null, "never a node of another diagram");
  assert.equal(nodeAtLine(arch, "src/a.rs", 30), null);

  const folder = { path: "src", target: "folder", state: "found" };
  const whole = diagram([
    node("D1.1", { kind: "boundary", anchors: [folder] }),
    node("D1.2", { group: "D1.1", anchors: [{ ...folder, path: "src/a.rs", target: "file" }] }),
    node("D1.3", { anchors: [cite(2, 2)] }),
  ]);
  assert.equal(nodeAtLine(whole, "src/a.rs", 2), "D1.3");
  assert.equal(nodeAtLine(whole, "src/a.rs", 5), "D1.2", "the node of the whole file");
  assert.equal(nodeAtLine(whole, "src/b.rs", 1), null, "a folder rings nothing");
});

test("a flow's question is 1 to 300 characters once trimmed, each emoji one", () => {
  for (const text of ["", "   \n"]) assert.match(checkFlowQuestion(text), /Ask a question/);
  assert.equal(checkFlowQuestion("  How is a note sent?  "), null);
  // 600 UTF-16 code units: 300 characters, as the route counts them.
  assert.equal(checkFlowQuestion("😀".repeat(300)), null);
  assert.match(checkFlowQuestion(`${"😀".repeat(300)}a`), /at most 300 characters, not 301/);
});
