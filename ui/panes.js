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

// The layout once the file list's divider is dragged to `x`, the pointer's distance from the
// window's left edge. The list stops at its minimum, which still shows the Settings button
// whole: the owner asked on 2026-11-23 for no narrower list, and no folded rail.
export function dragFiles(layout, limits, x) {
  return { ...layout, sidebar: clamp(Math.round(x), limits.sidebar) };
}

// The layout once the tray's divider is dragged to `x` in a window `width` px wide. The tray
// is what lies right of the pointer, less the body's right padding and half a gap.
export function dragTray(layout, limits, x, width) {
  return { ...layout, tray: clamp(Math.round(width - x - 12), limits.tray) };
}

// The layout once a focused divider is moved by `delta` px with the arrow keys: right is
// positive, which widens the file list and narrows the tray.
export function step(layout, limits, divider, delta) {
  return divider === "files"
    ? { ...layout, sidebar: clamp(layout.sidebar + delta, limits.sidebar) }
    : { ...layout, tray: clamp(layout.tray - delta, limits.tray) };
}

// The layout with one pane back at its default width (a double-click on its divider).
export function reset(layout, defaults, divider) {
  return divider === "files"
    ? { ...layout, sidebar: defaults.sidebar }
    : { ...layout, tray: defaults.tray };
}
