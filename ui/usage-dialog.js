// The Usage report dialog: shows live provider account quotas and filtered Postil activity
// (per agent and per underlying model) from `api/usage`.
//
// Polls while the agent scan or any live provider quota probe is in flight, and redraws only
// when the reply changes so focus and scroll position stay put.

import { el } from "./dom.js";
import {
  formatCost,
  formatCredits,
  formatDuration,
  formatPercent,
  formatReset,
  formatTokens,
  highestQuotaTone,
  quotaTone,
  rangeLabel,
  summaryItems,
  tokenBreakdownTitle,
  usageUrl,
} from "./usage.js";

const POLL_MS = 250;

function button(text, key, onClick, className = "button") {
  const node = el("button", className, text);
  node.type = "button";
  node.dataset.key = key;
  node.addEventListener("click", onClick);
  return node;
}

export function usageDialog(dialogEl, { openSettings, onTone }) {
  const ui = {
    scope: "project",
    range: "7d",
    view: null,
    error: null,
    refreshing: false,
    pollTimer: null,
    // Monotonic request number so replies to an older filter never overwrite a newer one.
    asked: 0,
  };

  async function request(url, init = undefined) {
    const response = await fetch(url, init);
    const text = await response.text();
    if (!response.ok) throw new Error(text);
    return JSON.parse(text);
  }

  function show(view) {
    const unchanged =
      !ui.refreshing && ui.error === null && JSON.stringify(view) === JSON.stringify(ui.view);
    ui.view = view;
    ui.scope = view.scope;
    ui.range = view.range;
    ui.error = null;
    ui.refreshing = false;
    onTone?.(highestQuotaTone(view.agents));
    clearTimeout(ui.pollTimer);
    if (dialogEl.open && (view.scanning || view.polling)) {
      ui.pollTimer = setTimeout(() => {
        fetchView().catch(fail);
      }, POLL_MS);
    }
    if (!unchanged) render();
  }

  function fail(error) {
    ui.error = String(error.message ?? error);
    ui.refreshing = false;
    render();
  }

  async function fetchView(forceRefresh = false) {
    const token = ++ui.asked;
    const base = forceRefresh ? "api/usage/refresh" : "api/usage";
    const init = forceRefresh ? { method: "POST" } : undefined;
    const view = await request(usageUrl(base, ui.scope, ui.range), init);
    if (token !== ui.asked) return;
    show(view);
  }

  function setScope(scope) {
    if (scope === ui.scope) return;
    ui.scope = scope;
    render();
    fetchView().catch(fail);
  }

  function setRange(range) {
    if (range === ui.range) return;
    ui.range = range;
    render();
    fetchView().catch(fail);
  }

  function triggerRefresh() {
    if (ui.refreshing || ui.view?.scanning || ui.view?.polling) return;
    ui.refreshing = true;
    ui.error = null;
    render();
    fetchView(true).catch(fail);
  }

  // ---- Drawing --------------------------------------------------------------------------

  function render() {
    const active = document.activeElement;
    const focused = dialogEl.contains(active) ? active.dataset.key : undefined;
    const scrolled = dialogEl.querySelector(".settings-body")?.scrollTop ?? 0;
    dialogEl.replaceChildren(panel());
    const body = dialogEl.querySelector(".settings-body");
    if (body) body.scrollTop = scrolled;
    if (!focused) return;
    const target = dialogEl.querySelector(`[data-key="${CSS.escape(focused)}"]`);
    if (target && !target.disabled) target.focus();
    else dialogEl.focus();
  }

  function panel() {
    const node = el("div", "settings-panel");
    node.append(header(), toolbar(), body(), footer());
    return node;
  }

  function header() {
    const node = el("header", "settings-header");
    const title = el("h2", "settings-title", "Usage");
    title.id = "usage-title";
    const right = el("div", "usage-header-actions");
    const busy = ui.refreshing || Boolean(ui.view?.scanning || ui.view?.polling);
    const refresh = button(busy ? "Refreshing…" : "Refresh", "usage-refresh", triggerRefresh);
    refresh.classList.add("usage-refresh-btn");
    refresh.disabled = busy;
    const close = button("×", "usage-close", () => dialogEl.close(), "icon-button");
    close.setAttribute("aria-label", "Close");
    right.append(refresh, close);
    node.append(title, right);
    return node;
  }

  function toolbar() {
    const bar = el("div", "usage-toolbar");
    const hasProject = ui.view ? ui.view.hasProject : true;
    const projectLabel = ui.view?.projectName ? ui.view.projectName : "This project";
    const scopes = [
      ["project", hasProject ? projectLabel : "This project", !hasProject],
      ["all", "All projects", false],
    ];
    const ranges = [
      ["24h", "24h", false],
      ["7d", "7d", false],
      ["30d", "30d", false],
      ["all", "All", false],
    ];
    bar.append(
      segmented("scope", scopes, ui.scope, setScope, "Project scope"),
      segmented("range", ranges, ui.range, setRange, "Time range"),
    );
    return bar;
  }

  function segmented(key, options, current, choose, ariaLabel) {
    const node = el("div", "segmented");
    node.setAttribute("role", "radiogroup");
    node.setAttribute("aria-label", ariaLabel);
    for (const [value, label, disabled] of options) {
      const option = button(label, `${key}-${value}`, () => choose(value), "segment");
      option.setAttribute("role", "radio");
      option.setAttribute("aria-checked", String(value === current));
      option.disabled = Boolean(disabled);
      node.append(option);
    }
    return node;
  }

  function body() {
    const node = el("div", "settings-body usage-body");
    if (!ui.view) {
      node.append(el("p", "settings-hint scanning", "Loading usage…"));
      return node;
    }
    if (ui.view.scanning || ui.view.agents === null) {
      node.append(el("p", "settings-hint scanning", "Looking for installed agents…"));
      return node;
    }

    node.append(summaryStrip(ui.view.summary, ui.view.agents.length));

    if (ui.view.agents.length === 0) {
      node.append(emptyState());
      return node;
    }

    const cards = el("div", "usage-cards");
    for (const agent of ui.view.agents) {
      cards.append(agentCard(agent));
    }
    node.append(cards);
    return node;
  }

  function summaryStrip(summary, agentCount) {
    const strip = el("div", "usage-summary");
    for (const item of summaryItems(summary, agentCount)) {
      const stat = el("div", "usage-stat");
      stat.append(
        el("span", "usage-stat-value", item.value),
        el("span", "usage-stat-label", item.label),
      );
      strip.append(stat);
    }
    return strip;
  }

  function emptyState() {
    const box = el("div", "usage-empty");
    box.append(
      el("p", "usage-empty-title", "No autodetected agents found."),
      el(
        "p",
        "settings-hint",
        "Install Claude, Codex, Gemini, Grok, or OpenCode, or add a custom ACP agent in Settings.",
      ),
    );
    const actions = el("div", "settings-actions");
    actions.append(button("Open Settings…", "usage-open-settings", () => openSettings?.()));
    box.append(actions);
    return box;
  }

  function agentCard(agent) {
    const card = el("section", "usage-card");

    const head = el("header", "usage-card-header");
    const left = el("div", "usage-card-title");
    left.append(el("span", "usage-agent-name", agent.name));
    if (!agent.builtIn) left.append(el("span", "agent-badge", "custom"));

    const badges = el("div", "usage-card-badges");
    if (agent.quota?.plan) badges.append(el("span", "usage-pill plan", agent.quota.plan));
    const stateLabel = agent.enabled ? "Enabled" : agent.found ? "Installed" : "Not installed";
    badges.append(el("span", `usage-pill${agent.enabled ? " enabled" : ""}`, stateLabel));
    head.append(left, badges);
    card.append(head);

    card.append(quotaBlock(agent.quota));
    card.append(activityBlock(agent.activity));
    return card;
  }

  function quotaBlock(quota) {
    const block = el("div", "usage-sub");
    block.append(el("div", "usage-sub-heading", "Account quota · live"));
    if (!quota || quota.status === "loading") {
      block.append(el("p", "usage-note", "Checking live quota…"));
      return block;
    }
    if (quota.status === "unavailable" || quota.status === "error") {
      const note = el(
        "p",
        `usage-note${quota.status === "error" ? " is-error" : ""}`,
        quota.message ?? "Live quota unavailable",
      );
      block.append(note);
      return block;
    }

    const nowSecs = Date.now() / 1000;
    const list = el("div", "quota-list");
    for (const win of quota.windows ?? []) {
      const row = el("div", "quota-row");
      const label = el("span", "quota-label", win.label);
      label.title = win.label;
      const track = el("div", "quota-track");
      const fill = el("div", `quota-fill tone-${quotaTone(win.usedPercent)}`);
      fill.style.width = `${Math.max(0, Math.min(100, win.usedPercent))}%`;
      track.append(fill);
      const pct = el("span", "quota-pct", formatPercent(win.usedPercent));
      const reset = el("span", "quota-reset", formatReset(win.resetsAt, nowSecs));
      row.append(label, track, pct, reset);
      list.append(row);
    }
    if (quota.credits) {
      const creditRow = el("div", "quota-credit");
      creditRow.append(
        el("span", "quota-label", quota.credits.label),
        el("span", "quota-credit-value", formatCredits(quota.credits)),
      );
      list.append(creditRow);
    }
    block.append(list);
    return block;
  }

  function activityBlock(activity) {
    const block = el("div", "usage-sub");
    const scopeName =
      ui.view?.scope === "project" ? (ui.view.projectName ?? "This project") : "All projects";
    block.append(
      el("div", "usage-sub-heading", `Postil activity · ${scopeName} · ${rangeLabel(ui.range)}`),
    );
    if (!activity || activity.runs === 0) {
      block.append(el("p", "usage-note", "No Postil runs in this period."));
      return block;
    }

    const table = el("div", "usage-models");
    const headerRow = el("div", "usage-model-row usage-model-head");
    headerRow.append(
      el("span", "usage-col-model", "Model"),
      el("span", "usage-col-num", "Runs"),
      el("span", "usage-col-num", "Tokens"),
      el("span", "usage-col-num", "Cost"),
      el("span", "usage-col-num", "Time"),
    );
    table.append(headerRow);

    for (const model of activity.models ?? []) {
      const row = el("div", "usage-model-row");
      const name = el("span", "usage-col-model", model.model);
      name.title = model.model;
      const runs = el("span", "usage-col-num", String(model.runs));
      const tokens = el("span", "usage-col-num", formatTokens(model.totalTokens));
      const breakdown = tokenBreakdownTitle(model);
      if (breakdown) tokens.title = breakdown;
      const cost = el(
        "span",
        "usage-col-num",
        formatCost(model.cost?.amount, model.cost?.currency),
      );
      const time = el("span", "usage-col-num", formatDuration(model.durationSecs));
      row.append(name, runs, tokens, cost, time);
      table.append(row);
    }
    block.append(table);
    return block;
  }

  function footer() {
    const node = el("footer", "settings-footer");
    const hint = el(
      "span",
      "usage-footer-hint",
      "Quotas are account-wide; activity reflects Postil runs.",
    );
    if (ui.view?.ledgerFile) hint.title = ui.view.ledgerFile;
    const statusText = ui.error ?? updatedLabel(ui.view?.agents);
    const status = el("span", `settings-status${ui.error ? " failed" : ""}`, statusText);
    status.setAttribute("role", "status");
    node.append(hint, status);
    return node;
  }

  function updatedLabel(agents) {
    if (ui.refreshing || ui.view?.polling) return "Checking quotas…";
    if (!Array.isArray(agents)) return "";
    let latest = 0;
    for (const agent of agents) {
      if (agent.quota?.fetchedAt && agent.quota.fetchedAt > latest) {
        latest = agent.quota.fetchedAt;
      }
    }
    if (!latest) return "";
    const date = new Date(latest * 1000);
    const hh = String(date.getHours()).padStart(2, "0");
    const mm = String(date.getMinutes()).padStart(2, "0");
    return `Updated ${hh}:${mm}`;
  }

  dialogEl.addEventListener("close", () => {
    clearTimeout(ui.pollTimer);
  });
  dialogEl.addEventListener("mousedown", (event) => {
    if (event.target === dialogEl) dialogEl.close();
  });

  return {
    async open() {
      if (dialogEl.open) return;
      ui.error = null;
      render();
      dialogEl.showModal();
      dialogEl.focus();
      await fetchView().catch(fail);
    },
  };
}
