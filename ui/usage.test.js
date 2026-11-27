import assert from "node:assert/strict";
import test from "node:test";

import {
  compactQuotaWindows,
  formatCost,
  formatCredits,
  formatDuration,
  formatPercent,
  formatReset,
  formatTokens,
  hasQuotaData,
  highestQuotaTone,
  mergeUsageView,
  quotaTone,
  summaryItems,
  tightestQuotaText,
  tightestQuotaWindow,
  tokenBreakdownTitle,
  usageUrl,
  visibleQuotaAgents,
} from "./usage.js";

test("formatTokens compacts counts cleanly", () => {
  assert.equal(formatTokens(0), "0");
  assert.equal(formatTokens(842), "842");
  assert.equal(formatTokens(12_450), "12.5k");
  assert.equal(formatTokens(145_000), "145k");
  assert.equal(formatTokens(1_420_000), "1.42M");
});

test("formatDuration formats seconds, minutes, and hours", () => {
  assert.equal(formatDuration(0), "0s");
  assert.equal(formatDuration(45), "45s");
  assert.equal(formatDuration(252), "4m 12s");
  assert.equal(formatDuration(3900), "1h 05m");
});

test("formatCost formats USD, small non-zero amounts, and other currencies", () => {
  assert.equal(formatCost(null, null), "—");
  assert.equal(formatCost(1.42, "USD"), "$1.42");
  assert.equal(formatCost(0.002, "USD"), "<$0.01");
  assert.equal(formatCost(3.5, "EUR"), "3.50 EUR");
});

test("formatPercent and quotaTone clamp values and pick thresholds", () => {
  assert.equal(formatPercent(62.4), "62%");
  assert.equal(formatPercent(4.5), "4.5%");
  assert.equal(formatPercent(120), "100%");
  assert.equal(quotaTone(79.9), "normal");
  assert.equal(quotaTone(80), "warn");
  assert.equal(quotaTone(95), "critical");
});

test("formatReset renders countdowns in minutes, hours, and days", () => {
  const now = 1_700_000_000;
  assert.equal(formatReset(null, now), "");
  assert.equal(formatReset(now + 30, now), "resets soon");
  assert.equal(formatReset(now + 42 * 60, now), "resets in 42m");
  assert.equal(formatReset(now + 2 * 3600 + 14 * 60, now), "resets in 2h 14m");
  assert.equal(formatReset(now + 3 * 86_400 + 4 * 3600, now), "resets in 3d 4h");
});

test("formatCredits handles unlimited, spent/limit, and remaining balances", () => {
  assert.equal(formatCredits({ unlimited: true }), "Unlimited");
  assert.equal(formatCredits({ used: 4.2, limit: 50, currency: "USD" }), "$4.20 / $50.00");
  assert.equal(formatCredits({ remaining: 12.5 }), "12.5 left");
});

test("highestQuotaTone flags >=90% as warn and >=95% as critical", () => {
  assert.equal(highestQuotaTone([]), null);
  assert.equal(
    highestQuotaTone([{ quota: { status: "ok", windows: [{ usedPercent: 70 }] } }]),
    null,
  );
  assert.equal(
    highestQuotaTone([{ quota: { status: "ok", windows: [{ usedPercent: 91 }] } }]),
    "warn",
  );
  assert.equal(
    highestQuotaTone([
      { quota: { status: "ok", windows: [{ usedPercent: 91 }] } },
      { quota: { status: "ok", windows: [{ usedPercent: 96 }] } },
    ]),
    "critical",
  );
});

test("summaryItems, usageUrl, and tokenBreakdownTitle format view metadata", () => {
  const items = summaryItems(
    { runs: 1, totalTokens: 160, cost: { amount: 0.02, currency: "USD" }, durationSecs: 12 },
    2,
  );
  assert.deepEqual(items, [
    { label: "run", value: "1" },
    { label: "tokens", value: "160" },
    { label: "reported", value: "$0.02" },
    { label: "active", value: "12s" },
    { label: "agents", value: "2" },
  ]);
  assert.equal(usageUrl("api/usage", "project", "7d"), "api/usage?scope=project&range=7d");
  assert.ok(tokenBreakdownTitle({ inputTokens: 120, outputTokens: 30 }).includes("Input:"));
});

