// The dividers beside the file list and the tray, which resize them. The widths are kept per
// Mac in layout.json (src/layout.rs); ui/panes.js has the rules. Until the kept widths arrive,
// app.css's defaults stand.

import { $, showToast } from "./dom.js";
import { BIG_STEP, STEP, dragFiles, dragTray, fit, reset, step } from "./panes.js";

// Wires both dividers and the window's resize. Returns `{ load }`, which reads the kept widths.
export function dividers() {
  const panes = {
    // As `api/layout` gives them: { sidebar, tray }, the defaults, and the limits.
    layout: null,
    defaults: null,
    limits: null,
    // The divider being dragged: "files" or "tray".
    dragging: null,
    saveTimer: 0,
  };

  async function loadLayout() {
    const response = await fetch("api/layout");
    if (!response.ok) return;
    const { defaults, limits, ...layout } = await response.json();
    panes.defaults = defaults;
    panes.limits = limits;
    for (const [id, [min, max]] of [
      ["divider-files", limits.sidebar],
      ["divider-tray", limits.tray],
    ]) {
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

  function drawLayout() {
    if (!panes.layout) return;
    const { sidebar, tray } = fit(panes.layout, panes.limits, innerWidth);
    const root = document.documentElement.style;
    root.setProperty("--sidebar-width", `${sidebar}px`);
    root.setProperty("--tray-width", `${tray}px`);
    $("divider-files").setAttribute("aria-valuenow", String(sidebar));
    $("divider-tray").setAttribute("aria-valuenow", String(tray));
  }

  async function saveLayout() {
    const body = JSON.stringify(panes.layout);
    const response = await fetch("api/layout", { method: "PUT", body });
    // The widths still apply in this window; the toast says they will not be kept.
    if (!response.ok) showToast(await response.text());
  }

  for (const [id, divider] of [
    ["divider-files", "files"],
    ["divider-tray", "tray"],
  ]) {
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
      const next =
        divider === "files"
          ? dragFiles(panes.layout, panes.limits, event.clientX, innerWidth)
          : dragTray(panes.layout, panes.limits, event.clientX, innerWidth);
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
      setLayout(step(panes.layout, panes.limits, divider, delta, innerWidth), { save: true });
    });
  }

  // The kept widths are fitted to the window again whenever it changes size; that is drawing,
  // not a change of layout, so nothing is saved.
  window.addEventListener("resize", drawLayout);

  return { load: loadLayout };
}
