// The benchmark: runs when the app was started with `--bench FILE`. Opens the file, scrolls
// through it one step per frame, lays out the largest diagram the map draws, and posts the
// timings to the Rust process, which prints them and exits.

import { layoutDiagram } from "./diagram.js";

const nextFrame = () => new Promise((resolve) => requestAnimationFrame(resolve));

// `code` is the code view (code-view.js) and `codeEl` the element it scrolls.
export async function runBench(path, { code, codeEl }) {
  const openStarted = performance.now();
  await code.openFile(path);
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
    lines: code.lineCount(),
    // From the page starting to run until the file was painted, and the open alone.
    pagePaintedAtMs: Math.round(paintedAtMs),
    openFileMs: Math.round(openMs),
    frames: frameMs.length,
    frameMs: { p50: median, p95: at(0.95), max: at(1) },
    // Frames that took more than one and a half times the median frame, which is the display
    // interval when scrolling keeps up: a visible stutter.
    slowFrames: frameMs.filter((ms) => ms > median * 1.5).length,
    scrolledPx: Math.round(codeEl.scrollTop),
    // After the paint and the scroll, so neither includes it.
    diagram: await benchDiagram(),
  };
  window.ipc.postMessage(`bench:${JSON.stringify(report)}`);
}

// The most a 200-node layout may take, in ms: spike S4 measured 35 to 52 ms in WebKit, 63 to
// 77 ms for a page's first layout (docs/design.md, "Dependencies"); twice the slowest leaves
// room for a loaded machine.
const DIAGRAM_BUDGET_MS = 150;

// Loads the map's layout library as the map does, and lays out the largest diagram an analysis
// keeps (200 nodes, src/analysis/merge.rs) with 260 edges, in boundaries of ten: the page's
// first layout, the slowest.
async function benchDiagram() {
  await new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = "vendor/dagre.min.js";
    script.addEventListener("load", resolve);
    script.addEventListener("error", () => reject(new Error("vendor/dagre.min.js did not load")));
    document.head.append(script);
  });
  const anchors = [{ path: "x", target: "file", state: "found" }];
  const nodes = Array.from({ length: 200 }, (_, index) => ({
    id: `D1.${index + 1}`,
    kind: index % 10 === 0 ? "boundary" : "component",
    label: `Node ${index + 1}`,
    summary: "",
    detail: "",
    group: index % 10 === 0 ? undefined : `D1.${index - (index % 10) + 1}`,
    anchors,
  }));
  const cards = nodes.filter((node) => node.kind !== "boundary");
  const edges = Array.from({ length: 260 }, (_, index) => ({
    from: cards[index % cards.length].id,
    to: cards[(index * 7 + 3) % cards.length].id,
    label: "",
  })).filter((edge) => edge.from !== edge.to);
  const diagram = { id: "D1", kind: "architecture", title: "Bench", run: "R1", nodes, edges };
  const started = performance.now();
  layoutDiagram(diagram, window.dagre, new Map());
  const layoutMs = Math.round(performance.now() - started);
  return {
    nodes: nodes.length,
    edges: edges.length,
    layoutMs,
    budgetMs: DIAGRAM_BUDGET_MS,
    withinBudget: layoutMs <= DIAGRAM_BUDGET_MS,
  };
}
