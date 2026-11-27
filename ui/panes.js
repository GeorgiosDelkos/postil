// The rules for the window's pane widths: dragging the three dividers and fitting the panes
// into the window. Kept apart from the document so `node --test` runs them. A layout is what
// `api/layout` keeps (src/layout.rs): `{ sidebar, tray, map }`, widths in CSS px, with the
// limits it sends: `{ sidebar: [min, max], tray: [min, max], map: [min, max] }`. The map sits
// inside the code pane, left of the code, and takes room only while it is open.

// The code pane's least width: about 50 columns of code at the default size, enough to read a
// line and its note. Arbitrary.
export const CODE_MIN = 420;

// The map's least width beside the code, as src/layout.rs's `MAP` starts: two diagram nodes
// side by side. A window with no room for both at their least shows one of them, whole.
export const MAP_MIN = 320;

// The fixed space around the panes: the body's two 8 px gaps between three columns, and its
// 8 px right padding (app.css, body). The map shares the code pane's box, so it adds none.
export const GAPS = 24;

// How far an arrow key moves a focused divider, and with Shift.
export const STEP = 16;
export const BIG_STEP = 64;

const clamp = (value, [min, max]) => Math.min(Math.max(value, min), max);

// Whether a window `width` px wide is too narrow to show the map and the code side by side,
// each at its least, beside the file list and the tray at theirs: the map then takes the whole
// code pane, and Cmd-Shift-M switches between it and the code.
function tooNarrow(limits, width) {
  return width < limits.sidebar[0] + limits.tray[0] + MAP_MIN + CODE_MIN + GAPS;
}

// The widths to draw for `layout` in a window `width` px wide, and whether the map is too
// narrow to share the code pane (`narrow`). The code pane keeps CODE_MIN, and the map, while
// open, its least too: the tray gives way first, then the map, then the file list, none below
// its minimum. A window too narrow even for that draws the minimums, and the code pane is
// what shrinks. A narrow window shows one of the map and the code at a time, so the side panes
// leave room for the code alone, whichever shows: switching moves no pane.
export function fit(layout, limits, width, { mapOpen = false } = {}) {
  const narrow = mapOpen && tooNarrow(limits, width);
  const sideBySide = mapOpen && !narrow;
  let sidebar = clamp(layout.sidebar, limits.sidebar);
  let tray = clamp(layout.tray, limits.tray);
  let map = clamp(layout.map, limits.map);
  let excess = sidebar + tray + (sideBySide ? map : 0) + CODE_MIN + GAPS - width;
  const give = (current, min) => {
    const given = Math.max(0, Math.min(excess, current - min));
    excess -= given;
    return current - given;
  };
  tray = give(tray, limits.tray[0]);
  if (sideBySide) map = give(map, limits.map[0]);
  sidebar = give(sidebar, limits.sidebar[0]);
  return { sidebar, tray, map, narrow };
}

// The widest each pane can be drawn in a window `width` px wide with the code pane at
// CODE_MIN, in the order `fit` takes room back: the file list may take the others' room down
// to their minimums, the map only what the drawn file list leaves beside the tray's minimum,
// the tray only what the drawn file list and map leave. A moved pane is kept at what is
// drawn, so the saved width is the one the owner saw, and every key press moves the divider.
function widest(layout, limits, width, options) {
  const drawn = fit(layout, limits, width, options);
  const map = options.mapOpen && !drawn.narrow;
  const room = width - CODE_MIN - GAPS;
  return {
    sidebar: Math.min(limits.sidebar[1], room - limits.tray[0] - (map ? limits.map[0] : 0)),
    map: Math.min(limits.map[1], room - drawn.sidebar - limits.tray[0]),
    tray: Math.min(limits.tray[1], room - drawn.sidebar - (map ? drawn.map : 0)),
  };
}

// `value` at most `max`, then at least `min`: in a window with no room to spare the minimum
// wins, as in `fit`.
const within = (value, min, max) => Math.max(Math.min(value, max), min);

// Each divider: the layout key it sets, and which way an arrow key moves it. Right is
// positive, which widens the file list and the map and narrows the tray.
const DIVIDERS = {
  files: { key: "sidebar", sign: 1 },
  map: { key: "map", sign: 1 },
  tray: { key: "tray", sign: -1 },
};

// The layout with `divider`'s pane at `value`, kept within its limits and what the window
// leaves it.
function moved(layout, limits, width, options, divider, value) {
  const { key } = DIVIDERS[divider];
  const max = widest(layout, limits, width, options)[key];
  return { ...layout, [key]: within(Math.round(value), limits[key][0], max) };
}

// The layout once `divider` is dragged to `x`, the pointer's distance from the window's left
// edge, in a window `width` px wide, with the map open or not (`options`, as `fit`). The
// pointer holds the divider's centre: half a gap right of the file list, on the map's right
// border, half a gap left of the tray. The tray is what lies right of the pointer, less the
// body's right padding and half a gap. The list stops at its minimum, which still shows the
// Settings button whole: the owner asked on 2026-11-23 for no narrower list, and no folded
// rail.
export function drag(layout, limits, divider, x, width, options = {}) {
  const drawn = fit(layout, limits, width, options);
  const value = {
    files: x - 4,
    // The map starts a gap right of the file list.
    map: x - drawn.sidebar - 8,
    tray: width - x - 12,
  }[divider];
  return moved(layout, limits, width, options, divider, value);
}

// The layout once a focused divider is moved by `delta` px with the arrow keys, in a window
// `width` px wide. The move starts from the drawn width.
export function step(layout, limits, divider, delta, width, options = {}) {
  const { key, sign } = DIVIDERS[divider];
  const drawn = fit(layout, limits, width, options);
  return moved(layout, limits, width, options, divider, drawn[key] + sign * delta);
}

// The layout with one pane back at its default width (a double-click on its divider).
export function reset(layout, defaults, divider) {
  const { key } = DIVIDERS[divider];
  return { ...layout, [key]: defaults[key] };
}
