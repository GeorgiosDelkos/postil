import assert from "node:assert/strict";
import test from "node:test";

import { CODE_MIN, GAPS, dragFiles, dragTray, fit, reset, step } from "./panes.js";

const limits = { sidebar: [200, 480], tray: [280, 720] };
const layout = (sidebar, tray) => ({ sidebar, tray });

test("a wide window draws the widths as kept", () => {
  assert.deepEqual(fit(layout(300, 460), limits, 1600), { sidebar: 300, tray: 460 });
});

test("a narrow window takes room from the tray first, then the file list, for the code", () => {
  // 300 + 460 + 420 + 24 = 1204: 104 px too wide for 1100.
  assert.deepEqual(fit(layout(300, 460), limits, 1100), { sidebar: 300, tray: 356 });
  // The tray stops at 280; the remaining 24 px come from the file list.
  assert.deepEqual(fit(layout(300, 460), limits, 1000), { sidebar: 276, tray: 280 });
  const tiny = fit(layout(300, 460), limits, 600);
  assert.deepEqual(tiny, { sidebar: 200, tray: 280 }, "never below the minimums");
  assert.ok(tiny.sidebar + tiny.tray + CODE_MIN + GAPS > 600, "the code pane is what shrinks");
});

test("kept widths outside the limits are drawn at the nearest limit", () => {
  assert.deepEqual(fit(layout(10, 9000), limits, 3000), { sidebar: 200, tray: 720 });
});

test("dragging the file list's divider follows the pointer within the list's limits", () => {
  assert.deepEqual(dragFiles(layout(300, 320), limits, 260.4), layout(260, 320));
  assert.deepEqual(dragFiles(layout(300, 320), limits, 40), layout(200, 320), "no narrower");
  assert.deepEqual(dragFiles(layout(300, 320), limits, 900), layout(480, 320), "at most 480");
});

test("dragging the tray's divider sets the tray to what lies right of it", () => {
  // In a 1440 px window, a pointer at 1000 leaves 440 px, less 12 for padding and half a gap.
  assert.equal(dragTray(layout(248, 320), limits, 1000, 1440).tray, 428);
  assert.equal(dragTray(layout(248, 320), limits, 1400, 1440).tray, 280);
  assert.equal(dragTray(layout(248, 320), limits, 100, 1440).tray, 720);
});

test("arrow keys move a divider within its limits", () => {
  assert.equal(step(layout(248, 320), limits, "files", 16).sidebar, 264);
  assert.equal(step(layout(470, 320), limits, "files", 64).sidebar, 480);
  assert.equal(step(layout(210, 320), limits, "files", -64).sidebar, 200);
  assert.equal(step(layout(248, 320), limits, "tray", 16).tray, 304, "right narrows the tray");
  assert.equal(step(layout(248, 290), limits, "tray", 64).tray, 280);
});

test("a double-click resets one pane and leaves the other", () => {
  const defaults = layout(248, 320);
  assert.deepEqual(reset(layout(400, 600), defaults, "files"), layout(248, 600));
  assert.deepEqual(reset(layout(400, 600), defaults, "tray"), layout(400, 320));
});
