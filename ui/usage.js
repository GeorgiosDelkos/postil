// Pure formatting and view-model helpers for the Usage report dialog, without the DOM, so
// `ui/usage.test.js` can test them under `node --test`.

// Compact human-readable token count: 0, 842, 12.5k, 1.42M.
export function formatTokens(count) {
  if (!Number.isFinite(count) || count <= 0) return "0";
  if (count < 1000) return String(Math.round(count));
  if (count < 1_000_000) {
    const k = count / 1000;
    return `${k >= 100 ? Math.round(k) : Math.round(k * 10) / 10}k`;
  }
  const m = count / 1_000_000;
  return `${m >= 100 ? Math.round(m) : Math.round(m * 100) / 100}M`;
}

// Compact duration: 0s, 45s, 4m 12s, 1h 05m.
export function formatDuration(secs) {
  if (!Number.isFinite(secs) || secs <= 0) return "0s";
  const total = Math.round(secs);
  if (total < 60) return `${total}s`;
  const hours = Math.floor(total / 3600);
  const mins = Math.floor((total % 3600) / 60);
  const rem = total % 60;
  if (hours > 0) return `${hours}h ${String(mins).padStart(2, "0")}m`;
  return `${mins}m ${String(rem).padStart(2, "0")}s`;
}

// Formats a reported cost, or "—" when none was reported.
export function formatCost(amount, currency) {
  if (!Number.isFinite(amount) || !currency) return "—";
  if (currency === "USD") {
    if (amount > 0 && amount < 0.005) return "<$0.01";
    return `$${amount.toFixed(2)}`;
  }
  return `${amount.toFixed(2)} ${currency}`;
}

// Clamps and formats a quota utilization percentage.
export function formatPercent(value) {
  if (!Number.isFinite(value)) return "0%";
  const clamped = Math.max(0, Math.min(100, value));
  if (clamped >= 10 || Math.abs(clamped - Math.round(clamped)) < 0.05) {
    return `${Math.round(clamped)}%`;
  }
  return `${clamped.toFixed(1)}%`;
}

// Visual tone for a quota utilization bar: normal (<80%), warn (80–94%), critical (>=95%).
export function quotaTone(usedPercent) {
  if (!Number.isFinite(usedPercent)) return "normal";
  if (usedPercent >= 95) return "critical";
  if (usedPercent >= 80) return "warn";
  return "normal";
}

// Human-readable countdown until `resetsAt` (Unix seconds), relative to `nowSecs`.
export function formatReset(resetsAt, nowSecs) {
  if (!Number.isFinite(resetsAt)) return "";
  const delta = Math.round(resetsAt - nowSecs);
  if (delta <= 60) return "resets soon";
  const mins = Math.floor(delta / 60);
  if (mins < 60) return `resets in ${mins}m`;
  const hours = Math.floor(mins / 60);
  const remMins = mins % 60;
  if (hours < 24) {
    return remMins > 0 ? `resets in ${hours}h ${remMins}m` : `resets in ${hours}h`;
  }
  const days = Math.floor(hours / 24);
  const remHours = hours % 24;
  return remHours > 0 ? `resets in ${days}d ${remHours}h` : `resets in ${days}d`;
}

// Formats an agent's credit or extra-usage line.
export function formatCredits(credits) {
  if (!credits) return "";
  if (credits.unlimited) return "Unlimited";
  if (credits.used != null && credits.limit != null) {
    const currency = credits.currency ?? "USD";
    return `${formatCost(credits.used, currency)} / ${formatCost(credits.limit, currency)}`;
  }
  if (credits.remaining != null) {
    return credits.currency
      ? `${formatCost(credits.remaining, credits.currency)} left`
      : `${Number(credits.remaining.toFixed(2))} left`;
  }
  return "";
}

// Whether `quota` carries displayable windows or credits (including cached data preserved
// during a background refresh or after a transient probe error).
export function hasQuotaData(quota) {
  if (!quota || quota.status === "unavailable") return false;
  return (Array.isArray(quota.windows) && quota.windows.length > 0) || Boolean(quota.credits);
}

// Highest alert tone across cached quota windows for the sidebar indicator:
// "critical" (>= 95%), "warn" (>= 90%), or null.
export function highestQuotaTone(agents) {
  if (!Array.isArray(agents)) return null;
  let tone = null;
  for (const agent of agents) {
    if (!hasQuotaData(agent.quota)) continue;
    for (const win of agent.quota.windows ?? []) {
      if (win.usedPercent >= 95) return "critical";
      if (win.usedPercent >= 90) tone = "warn";
    }
  }
  return tone;
}

// Compact top-level summary items for the Usage dialog header strip.
export function summaryItems(summary, agentCount) {
  const items = [
    { label: summary.runs === 1 ? "run" : "runs", value: String(summary.runs) },
    { label: "tokens", value: formatTokens(summary.totalTokens) },
  ];
  if (summary.cost) {
    items.push({
      label: "reported",
      value: formatCost(summary.cost.amount, summary.cost.currency),
    });
  }
  if (summary.durationSecs > 0) {
    items.push({ label: "active", value: formatDuration(summary.durationSecs) });
  }
  items.push({ label: agentCount === 1 ? "agent" : "agents", value: String(agentCount) });
  return items;
}

