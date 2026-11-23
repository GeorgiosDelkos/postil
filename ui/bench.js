// The benchmark: runs when the app was started with `--bench FILE`. Opens the file, scrolls
// through it one step per frame, and posts the timings to the Rust process, which prints them
// and exits.

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
  const at = (fraction) =>
    Number(frameMs[Math.floor((frameMs.length - 1) * fraction)].toFixed(2));
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
  };
  window.ipc.postMessage(`bench:${JSON.stringify(report)}`);
}
