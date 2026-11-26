// The Usage popover: a small panel anchored above the sidebar's Usage button, as Orca shows
// usage from its status bar. It lists each installed built-in agent's live account quota,
// tightest first; Postil's own run history stays in the Usage dialog, which its last row
// opens.
//
// It asks `api/usage` when it opens, which probes the quotas that are stale, and polls while
// probes are in flight. Closed, it does no work: the button keeps the last reading.

import { el } from "./dom.js";
import {
  formatCredits,
  formatPercent,
  formatReset,
  popoverAgents,
  quotaTone,
  soonestReset,
  usageUrl,
} from "./usage.js";

const POLL_MS = 250;

// `popoverEl` is the panel, `anchorEl` the button it opens above. `openDetails` opens the
// Usage dialog; `onView(view)` receives every reply, for the button's reading.
export function usagePopover({ popoverEl, anchorEl, openDetails, onView }) {
  const ui = { view: null, error: null, refreshing: false, pollTimer: null, asked: 0 };

  // Asks for the view; a reply or failure for a request since superseded (by a newer one, or
  // by closing the panel) is dropped, so it can neither overwrite a newer reading nor restart
  // polling. Never rejects.
  async function fetchView(refresh = false) {
    const token = ++ui.asked;
    // The route always lists an installed built-in, whatever the range; the range only adds
    // agents with past runs, which the popover does not list (`popoverAgents`).
    const url = usageUrl(refresh ? "api/usage/refresh" : "api/usage", "all", "24h");
    try {
      const response = await fetch(url, refresh ? { method: "POST" } : undefined);
      const text = await response.text();
      if (token !== ui.asked) return;
      if (!response.ok) throw new Error(text);
      show(JSON.parse(text));
    } catch (error) {
      if (token !== ui.asked) return;
      fail(error);
    }
  }

  function show(view) {
    // A poll that changed nothing is not drawn, so the list keeps its scroll and focus; a
    // refresh always is, to take the busy state off its button.
    const unchanged =
      !ui.refreshing && ui.error === null && JSON.stringify(view) === JSON.stringify(ui.view);
    ui.view = view;
    ui.error = null;
    ui.refreshing = false;
    onView?.(view);
    poll();
    if (!unchanged) render();
  }

  // Keeps the rows already drawn: one failed poll should not hide a good reading.
  function fail(error) {
    ui.error = String(error.message ?? error);
    ui.refreshing = false;
    poll();
    render();
  }

  // Asks again shortly while the agent scan or a quota probe is still running.
  function poll() {
    clearTimeout(ui.pollTimer);
    if (!popoverEl.hidden && (ui.view?.scanning || ui.view?.polling)) {
      ui.pollTimer = setTimeout(fetchView, POLL_MS);
    }
  }

  function busy() {
    return ui.refreshing || Boolean(ui.view?.scanning || ui.view?.polling);
  }

  function refresh() {
    if (busy()) return;
    ui.refreshing = true;
    ui.error = null;
    render();
    fetchView(true);
  }

  // ---- Drawing --------------------------------------------------------------------------

  function render() {
    const focused = popoverEl.contains(document.activeElement)
      ? document.activeElement.dataset.key
      : undefined;
    const scrolled = popoverEl.querySelector(".usage-pop-body")?.scrollTop ?? 0;
    popoverEl.replaceChildren(header(), body(), footer());
    popoverEl.querySelector(".usage-pop-body").scrollTop = scrolled;
    const target = focused && popoverEl.querySelector(`[data-key="${CSS.escape(focused)}"]`);
    if (target && !target.disabled) target.focus();
    else if (focused) popoverEl.focus();
  }

  function header() {
    const node = el("header", "usage-pop-header");
    const title = el("span", "usage-pop-title", "Usage");
    title.id = "usage-popover-title";
    const again = el("button", "usage-pop-refresh", "↻");
    again.type = "button";
    again.dataset.key = "usage-pop-refresh";
    again.setAttribute("aria-label", "Refresh");
    again.title = "Refresh";
    again.disabled = busy();
    again.classList.toggle("is-busy", busy());
    again.addEventListener("click", refresh);
    node.append(title, el("span", "usage-pop-scope", "· all agents"), again);
    return node;
  }

  function body() {
    const node = el("div", "usage-pop-body");
    if (ui.error) {
      const note = el("p", "usage-pop-note is-error", ui.error);
      note.setAttribute("role", "alert");
      node.append(note);
      if (!ui.view) return node;
    }
    if (!ui.view || ui.view.scanning || ui.view.agents === null) {
      node.append(el("p", "usage-pop-note", "Loading usage…"));
      return node;
    }
    const agents = popoverAgents(ui.view.agents);
    if (agents.length === 0) {
      node.append(el("p", "usage-pop-note", "No installed agent reports an account quota."));
      return node;
    }
    const nowSecs = Date.now() / 1000;
    for (const agent of agents) node.append(row(agent, nowSecs));
    return node;
  }

  function row(agent, nowSecs) {
    const node = el("div", "usage-pop-row");
    const quota = agent.quota;
    const head = el("div", "usage-pop-row-head");
    const name = el("span", "usage-pop-name", agent.name);
    if (quota?.plan) name.append(el("span", "usage-pop-plan", ` · ${quota.plan}`));
    head.append(name);
    node.append(head);

    if (!quota || quota.status === "loading") {
      head.append(el("span", "usage-pop-status", "Checking…"));
      return node;
    }
    // On a line of its own, wrapped: the message often says what to run, which a cut-off
    // line would hide.
    if (quota.status !== "ok") {
      const message = el(
        "p",
        `usage-pop-message${quota.status === "error" ? " is-error" : ""}`,
        quota.message ?? "Usage unavailable",
      );
      node.append(message);
      return node;
    }

    const reset = soonestReset(quota.windows);
    if (reset !== null) head.append(el("span", "usage-pop-status", formatReset(reset, nowSecs)));
    // One line per window: its label, a bar taking the width left over, and the percentage.
    const windows = el("div", "usage-pop-windows");
    for (const win of quota.windows ?? []) {
      const label = el("span", "usage-pop-label", win.label);
      const resets = formatReset(win.resetsAt, nowSecs);
      label.title = `${win.label}: ${formatPercent(win.usedPercent)} used${resets ? `, ${resets}` : ""}`;
      const track = el("span", "usage-pop-track");
      const fill = el("span", `usage-pop-fill tone-${quotaTone(win.usedPercent)}`);
      fill.style.width = `${Math.max(0, Math.min(100, win.usedPercent))}%`;
      track.append(fill);
      windows.append(label, track, el("span", "usage-pop-pct", formatPercent(win.usedPercent)));
    }
    if (quota.credits) {
      windows.append(
        el("span", "usage-pop-label", quota.credits.label),
        el("span", "usage-pop-credit", formatCredits(quota.credits)),
      );
    }
    node.append(windows);
    return node;
  }

  function footer() {
    const details = el("button", "usage-pop-details");
    details.type = "button";
    details.dataset.key = "usage-pop-details";
    details.append(el("span", "", "Usage details & history"), el("span", "usage-pop-chevron", "›"));
    details.addEventListener("click", () => {
      close();
      openDetails();
    });
    return details;
  }

  // ---- Opening and closing --------------------------------------------------------------

  // Opens above the button, its left edge on the button's, inside the window.
  function place() {
    const anchor = anchorEl.getBoundingClientRect();
    const { width } = popoverEl.getBoundingClientRect();
    popoverEl.style.left = `${Math.max(4, Math.min(anchor.left, innerWidth - width - 4))}px`;
    popoverEl.style.bottom = `${Math.max(4, innerHeight - anchor.top + 6)}px`;
  }

  function open() {
    if (!popoverEl.hidden) return;
    // A hidden button (the welcome screen hides the sidebar) has no box to open above: the
    // panel would sit off screen, probing.
    if (anchorEl.getClientRects().length === 0) return;
    ui.error = null;
    render();
    popoverEl.hidden = false;
    anchorEl.setAttribute("aria-expanded", "true");
    place();
    popoverEl.focus();
    fetchView();
  }

  function close() {
    if (popoverEl.hidden) return;
    popoverEl.hidden = true;
    anchorEl.setAttribute("aria-expanded", "false");
    clearTimeout(ui.pollTimer);
    // A reply still on its way must not restart polling or redraw a closed panel's focus.
    ui.asked++;
    if (popoverEl.contains(document.activeElement) || document.activeElement === document.body) {
      anchorEl.focus();
    }
  }

  // While open, the panel owns the keyboard, as the context menu does: no key reaches the
  // window's handlers, so `a` cannot answer a permission and `c` cannot open a composer
  // under it. A Command key is stopped but not prevented, so WebKit still hands it to the menu
  // bar, whose command closes or toggles the panel (`window.postil.menu`).
  popoverEl.addEventListener("keydown", (event) => {
    event.stopPropagation();
    if (event.metaKey) return;
    if (event.key === "Escape") {
      event.preventDefault();
      close();
    } else if (event.key === "Tab") {
      // The focus cycles inside the panel instead of leaving it open behind the focus.
      event.preventDefault();
      const buttons = [...popoverEl.querySelectorAll("button:not(:disabled)")];
      const index = buttons.indexOf(document.activeElement);
      const step = event.shiftKey ? -1 : 1;
      const next = index === -1 ? (step === 1 ? 0 : buttons.length - 1) : index + step;
      buttons.at(next % buttons.length)?.focus();
    }
    // Enter and Space are the focused button's own: they click it.
  });
  // WebKit does not focus a button that is clicked: the press would move the focus out of the
  // panel to the page, where the next Escape would stop a run instead of closing the panel.
  popoverEl.addEventListener("mousedown", (event) => event.preventDefault());
  // A press outside the panel closes it; a press on the button is the button's (it toggles).
  window.addEventListener(
    "mousedown",
    (event) => {
      if (!popoverEl.contains(event.target) && !anchorEl.contains(event.target)) close();
    },
    true,
  );
  window.addEventListener("blur", close);
  addEventListener("resize", () => {
    if (!popoverEl.hidden) place();
  });

  return {
    open,
    close,
    toggle: () => (popoverEl.hidden ? open() : close()),
  };
}