export function rangeLabel(range) {
  return { "24h": "24h", "7d": "7d", "30d": "30d", all: "All time" }[range] ?? range;
}

export function usageUrl(base, scope, range) {
  const params = new URLSearchParams({ scope, range });
  return `${base}?${params.toString()}`;
}

export function tokenBreakdownTitle(row) {
  const parts = [];
  if (row.inputTokens > 0) parts.push(`Input: ${row.inputTokens.toLocaleString()}`);
  if (row.outputTokens > 0) parts.push(`Output: ${row.outputTokens.toLocaleString()}`);
  if (row.cachedReadTokens > 0) parts.push(`Cached: ${row.cachedReadTokens.toLocaleString()}`);
  if (row.cachedWriteTokens > 0) {
    parts.push(`Cache writes: ${row.cachedWriteTokens.toLocaleString()}`);
  }
  if (row.thoughtTokens > 0) parts.push(`Thought: ${row.thoughtTokens.toLocaleString()}`);
  return parts.join(" · ");
}

// Primary compact quota windows (`5h`, `wk`, `1d`, `mo`) for the sidebar strip and agent
// picker menus. Falls back to the first two windows when none carries `shortLabel`.
export function compactQuotaWindows(quota) {
  if (
    !quota ||
    quota.status === "unavailable" ||
    !Array.isArray(quota.windows) ||
    quota.windows.length === 0
  ) {
    return [];
  }
  const tagged = quota.windows.filter((win) => win.shortLabel);
  const chosen = tagged.length > 0 ? tagged : quota.windows.slice(0, 2);
  return chosen.map((win) => {
    const usedPercent = Number.isFinite(win.usedPercent)
      ? Math.max(0, Math.min(100, win.usedPercent))
      : 0;
    return {
      label: win.shortLabel || inferShortLabel(win.label),
      fullLabel: win.label,
      usedPercent,
      tone: quotaTone(usedPercent),
      resetsAt: win.resetsAt ?? null,
    };
  });
}

function inferShortLabel(label) {
  const lower = String(label ?? "").toLowerCase();
  if (lower.includes("5h") || lower.includes("5-hour")) return "5h";
  if (lower.includes("week") || lower.includes("7-day")) return "wk";
  if (lower.includes("day") || lower.includes("daily") || lower.includes("24h")) return "1d";
  if (lower.includes("month")) return "mo";
  return "lim";
}

// The compact window with the highest utilization: the one meter the sidebar strip and the menu
// badges show. The owner asked on 2026-11-27 for every agent's line to look alike, its bar in
// the same place; two stacked bars and two values made Antigravity's line wider than the rest
// and cut its name. The popover lists every window.
export function tightestQuotaWindow(quota) {
  const compact = compactQuotaWindows(quota);
  if (compact.length === 0) return null;
  return compact.reduce((best, win) => (win.usedPercent > best.usedPercent ? win : best));
}

// The text beside that meter ("wk 58%"), or the credits when there is no window, or "" when
// there is nothing to show.
export function tightestQuotaText(quota) {
  const tightest = tightestQuotaWindow(quota);
  if (tightest) return `${tightest.label} ${formatPercent(tightest.usedPercent)}`;
  if (quota && quota.status !== "unavailable" && quota.credits) {
    return formatCredits(quota.credits);
  }
  return "";
}

// Merges a newly fetched usage view with the previous one so agents and their last-known-good
// quotas are never lost while a scan/probe is in flight or after a transient probe error.
export function mergeUsageView(previousView, nextView) {
  if (!nextView) return previousView ?? null;
  const prevAgents = previousView?.agents;
  if (!Array.isArray(prevAgents) || prevAgents.length === 0) return nextView;

  if (!Array.isArray(nextView.agents)) {
    return { ...nextView, agents: prevAgents };
  }

  const prevByName = new Map(prevAgents.map((agent) => [agent.name, agent]));
  const agents = nextView.agents.map((agent) => {
    const quota = agent.quota;
    if (hasQuotaData(quota) || quota?.status === "unavailable") return agent;
    const prevQuota = prevByName.get(agent.name)?.quota;
    if (!hasQuotaData(prevQuota)) return agent;
    return {
      ...agent,
      quota: {
        ...prevQuota,
        status: quota?.status ?? prevQuota.status,
        message: quota?.message,
      },
    };
  });
  return { ...nextView, agents };
}

// Built-in agents to show in the sidebar quota strip and popover: enabled built-in agents when
// any are enabled, else installed built-in agents.
export function visibleQuotaAgents(agents, enabledNames) {
  if (!Array.isArray(agents)) return [];
  const enabledSet = new Set(Array.isArray(enabledNames) ? enabledNames : []);
  const builtIns = agents.filter((agent) => agent.builtIn && agent.found);
  if (enabledSet.size > 0) {
    const enabledBuiltIns = builtIns.filter((agent) => enabledSet.has(agent.name));
    if (enabledBuiltIns.length > 0) return enabledBuiltIns;
  }
  return builtIns;
}
