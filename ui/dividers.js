// The dividers beside the file list, the map and the tray, which resize them. The widths are
// kept per Mac in layout.json (src/layout.rs); ui/panes.js has the rules. Until the kept widths
// arrive, app.css's defaults stand.

import { $, showToast } from "./dom.js";
import { BIG_STEP, STEP, drag, fit, reset, step } from "./panes.js";

// Each divider's element, the layout key it sets, and the CSS variable that width goes to.
const DIVIDERS = {
  files: { id: "divider-files", key: "sidebar", prop: "--sidebar-width" },
  map: { id: "divider-map", key: "map", prop: "--map-width" },
  tray: { id: "divider-tray", key: "tray", prop: "--tray-width" },
};

// Wires the dividers and the window's resize. `onNarrow(narrow)` is called after every draw
// with whether the window is too narrow for the open map beside the code (panes.js, `fit`).
// Returns `{ load, setMapOpen, isNarrow }`: `load` reads the kept widths, `setMapOpen` draws the
// panes with the map open or closed.
export function dividers({ onNarrow }) {
  const panes = {
    // As `api/layout` gives them: { sidebar, tray, map }, the defaults, and the limits.
    layout: null,
    defaults: null,
    limits: null,
    // Whether the map shows; kept for the window's session only, not in layout.json.
    mapOpen: false,
    // Whether the file list drawer shows beside the activity bar; session-scoped.
    filesOpen: true,
    // Whether the last draw found the window too narrow for the map beside the code.
    narrow: false,
    // The divider being dragged: a key of DIVIDERS.
    dragging: null,
    saveTimer: 0,
  };

  async function loadLayout() {
    const response = await fetch("api/layout");
    if (!response.ok) return;
    const { defaults, limits, ...layout } = await response.json();
    panes.defaults = defaults;
    panes.limits = limits;
    for (const { id, key } of Object.values(DIVIDERS)) {
      const [min, max] = limits[key];
      $(id).setAttribute("aria-valuemin", String(min));
      $(id).setAttribute("aria-valuemax", String(max));
    }
    setLayout(layout, { save: false });
  }

  // Draws `layout`, fitted to the window, and saves it unless asked not to. Saving waits for a
  // moment of quiet, so holding an arrow key writes the file once.
  function setLayout(layout, { save }) {
    panes.layout = layout;
    drawLayout();
    if (!save) return;
    clearTimeout(panes.saveTimer);
    panes.saveTimer = setTimeout(saveLayout, 300);
  }

  function options() {
    return { mapOpen: panes.mapOpen, filesOpen: panes.filesOpen };
  }

  function drawLayout() {
    const root = document.documentElement.style;
    if (panes.layout) {
      const drawn = fit(panes.layout, panes.limits, innerWidth, options());
      for (const { id, key, prop } of Object.values(DIVIDERS)) {
        root.setProperty(prop, `${drawn[key]}px`);
        $(id).setAttribute("aria-valuenow", String(drawn[key]));
      }
      root.setProperty("--sidebar-drawn-width", panes.filesOpen ? `${drawn.sidebar}px` : "0px");
      panes.narrow = drawn.narrow;
    } else if (!panes.filesOpen) {
      root.setProperty("--sidebar-drawn-width", "0px");
    } else {
      root.removeProperty("--sidebar-drawn-width");
    }
    $("divider-files").hidden = !panes.filesOpen;
    // The map's divider shows only beside the map; a narrow window shows the map alone.
    $("divider-map").hidden = !panes.mapOpen || panes.narrow;
    onNarrow(panes.narrow);
  }

  async function saveLayout() {
    const body = JSON.stringify(panes.layout);
    const response = await fetch("api/layout", { method: "PUT", body });
    // The widths still apply in this window; the toast says they will not be kept.
    if (!response.ok) showToast(await response.text());
  }

  for (const [divider, { id }] of Object.entries(DIVIDERS)) {
    const el = $(id);
    const stop = () => {
      if (panes.dragging !== divider) return;
      panes.dragging = null;
      el.classList.remove("active");
      document.body.classList.remove("resizing");
      setLayout(panes.layout, { save: true });
    };
    el.addEventListener("pointerdown", (event) => {
      if (event.button !== 0 || !panes.layout) return;
      event.preventDefault();
      el.setPointerCapture(event.pointerId);
      panes.dragging = divider;
      el.classList.add("active");
      document.body.classList.add("resizing");
    });
    el.addEventListener("pointermove", (event) => {
      if (panes.dragging !== divider) return;
      // A release outside the window can arrive without a pointerup here.
      if ((event.buttons & 1) === 0) return stop();
      const { layout, limits } = panes;
      const next = drag(layout, limits, divider, event.clientX, innerWidth, options());
      setLayout(next, { save: false });
    });
    // Fires after pointerup and pointercancel, and also when the capture is lost without either:
    // a dialog opened with Cmd-, mid-drag makes the divider inert.
    el.addEventListener("lostpointercapture", stop);
    el.addEventListener("dblclick", () => {
      if (panes.layout) setLayout(reset(panes.layout, panes.defaults, divider), { save: true });
    });
    el.addEventListener("keydown", (event) => {
      if (!panes.layout || (event.key !== "ArrowLeft" && event.key !== "ArrowRight")) return;
      // The arrows move the divider and nothing else; the window's other keys still work here.
      event.preventDefault();
      event.stopPropagation();
      const size = event.shiftKey ? BIG_STEP : STEP;
      const delta = event.key === "ArrowRight" ? size : -size;
      const next = step(panes.layout, panes.limits, divider, delta, innerWidth, options());
      setLayout(next, { save: true });
    });
  }

  // The kept widths are fitted to the window again whenever it changes size; that is drawing,
  // not a change of layout, so nothing is saved.
  window.addEventListener("resize", drawLayout);

  return {
    load: loadLayout,
    // Opening or closing the map or file list changes what the other panes must leave room for,
    // so the panes are fitted again; nothing is saved.
    setMapOpen: (open) => {
      panes.mapOpen = open;
      drawLayout();
    },
    setFilesOpen: (open) => {
      panes.filesOpen = open;
      drawLayout();
    },
    isNarrow: () => panes.narrow,
  };
}
