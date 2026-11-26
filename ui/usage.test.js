import assert from "node:assert/strict";
import test from "node:test";

import {
  formatCost,
  formatCredits,
  formatDuration,
  formatPercent,
  formatReset,
  formatTokens,
  popoverAgents,
  quotaTone,
  soonestReset,
  summaryItems,
  tightestQuota,
  tokenBreakdownTitle,
  usageUrl,
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
  assert.equal(quotaTone(79.4), "normal");
  assert.equal(quotaTone(80), "warn");
  assert.equal(quotaTone(89.4), "warn");
  assert.equal(quotaTone(90), "critical");
  // The tone follows the printed percentage: "80%" is amber, "90%" red.
  assert.equal(formatPercent(79.6), "80%");
  assert.equal(quotaTone(79.6), "warn");
  assert.equal(formatPercent(89.6), "90%");
  assert.equal(quotaTone(89.6), "critical");
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

const ok = (...used) => ({
  status: "ok",
  windows: used.map((usedPercent) => ({ usedPercent })),
});

test("tightestQuota takes the highest window of any agent with a reading", () => {
  assert.equal(tightestQuota(null), null);
  assert.equal(tightestQuota([]), null);
  assert.equal(tightestQuota([{ quota: { status: "error", windows: [] } }]), null);
  assert.equal(tightestQuota([{ quota: ok() }]), null);
  // 0% is a reading, not its absence.
  assert.equal(tightestQuota([{ quota: ok(0) }]), 0);
  assert.equal(tightestQuota([{ quota: ok(12, 61) }, { quota: ok(40) }, { quota: null }]), 61);
  // A window still unread by a failed probe does not count.
  assert.equal(
    tightestQuota([{ quota: { status: "unavailable", windows: [{ usedPercent: 99 }] } }]),
    null,
  );
});

test("popoverAgents lists installed built-ins, tightest first, then by name", () => {
  // `found` is the program's path, or null, as the route sends it.
  const agents = [
    { name: "Gemini", builtIn: true, found: "/bin/gemini", quota: { status: "unavailable" } },
    { name: "Claude", builtIn: true, found: "/bin/claude", quota: ok(20, 62) },
    { name: "Local", builtIn: false, found: "/bin/local", quota: ok(99) },
    // Enabled with past runs but not installed: never probed, so not listed.
    {
      name: "Grok",
      builtIn: true,
      found: null,
      enabled: true,
      quota: { status: "unavailable" },
    },
    { name: "Codex", builtIn: true, found: "/bin/codex", quota: ok(80) },
    { name: "Amp", builtIn: true, found: "/bin/amp", quota: { status: "loading" } },
  ];
  assert.deepEqual(
    popoverAgents(agents).map((agent) => agent.name),
    ["Codex", "Claude", "Amp", "Gemini"],
  );
  assert.deepEqual(popoverAgents(null), []);
});

test("soonestReset picks the earliest reset and skips windows without one", () => {
  assert.equal(soonestReset(undefined), null);
  assert.equal(soonestReset([{ resetsAt: null }]), null);
  assert.equal(soonestReset([{ resetsAt: 300 }, { resetsAt: null }, { resetsAt: 120 }]), 120);
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
