import assert from "node:assert/strict";
import test from "node:test";

import { CODE_MIN, GAPS, MAP_MIN, RAIL_WIDTH, drag, fit, reset, step } from "./panes.js";

const limits = { sidebar: [200, 480], tray: [280, 720], map: [320, 960] };
const layout = (sidebar, tray, map = 400) => ({ sidebar, tray, map });
const open = { mapOpen: true };
const collapsed = { filesOpen: false };
const mapNoFiles = { mapOpen: true, filesOpen: false };
const widths = ({ sidebar, tray, map }) => ({ sidebar, tray, map });

test("a wide window draws the widths as kept", () => {
  assert.deepEqual(fit(layout(300, 460), limits, 1600), {
    sidebar: 300,
    tray: 460,
    map: 400,
    narrow: false,
  });
});

test("a narrow window takes room from the tray first, then the file list, for the code", () => {
  // 48 + 300 + 460 + 420 + 24 = 1252: 152 px too wide for 1100. The closed map takes none.
  assert.deepEqual(widths(fit(layout(300, 460), limits, 1100)), layout(300, 308));
  // The tray stops at 280; the remaining 72 px come from the file list.
  assert.deepEqual(widths(fit(layout(300, 460), limits, 1000)), layout(228, 280));
  const tiny = fit(layout(300, 460), limits, 600);
  assert.deepEqual(widths(tiny), layout(200, 280), "never below the minimums");
  assert.ok(
    RAIL_WIDTH + tiny.sidebar + tiny.tray + CODE_MIN + GAPS > 600,
    "the code pane is what shrinks",
  );
});

test("kept widths outside the limits are drawn at the nearest limit", () => {
  assert.deepEqual(widths(fit(layout(10, 9000, 5), limits, 3000)), layout(200, 720, 320));
});

test("the open map takes its room from the tray, then itself, then the file list", () => {
  // 1600: 48 + 300 + 460 + 400 + 420 + 24 = 1652, 52 px too wide: from the tray.
  assert.deepEqual(widths(fit(layout(300, 460), limits, 1600, open)), layout(300, 408));
  // 1440: 212 px too wide; the tray gives 180 down to 280, the map gives 32.
  assert.deepEqual(widths(fit(layout(300, 460), limits, 1440, open)), layout(300, 280, 368));
  // 1300: 352 too wide; the tray gives 180, the map 80 down to 320, the file list 92.
  assert.deepEqual(widths(fit(layout(300, 460, 400), limits, 1300, open)), layout(208, 280, 320));
  // 1292, the least for all four + rail: every pane at its minimum.
  const least = fit(layout(300, 460, 600), limits, 1292, open);
  assert.deepEqual(least, { sidebar: 200, tray: 280, map: 320, narrow: false });
  assert.equal(least.map, MAP_MIN);
  assert.equal(
    1292 - RAIL_WIDTH - least.sidebar - least.tray - least.map - GAPS,
    CODE_MIN,
    "code keeps 420",
  );
});

test("a window too narrow for the map and the code side by side shows one, whole", () => {
  const narrow = fit(layout(300, 600), limits, 1291, open);
  assert.equal(narrow.narrow, true);
  // The side panes leave room for the code alone: 1291 - 48 - 420 - 24 = 799 for both.
  assert.deepEqual(widths(narrow), layout(300, 499));
  assert.equal(fit(layout(300, 460), limits, 1291).narrow, false, "a closed map is never narrow");
  assert.equal(fit(layout(300, 460), limits, 800, open).narrow, true);
});

test("a collapsed file list frees its room for the map, code, and tray", () => {
  // With files collapsed, minimum side-by-side width drops by 200 px (from 1292 to 1092).
  const sideBySide = fit(layout(300, 460, 400), limits, 1092, mapNoFiles);
  assert.equal(sideBySide.narrow, false);
  assert.equal(sideBySide.tray, 280);
  assert.equal(sideBySide.map, 320);
  // Sidebar keeps its clamped saved width even while collapsed.
  assert.equal(sideBySide.sidebar, 300);
  assert.equal(fit(layout(300, 460, 400), limits, 1091, mapNoFiles).narrow, true);
  // Map starts right after the activity bar + gap when files are collapsed: 48 + 8 = 56.
  assert.equal(drag(layout(248, 320), limits, "map", 556, 3000, mapNoFiles).map, 500);
  // Tray can grow into the room freed by the collapsed file list.
  assert.equal(fit(layout(300, 500), limits, 1100, collapsed).tray, 500);
});

test("dragging the file list's divider follows the pointer within the list's limits", () => {
  // The pointer holds the divider's centre, 4 px right of the list (which starts at RAIL_WIDTH).
  assert.deepEqual(drag(layout(300, 320), limits, "files", 312.4, 3000), layout(260, 320));
  assert.deepEqual(drag(layout(300, 320), limits, "files", 40, 3000), layout(200, 320));
  assert.deepEqual(drag(layout(300, 320), limits, "files", 900, 3000), layout(480, 320));
});

