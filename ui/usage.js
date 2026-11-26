// Pure formatting and view-model helpers for the Usage popover and dialog, without the DOM, so
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

// Visual tone for a quota utilization bar: normal (<80%), warn (80–89%), critical (>=90%).
// The thresholds are Orca's (20% and 10% left), which the popover follows. Compared after
// rounding as `formatPercent` rounds, so 79.6%, printed "80%", is already amber.
export function quotaTone(usedPercent) {
  if (!Number.isFinite(usedPercent)) return "normal";
  const shown = Math.round(usedPercent);
  if (shown >= 90) return "critical";
  if (shown >= 80) return "warn";
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

// The highest used percentage of one agent's quota windows, or null when it has no reading.
export function agentTightest(agent) {
  if (agent.quota?.status !== "ok") return null;
  let tightest = null;
  for (const win of agent.quota.windows ?? []) {
    if (Number.isFinite(win.usedPercent) && (tightest === null || win.usedPercent > tightest)) {
      tightest = win.usedPercent;
    }
  }
  return tightest;
}

// The highest used percentage across every agent's quota windows, for the sidebar button, or
// null when no agent has a reading.
export function tightestQuota(agents) {
  if (!Array.isArray(agents)) return null;
  let tightest = null;
  for (const agent of agents) {
    const used = agentTightest(agent);
    if (used !== null && (tightest === null || used > tightest)) tightest = used;
  }
  return tightest;
}

// The agents the popover lists: the installed built-ins (`found` is the program's path), the
// only ones whose quota is probed; any other row could say only that it has no quota. The
// tightest come first, then those without a reading, by name, so the agent closest to a
// limit is the first row.
export function popoverAgents(agents) {
  if (!Array.isArray(agents)) return [];
  const listed = agents.filter((agent) => agent.builtIn && agent.found);
  return listed.toSorted((a, b) => {
    const left = agentTightest(a) ?? -1;
    const right = agentTightest(b) ?? -1;
    return right - left || a.name.localeCompare(b.name);
  });
}

// The earliest reset among `windows` (Unix seconds), or null when none says when.
export function soonestReset(windows) {
  let soonest = null;
  for (const win of windows ?? []) {
    if (Number.isFinite(win.resetsAt) && (soonest === null || win.resetsAt < soonest)) {
      soonest = win.resetsAt;
    }
  }
  return soonest;
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