test("compactQuotaWindows, tightestQuotaWindow, tightestQuotaText, and visibleQuotaAgents", () => {
  const quota = {
    status: "ok",
    windows: [
      { label: "Gemini models (5h)", shortLabel: "5h", usedPercent: 35.3 },
      { label: "Gemini models (weekly)", shortLabel: "wk", usedPercent: 81.2 },
      { label: "Claude & GPT (5h)", usedPercent: 0 },
    ],
  };
  const compact = compactQuotaWindows(quota);
  assert.equal(compact.length, 2);
  assert.equal(tightestQuotaText(quota), "wk 81%", "one window: the tightest");
  assert.equal(tightestQuotaWindow(quota).label, "wk");
  assert.equal(tightestQuotaWindow(quota).tone, "warn");

  const creditsOnly = {
    status: "ok",
    windows: [],
    credits: { used: 12.5, limit: 50, currency: "USD" },
  };
  assert.equal(tightestQuotaText(creditsOnly), "$12.50 / $50.00");
  assert.equal(tightestQuotaText({ status: "unavailable", windows: [] }), "");

  const agents = [
    { name: "Claude", builtIn: true, found: "/bin/claude" },
    { name: "Antigravity", builtIn: true, found: "/bin/agy" },
    { name: "Custom", builtIn: false, found: "/bin/custom" },
  ];
  assert.deepEqual(
    visibleQuotaAgents(agents, ["Antigravity"]).map((a) => a.name),
    ["Antigravity"],
  );
  assert.deepEqual(
    visibleQuotaAgents(agents, []).map((a) => a.name),
    ["Claude", "Antigravity"],
  );
});

test("cached quota data stays displayable during loading and transient errors", () => {
  const cachedWindows = [{ label: "5-hour session", shortLabel: "5h", usedPercent: 92 }];
  const loadingCached = { status: "loading", windows: cachedWindows };
  const errorCached = { status: "error", message: "HTTP 429", windows: cachedWindows };
  const unavailable = { status: "unavailable", message: "Signed out", windows: cachedWindows };

  assert.equal(hasQuotaData(loadingCached), true);
  assert.equal(hasQuotaData(errorCached), true);
  assert.equal(hasQuotaData(unavailable), false);
  assert.equal(hasQuotaData({ status: "loading", windows: [] }), false);

  assert.equal(tightestQuotaText(loadingCached), "5h 92%");
  assert.equal(tightestQuotaText(errorCached), "5h 92%");
  assert.equal(tightestQuotaText(unavailable), "");

  assert.equal(highestQuotaTone([{ quota: loadingCached }]), "warn");
  assert.equal(highestQuotaTone([{ quota: errorCached }]), "warn");
  assert.equal(highestQuotaTone([{ quota: unavailable }]), null);
});

test("mergeUsageView preserves agents across rescans, refreshes, and transient errors", () => {
  const previous = {
    scanning: false,
    polling: false,
    agents: [
      {
        name: "Claude",
        builtIn: true,
        found: "/bin/claude",
        quota: {
          status: "ok",
          plan: "Max",
          windows: [{ label: "5-hour session", shortLabel: "5h", usedPercent: 42 }],
          fetchedAt: 1000,
        },
      },
      {
        name: "Codex",
        builtIn: true,
        found: "/bin/codex",
        quota: {
          status: "ok",
          plan: "Plus",
          windows: [{ label: "5-hour window", shortLabel: "5h", usedPercent: 15 }],
          fetchedAt: 1000,
        },
      },
    ],
  };

  // While scanning (agents === null), previous agents stay visible.
  const scanning = mergeUsageView(previous, { scanning: true, polling: true, agents: null });
  assert.equal(scanning.scanning, true);
  assert.equal(scanning.agents.length, 2);

  // Loading or error with empty windows keeps previous quota data; unavailable clears it.
  const refreshing = mergeUsageView(previous, {
    scanning: false,
    polling: true,
    agents: [
      { name: "Claude", builtIn: true, found: "/bin/claude", quota: { status: "loading", windows: [] } },
      { name: "Codex", builtIn: true, found: "/bin/codex", quota: { status: "error", message: "timeout", windows: [] } },
    ],
  });
  assert.equal(refreshing.agents[0].quota.status, "loading");
  assert.equal(refreshing.agents[0].quota.plan, "Max");
  assert.equal(refreshing.agents[0].quota.windows[0].usedPercent, 42);
  assert.equal(refreshing.agents[1].quota.status, "error");
  assert.equal(refreshing.agents[1].quota.message, "timeout");
  assert.equal(refreshing.agents[1].quota.windows[0].usedPercent, 15);

  // Explicit unavailable (e.g. signed out) does not retain stale quota windows.
  const signedOut = mergeUsageView(previous, {
    scanning: false,
    polling: false,
    agents: [
      { name: "Claude", builtIn: true, found: "/bin/claude", quota: { status: "unavailable", message: "Session expired", windows: [] } },
      {
        name: "Codex",
        builtIn: true,
        found: "/bin/codex",
        quota: { status: "ok", plan: "Plus", windows: [{ label: "5-hour window", shortLabel: "5h", usedPercent: 20 }], fetchedAt: 2000 },
      },
    ],
  });
  assert.equal(signedOut.agents[0].quota.status, "unavailable");
  assert.deepEqual(signedOut.agents[0].quota.windows, []);
  assert.equal(signedOut.agents[1].quota.windows[0].usedPercent, 20);
});
