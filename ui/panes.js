// The rules for the window's pane widths: dragging the two dividers and fitting the panes into
// the window. Kept apart from the document so `node --test` runs them. A layout is what
// `api/layout` keeps (src/layout.rs): `{ sidebar, tray }`, widths in CSS px, with the limits it
// sends: `{ sidebar: [min, max], tray: [min, max] }`.

// The code pane's least width: about 50 columns of code at the default size, enough to read a
// line and its note. Arbitrary.
export const CODE_MIN = 420;

// The fixed space around the panes: the body's two 8 px gaps between three columns, and its
// 8 px right padding (app.css, body).
export const GAPS = 24;

// How far an arrow key moves a focused divider, and with Shift.
export const STEP = 16;
export const BIG_STEP = 64;

const clamp = (value, [min, max]) => Math.min(Math.max(value, min), max);

// The widths to draw for `layout` in a window `width` px wide. The code pane keeps CODE_MIN:
// the tray gives way first, then the file list, neither below its minimum. A window too
// narrow even for that draws the minimums, and the code pane is what shrinks.
export function fit(layout, limits, width) {
  let sidebar = clamp(layout.sidebar, limits.sidebar);
  let tray = clamp(layout.tray, limits.tray);
  let excess = sidebar + tray + CODE_MIN + GAPS - width;
  if (excess > 0) {
    const fromTray = Math.min(excess, tray - limits.tray[0]);
    tray -= fromTray;
    excess -= fromTray;
  }
  if (excess > 0) sidebar -= Math.min(excess, sidebar - limits.sidebar[0]);
  return { sidebar, tray };
}

// The widest each pane can be drawn in a window `width` px wide with the code pane at CODE_MIN:
// the file list may take the tray's room down to the tray's minimum, the tray only what the
// drawn file list leaves. A moved pane is kept at what is drawn, so the saved width is the one
// the owner saw, and every key press moves the divider.
function widest(layout, limits, width) {
  const room = width - CODE_MIN - GAPS;
  return {
    sidebar: Math.min(limits.sidebar[1], room - limits.tray[0]),
    tray: Math.min(limits.tray[1], room - fit(layout, limits, width).sidebar),
  };
}

// `value` at most `max`, then at least `min`: in a window with no room to spare the minimum
// wins, as in `fit`.
const within = (value, min, max) => Math.max(Math.min(value, max), min);

// The layout once the file list's divider is dragged to `x`, the pointer's distance from the
// window's left edge, in a window `width` px wide. The pointer holds the divider's centre, half
// a gap right of the list. The list stops at its minimum, which still shows the Settings button
// whole: the owner asked on 2026-11-23 for no narrower list, and no folded rail.
export function dragFiles(layout, limits, x, width) {
  const max = widest(layout, limits, width).sidebar;
  return { ...layout, sidebar: within(Math.round(x - 4), limits.sidebar[0], max) };
}

// The layout once the tray's divider is dragged to `x` in a window `width` px wide. The tray
// is what lies right of the pointer, less the body's right padding and half a gap.
export function dragTray(layout, limits, x, width) {
  const max = widest(layout, limits, width).tray;
  return { ...layout, tray: within(Math.round(width - x - 12), limits.tray[0], max) };
}

// The layout once a focused divider is moved by `delta` px with the arrow keys, in a window
// `width` px wide: right is positive, which widens the file list and narrows the tray. The move
// starts from the drawn width.
export function step(layout, limits, divider, delta, width) {
  const drawn = fit(layout, limits, width);
  const max = widest(layout, limits, width);
  return divider === "files"
    ? { ...layout, sidebar: within(drawn.sidebar + delta, limits.sidebar[0], max.sidebar) }
    : { ...layout, tray: within(drawn.tray - delta, limits.tray[0], max.tray) };
}

// The layout with one pane back at its default width (a double-click on its divider).
export function reset(layout, defaults, divider) {
  return divider === "files"
    ? { ...layout, sidebar: defaults.sidebar }
    : { ...layout, tray: defaults.tray };
}