test("dragging the tray's divider sets the tray to what lies right of it", () => {
  // In a 1440 px window, a pointer at 1000 leaves 440 px, less 12 for padding and half a gap.
  assert.equal(drag(layout(248, 320), limits, "tray", 1000, 1440).tray, 428);
  assert.equal(drag(layout(248, 320), limits, "tray", 1400, 1440).tray, 280);
  assert.equal(drag(layout(248, 320), limits, "tray", 100, 3000).tray, 720);
});

test("dragging the map's divider sets the map to what lies between the list and it", () => {
  // The map starts a gap right of the left rail: 48 + 248 + 8 = 304.
  assert.equal(drag(layout(248, 320), limits, "map", 804, 3000, open).map, 500);
  assert.equal(drag(layout(248, 320), limits, "map", 300, 3000, open).map, 320);
  assert.equal(drag(layout(248, 320), limits, "map", 2900, 3000, open).map, 960);
  // 1440 leaves 948 beside the rail and code: the map at most 948 - 248 - 280 = 420.
  assert.equal(drag(layout(248, 320), limits, "map", 1400, 1440, open).map, 420);
});

test("arrow keys move a divider within its limits", () => {
  assert.equal(step(layout(248, 320), limits, "files", 16, 3000).sidebar, 264);
  assert.equal(step(layout(470, 320), limits, "files", 64, 3000).sidebar, 480);
  assert.equal(step(layout(210, 320), limits, "files", -64, 3000).sidebar, 200);
  assert.equal(
    step(layout(248, 320), limits, "tray", 16, 3000).tray,
    304,
    "right narrows the tray",
  );
  assert.equal(step(layout(248, 290), limits, "tray", 64, 3000).tray, 280);
  assert.equal(step(layout(248, 320), limits, "map", 16, 3000, open).map, 416, "right widens");
  assert.equal(step(layout(248, 320, 330), limits, "map", -64, 3000, open).map, 320);
});

test("in a narrow window a moved pane is kept at the width it is drawn at", () => {
  // 1000 px leaves 508 px for both panes beside the rail and code: the list at most 508 - 280.
  const kept = drag(layout(248, 320), limits, "files", 484, 1000);
  assert.deepEqual(kept, layout(228, 320));
  assert.equal(fit(kept, limits, 1000).sidebar, 228, "drawn as kept");

  // The tray gets what the drawn list leaves: 508 - 228 = 280 (at 1048 px: 556 - 248 = 308).
  assert.equal(drag(layout(248, 320), limits, "tray", 300, 1048).tray, 308);
  assert.equal(step(layout(248, 300), limits, "tray", -64, 1048).tray, 308);

  // A list kept wider than the window allows moves from where it is drawn (228).
  assert.equal(step(layout(480, 320), limits, "files", -16, 1000).sidebar, 212);

  // With no room to spare the minimum wins.
  assert.equal(drag(layout(248, 320), limits, "files", 484, 600).sidebar, 200);
  assert.equal(drag(layout(248, 320), limits, "tray", 100, 600).tray, 280);
});

test("with the map open, every divider's pane is drawn at the width it was moved to", () => {
  // Each divider, dragged and stepped both ways, in windows from the least that shows the
  // map and the code side by side up: `fit` and `widest` take room in one order, or a pane
  // kept wider than drawn would move under the pointer.
  const starts = [layout(248, 320), layout(480, 720, 960), layout(200, 280, 320)];
  for (const width of [1292, 1340, 1440, 1700, 2400]) {
    for (const start of starts) {
      for (const [divider, key] of [
        ["files", "sidebar"],
        ["map", "map"],
        ["tray", "tray"],
      ]) {
        for (let x = 0; x <= width; x += 37) {
          const next = drag(start, limits, divider, x, width, open);
          const drawn = fit(next, limits, width, open);
          assert.equal(drawn[key], next[key], `${divider} to ${x} in ${width}`);
          assert.ok(width - RAIL_WIDTH - drawn.sidebar - drawn.tray - drawn.map - GAPS >= CODE_MIN);
        }
        for (const delta of [-64, -16, 16, 64]) {
          const next = step(start, limits, divider, delta, width, open);
          assert.equal(fit(next, limits, width, open)[key], next[key], `${divider} ${delta}`);
        }
      }
    }
  }
});

test("a double-click resets one pane and leaves the others", () => {
  const defaults = layout(248, 320, 400);
  assert.deepEqual(reset(layout(400, 600, 700), defaults, "files"), layout(248, 600, 700));
  assert.deepEqual(reset(layout(400, 600, 700), defaults, "tray"), layout(400, 320, 700));
  assert.deepEqual(reset(layout(400, 600, 700), defaults, "map"), layout(400, 600, 400));
});
