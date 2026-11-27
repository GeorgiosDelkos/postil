// Live provider quota strip in the sidebar footer, its click popover, and inline mini quota
// badges for agent picker menus (`#agent-picker` and `+ Add reviewer`).
//
// Shares the `api/usage` payload with `usage-dialog.js` and caches finished replies for 5 min so
// window focus events and menu opens never spam provider probes.

import { agentLogoIcon, el } from "./dom.js";
import {
  formatCost,
  formatCredits,
  formatPercent,
  formatReset,
  formatTokens,
  hasQuotaData,
  highestQuotaTone,
  mergeUsageView,
  quotaTone,
  rangeLabel,
  tightestQuotaText,
  tightestQuotaWindow,
  usageUrl,
  visibleQuotaAgents,
} from "./usage.js";

const POLL_MS = 1000;
const FRESH_MS = 300_000;
const AUTO_REFRESH_COOLDOWN_MS = 60_000;
const MAX_POLLS = 45;

export function usageBar({ stripEl, popoverEl, warnDotEl, openUsage }) {
  const ui = {
    view: null,
    settings: null,
    fetchedAtMs: 0,
    lastForceAtMs: 0,
    inFlight: false,
    refreshing: false,
    error: null,
    pollTimer: null,
    polls: 0,
    asked: 0,
    started: false,
    stripKey: "",
    popoverKey: "",
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
    const merged = mergeUsageView(ui.view, view);
    ui.view = merged;
    ui.fetchedAtMs = Date.now();
    ui.refreshing = false;
    ui.error = null;
    updateWarnDot(highestQuotaTone(merged.agents));

    clearTimeout(ui.pollTimer);
    ui.pollTimer = null;
    if (!fromDialog && (merged.scanning || merged.polling) && ui.polls < MAX_POLLS) {
      ui.polls += 1;
      ui.pollTimer = setTimeout(() => {
        fetchUsage({ force: false, allowStale: true }).catch(() => {});
      }, POLL_MS);
    } else if (!merged.scanning && !merged.polling) {
      ui.polls = 0;
    }

    renderStrip();
    if (!popoverEl.hidden) renderPopover();
  }

  async function fetchUsage({ force = false, allowStale = false, manual = false } = {}) {
    if (!ui.started) return;
    const now = Date.now();
    const settled = ui.view && !ui.view.scanning && !ui.view.polling;
    if (!force) {
      if (ui.inFlight) return;
      if (!allowStale && settled && now - ui.fetchedAtMs < FRESH_MS) return;
    } else if (!manual) {
      if (ui.inFlight || ui.pollTimer !== null) return;
      if (now - ui.lastForceAtMs < AUTO_REFRESH_COOLDOWN_MS) return;
    }
    const token = ++ui.asked;
    ui.inFlight = true;
    if (force) {
      ui.lastForceAtMs = now;
      ui.refreshing = true;
      ui.polls = 0;
      if (!popoverEl.hidden) renderPopover();
    }
    const base = force ? "api/usage/refresh" : "api/usage";
    const init = force ? { method: "POST" } : undefined;
    try {
      const view = await request(usageUrl(base, "project", "7d"), init);
      if (token !== ui.asked) return;
      ui.inFlight = false;
      applyView(view);
    } catch (error) {
      if (token !== ui.asked) return;
      ui.inFlight = false;
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
    const active = candidates.filter((agent) => Boolean(tightestQuotaText(agent.quota)));

    const key = JSON.stringify(
      active.map((agent) => {
        const tightest = tightestQuotaWindow(agent.quota);
        return [
          agent.name,
          tightest ? [tightest.usedPercent, tightest.tone] : null,
          tightestQuotaText(agent.quota),
        ];
      }),
    );
    const shouldHide = active.length === 0;
    if (key === ui.stripKey && stripEl.hidden === shouldHide) return;
    ui.stripKey = key;

    if (shouldHide) {
      closePopover();
      stripEl.hidden = true;
      stripEl.replaceChildren();
      return;
    }

    // Each row's cells are cells of the strip's grid, so the bars and the values line up in
    // columns whatever the names. An agent with credits only has no bar, and its row is a flex
    // row across the whole grid instead: a value placed in the grid's columns, even one spanning
    // the bar column, widens the value column for every row and squeezes every name.
    const rows = active.map((agent) => {
      const tightest = tightestQuotaWindow(agent.quota);
      const row = el("div", tightest ? "quota-strip-row" : "quota-strip-row is-credits");
      const left = el("span", "quota-strip-agent");
      const logo = agentLogoIcon(agent.name);
      if (logo) left.append(logo);
      left.append(el("span", "quota-strip-name", agent.name));
      row.append(left);

      if (tightest) {
        const bar = el("span", "quota-strip-bar");
        bar.append(meter(tightest));
        row.append(bar);
      }
      row.append(el("span", "quota-strip-text", tightestQuotaText(agent.quota)));
      return row;
    });

    stripEl.replaceChildren(...rows);
    stripEl.hidden = false;
  }

  // ---- Click Popover --------------------------------------------------------------------

  function renderPopover({ force = false } = {}) {
    const busy =
      ui.refreshing || (Boolean(ui.view?.scanning || ui.view?.polling) && ui.pollTimer !== null);
    const agents = visibleQuotaAgents(ui.view?.agents, enabledNames());
    const key = JSON.stringify({
      busy,
      error: ui.error,
      range: ui.view?.range ?? "7d",
      summary: ui.view?.summary ?? null,
      agents: agents.map((agent) => ({ name: agent.name, quota: agent.quota })),
    });
    if (!force && key === ui.popoverKey) return;
    ui.popoverKey = key;

    const hadFocus = popoverEl.contains(document.activeElement);
    const head = el("header", "quota-popover-head");
    head.append(el("span", "quota-popover-title", "Live Quotas"));
    const refreshBtn = el("button", "button quota-popover-btn", busy ? "Refreshing…" : "Refresh");
    refreshBtn.type = "button";
    refreshBtn.disabled = busy;
    refreshBtn.addEventListener("click", () => {
      fetchUsage({ force: true, manual: true });
    });
    head.append(refreshBtn);

    const list = el("div", "quota-popover-body");
    if (ui.error) {
      const err = el("p", "usage-note is-error", ui.error);
      err.style.padding = "10px 12px";
      list.append(err);
    }
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
      if (!hasQuotaData(quota)) {
        if (!quota || quota.status === "loading") {
          section.append(el("p", "usage-note", "Checking live quota…"));
        } else {
          section.append(
            el(
              "p",
              `usage-note${quota.status === "error" ? " is-error" : ""}`,
              quota.message ?? "Live quota unavailable",
            ),
          );
        }
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
        if (quota.status === "error" && quota.message) {
          section.append(el("p", "usage-note is-error", quota.message));
        }
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
    renderPopover({ force: true });
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
    const text = tightestQuotaText(quota);
    if (!text) return null;
    const badge = el("span", "menu-quota");
    if (tightest) badge.append(meter(tightest));
    badge.append(el("span", "menu-quota-text", text));
    return badge;
  }

  // A small bar filled to the window's used share, coloured by its tone.
  function meter(win) {
    const track = el("span", "quota-mini-track");
    const fill = el("span", `quota-mini-fill tone-${win.tone}`);
    fill.style.width = `${win.usedPercent}%`;
    track.append(fill);
    return track;
  }

  return {
    start() {
      ui.started = true;
      fetchUsage({ force: false, allowStale: true });
    },
    refresh({ force = false, manual = false } = {}) {
      fetchUsage({ force, manual });
    },
    syncView(view) {
      applyView(view, { fromDialog: true });
    },
    setSettings(settings) {
      ui.settings = settings;
      renderStrip();
      if (!popoverEl.hidden) renderPopover();
      if (!ui.started || ui.inFlight || ui.pollTimer !== null) return;
      if (!ui.view) {
        fetchUsage({ force: false, allowStale: true });
        return;
      }
      const missing = (settings?.enabledAgents ?? []).some(
        (name) => !ui.view.agents?.some((a) => a.name === name),
      );
      if (missing && Date.now() - ui.fetchedAtMs >= FRESH_MS) {
        fetchUsage({ force: false });
      }
    },
    closePopover,
    quotaFor,
    inlineBadge,
  };
}
