// Live provider quota strip in the sidebar footer, its click popover, and inline mini quota
// badges for agent picker menus (`#agent-picker` and `+ Add reviewer`).
//
// Shares the `api/usage` payload with `usage-dialog.js` and caches finished replies for 60 s so
// window focus events and menu opens never spam provider probes.

import { agentLogoIcon, el } from "./dom.js";
import {
  compactQuotaWindows,
  formatCost,
  formatCredits,
  formatPercent,
  formatReset,
  formatTokens,
  highestQuotaTone,
  inlineQuotaText,
  quotaTone,
  rangeLabel,
  tightestQuotaWindow,
  usageUrl,
  visibleQuotaAgents,
} from "./usage.js";

const POLL_MS = 250;
const FRESH_MS = 60_000;
const MAX_POLLS = 150;

export function usageBar({ stripEl, popoverEl, warnDotEl, openUsage }) {
  const ui = {
    view: null,
    settings: null,
    fetchedAtMs: 0,
    refreshing: false,
    error: null,
    pollTimer: null,
    polls: 0,
    asked: 0,
    started: false,
  };

  function updateWarnDot(tone) {
    if (!warnDotEl) return;
    warnDotEl.hidden = tone === null;
    warnDotEl.classList.toggle("is-critical", tone === "critical");
    warnDotEl.classList.toggle("is-warn", tone === "warn");
  }

  async function request(url, init = undefined) {
    const response = await fetch(url, init);
    const text = await response.text();
    if (!response.ok) throw new Error(text);
    return JSON.parse(text);
  }

  function applyView(view, { fromDialog = false } = {}) {
    ui.view = view;
    ui.fetchedAtMs = Date.now();
    ui.refreshing = false;
    ui.error = null;
    updateWarnDot(highestQuotaTone(view.agents));

    clearTimeout(ui.pollTimer);
    ui.pollTimer = null;
    if (!fromDialog && (view.scanning || view.polling) && ui.polls < MAX_POLLS) {
      ui.polls += 1;
      ui.pollTimer = setTimeout(() => {
        fetchUsage({ force: false, allowStale: true }).catch(() => {});
      }, POLL_MS);
    } else if (!view.scanning && !view.polling) {
      ui.polls = 0;
    }

    renderStrip();
    if (!popoverEl.hidden) renderPopover();
  }

  async function fetchUsage({ force = false, allowStale = false } = {}) {
    if (!ui.started) return;
    const settled = ui.view && !ui.view.scanning && !ui.view.polling;
    if (!force && !allowStale && settled && Date.now() - ui.fetchedAtMs < FRESH_MS) {
      return;
    }
    const token = ++ui.asked;
    if (force) {
      ui.refreshing = true;
      ui.polls = 0;
      if (!popoverEl.hidden) renderPopover();
    }
    const base = force ? "api/usage/refresh" : "api/usage";
    const init = force ? { method: "POST" } : undefined;
    try {
      const view = await request(usageUrl(base, "project", "7d"), init);
      if (token !== ui.asked) return;
      applyView(view);
    } catch (error) {
      if (token !== ui.asked) return;
      ui.refreshing = false;
      ui.error = String(error.message ?? error);
      if (!popoverEl.hidden) renderPopover();
    }
  }

  function enabledNames() {
    return ui.settings?.enabledAgents ?? [];
  }

  // ---- Sidebar Strip --------------------------------------------------------------------

  function renderStrip() {
    const candidates = visibleQuotaAgents(ui.view?.agents, enabledNames());
    const active = candidates.filter((agent) => Boolean(inlineQuotaText(agent.quota)));

    if (active.length === 0) {
      closePopover();
      stripEl.hidden = true;
      stripEl.replaceChildren();
      return;
    }

    const rows = active.map((agent) => {
      const row = el("div", "quota-strip-row");
      const left = el("span", "quota-strip-agent");
      const logo = agentLogoIcon(agent.name);
      if (logo) left.append(logo);
      left.append(el("span", "quota-strip-name", agent.name));

      const right = el("span", "quota-strip-meters");
      const compact = compactQuotaWindows(agent.quota);
      if (compact.length > 0) {
        const bars = el("span", "quota-mini-bars");
        for (const win of compact) {
          const track = el("span", "quota-mini-track");
          const fill = el("span", `quota-mini-fill tone-${win.tone}`);
          fill.style.width = `${win.usedPercent}%`;
          track.append(fill);
          bars.append(track);
        }
        right.append(bars);
      }
      right.append(el("span", "quota-strip-text", inlineQuotaText(agent.quota)));
      row.append(left, right);
      return row;
    });

    stripEl.replaceChildren(...rows);
    stripEl.hidden = false;
  }

  // ---- Click Popover --------------------------------------------------------------------

  function renderPopover() {
    const hadFocus = popoverEl.contains(document.activeElement);
    const head = el("header", "quota-popover-head");
    head.append(el("span", "quota-popover-title", "Live Quotas"));
    const busy =
      ui.refreshing || (Boolean(ui.view?.scanning || ui.view?.polling) && ui.pollTimer !== null);
    const refreshBtn = el("button", "button quota-popover-btn", busy ? "Refreshing…" : "Refresh");
    refreshBtn.type = "button";
    refreshBtn.disabled = busy;
    refreshBtn.addEventListener("click", () => {
      fetchUsage({ force: true });
    });
    head.append(refreshBtn);

    const list = el("div", "quota-popover-body");
    if (ui.error) {
      const err = el("p", "usage-note is-error", ui.error);
      err.style.padding = "10px 12px";
      list.append(err);
    }
    const agents = visibleQuotaAgents(ui.view?.agents, enabledNames());
    const nowSecs = Date.now() / 1000;

    for (const agent of agents) {
      const section = el("div", "quota-popover-agent");
      const titleRow = el("div", "quota-popover-agent-head");
      const nameWrap = el("span", "quota-popover-agent-name");
      const logo = agentLogoIcon(agent.name);
      if (logo) nameWrap.append(logo);
      nameWrap.append(agent.name);
      titleRow.append(nameWrap);
      if (agent.quota?.plan) {
        titleRow.append(el("span", "usage-pill plan", agent.quota.plan));
      }
      section.append(titleRow);

      const quota = agent.quota;
      if (!quota || quota.status === "loading") {
        section.append(el("p", "usage-note", "Checking live quota…"));
      } else if (quota.status !== "ok") {
        section.append(
          el(
            "p",
            `usage-note${quota.status === "error" ? " is-error" : ""}`,
            quota.message ?? "Live quota unavailable",
          ),
        );
      } else {
        const rows = el("div", "quota-popover-windows");
        for (const win of quota.windows ?? []) {
          const wRow = el("div", "quota-popover-win");
          const top = el("div", "quota-popover-win-top");
          top.append(el("span", "quota-label", win.label));
          const meta = el("span", "quota-popover-win-meta");
          const reset = formatReset(win.resetsAt, nowSecs);
          if (reset) meta.append(el("span", "quota-reset", reset));
          meta.append(el("span", "quota-pct", formatPercent(win.usedPercent)));
          top.append(meta);

          const track = el("div", "quota-track");
          const fill = el("div", `quota-fill tone-${quotaTone(win.usedPercent)}`);
          fill.style.width = `${Math.max(0, Math.min(100, win.usedPercent))}%`;
          track.append(fill);
          wRow.append(top, track);
          rows.append(wRow);
        }
        if (quota.credits) {
          const credit = el("div", "quota-credit");
          credit.append(
            el("span", "quota-label", quota.credits.label),
            el("span", "quota-credit-value", formatCredits(quota.credits)),
          );
          rows.append(credit);
        }
        section.append(rows);
      }
      list.append(section);
    }

    const foot = el("footer", "quota-popover-foot");
    const period = rangeLabel(ui.view?.range ?? "7d");
    const sum = ui.view?.summary;
    const sumParts = [];
    if (sum && sum.runs > 0) {
      sumParts.push(`${sum.runs} ${sum.runs === 1 ? "run" : "runs"}`);
      sumParts.push(`${formatTokens(sum.totalTokens)} tok`);
      if (sum.cost) sumParts.push(formatCost(sum.cost.amount, sum.cost.currency));
    }
    const summaryLabel =
      sumParts.length > 0 ? `${period} · ${sumParts.join(" · ")}` : `${period} activity`;
    foot.append(el("span", "quota-popover-summary", summaryLabel));
    const openBtn = el("button", "button quota-popover-btn", "Analytics ⇧⌘U");
    openBtn.type = "button";
    openBtn.addEventListener("click", () => {
      closePopover();
      openUsage?.();
    });
    foot.append(openBtn);

    popoverEl.replaceChildren(head, list, foot);
    if (hadFocus) popoverEl.focus();
    positionPopover();
  }

  function positionPopover() {
    const rect = stripEl.getBoundingClientRect();
    const popRect = popoverEl.getBoundingClientRect();
    const left = Math.min(Math.max(8, rect.right + 8), Math.max(8, innerWidth - popRect.width - 8));
    const maxTop = innerHeight - popRect.height - 8;
    const top = Math.max(8, Math.min(rect.bottom - popRect.height, maxTop));
    popoverEl.style.left = `${left}px`;
    popoverEl.style.top = `${top}px`;
  }

  function openPopover() {
    if (stripEl.hidden) return;
    popoverEl.hidden = false;
    stripEl.setAttribute("aria-expanded", "true");
    renderPopover();
    popoverEl.focus();
    fetchUsage();
  }

  function closePopover() {
    if (popoverEl.hidden) return;
    popoverEl.hidden = true;
    stripEl.setAttribute("aria-expanded", "false");
  }

  stripEl.addEventListener("click", () => {
    if (popoverEl.hidden) openPopover();
    else closePopover();
  });

  window.addEventListener(
    "keydown",
    (event) => {
      if (popoverEl.hidden || event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      closePopover();
      stripEl.focus();
    },
    true,
  );

  window.addEventListener(
    "mousedown",
    (event) => {
      if (popoverEl.hidden) return;
      if (!popoverEl.contains(event.target) && !stripEl.contains(event.target)) {
        closePopover();
      }
    },
    true,
  );
  window.addEventListener("blur", closePopover);
  window.addEventListener("resize", () => {
    if (!popoverEl.hidden) positionPopover();
  });

  // ---- Inline Picker Badge --------------------------------------------------------------

  function quotaFor(agentName) {
    return ui.view?.agents?.find((agent) => agent.name === agentName)?.quota ?? null;
  }

  function inlineBadge(agentName) {
    const quota = quotaFor(agentName);
    const tightest = tightestQuotaWindow(quota);
    const text = inlineQuotaText(quota);
    if (!text) return null;
    const badge = el("span", "menu-quota");
    if (tightest) {
      const track = el("span", "quota-mini-track");
      const fill = el("span", `quota-mini-fill tone-${tightest.tone}`);
      fill.style.width = `${tightest.usedPercent}%`;
      track.append(fill);
      badge.append(track);
    }
    badge.append(el("span", "menu-quota-text", text));
    return badge;
  }

  return {
    start() {
      ui.started = true;
      fetchUsage({ force: false, allowStale: true });
    },
    refresh({ force = false } = {}) {
      fetchUsage({ force });
    },
    syncView(view) {
      applyView(view, { fromDialog: true });
    },
    setSettings(settings) {
      ui.settings = settings;
      renderStrip();
      const missing = (settings?.enabledAgents ?? []).some(
        (name) => !ui.view?.agents?.some((a) => a.name === name && a.quota?.status !== "loading"),
      );
      if (missing) fetchUsage({ force: false, allowStale: true });
    },
    closePopover,
    quotaFor,
    inlineBadge,
  };
}
