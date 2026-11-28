import assert from "node:assert/strict";
import test from "node:test";

import {
  HUB_DIVIDER_GAPS,
  HUB_PANE_DEFAULTS,
  HUB_PANE_LIMITS,
  HUB_QUEUE_MIN,
  LANES,
  applyStageOverride,
  dragHubPane,
  filterHubItems,
  fitHubPanes,
  formatAge,
  groupByPriority,
  itemKey,
  laneLabel,
  laneSummary,
  memoryBackendSummary,
  resetHubPane,
  stepHubPane,
} from "./hub.js";

const ITEMS = [
  {
    repoPath: "/work/postil",
    repoName: "postil",
    kind: "issue",
    number: 84,
    title: "Race in worktree eviction lock order",
    labels: ["bug", "critical"],
    priority: "P0",
    priorityScore: 92,
    priorityReason: "critical/security signal",
    recommendedLane: "hard",
  },
  {
    repoPath: "/work/hippius-mem",
    repoName: "hippius-mem",
    kind: "pullRequest",
    number: 112,
    title: "Add S3 retry backoff",
    labels: ["s3"],
    priority: "P1",
    priorityScore: 76,
    priorityReason: "ready for review",
    recommendedLane: "middle",
  },
  {
    repoPath: "/work/postil",
    repoName: "postil",
    kind: "issue",
    number: 86,
    title: "Fix typo in Settings tooltip",
    labels: ["docs"],
    priority: "P3",
    priorityScore: 28,
    priorityReason: "low-urgency docs",
    recommendedLane: "easy",
  },
];

test("filterHubItems filters by kind, priority, and text query", () => {
  assert.equal(filterHubItems(ITEMS).length, 3);
  assert.equal(filterHubItems(ITEMS, { kind: "pullRequest" }).length, 1);
  assert.equal(filterHubItems(ITEMS, { priority: "P0" }).length, 1);
  assert.deepEqual(
    filterHubItems(ITEMS, { query: "#112" }).map((i) => i.number),
    [112],
  );
  assert.deepEqual(
    filterHubItems(ITEMS, { query: "lock" }).map((i) => i.number),
    [84],
  );
});

test("groupByPriority preserves P0..P3 ordering and omits empty buckets", () => {
  const groups = groupByPriority(ITEMS);
  assert.deepEqual(
    groups.map((g) => g.priority),
    ["P0", "P1", "P3"],
  );
  assert.equal(groups[0].items[0].number, 84);
});

test("laneSummary and laneLabel describe all lanes for issues and PRs", () => {
  assert.deepEqual(LANES, ["easy", "middle", "hard", "custom"]);
  assert.equal(laneLabel("easy"), "Easy");
  assert.equal(laneLabel("middle"), "Middle");
  assert.equal(laneLabel("hard"), "Hard");
  assert.equal(laneLabel("custom"), "Custom");
  assert.match(laneSummary("easy", "issue"), /no review/);
  assert.match(laneSummary("hard", "issue"), /2 Frontier/);
  assert.match(laneSummary("hard", "pullRequest"), /2 Frontier audits/);
  assert.match(laneSummary("custom", "issue"), /Customized stage roster/);
});

test("applyStageOverride updates agent/model and recomputes diversity flags", () => {
  const stages = [
    { label: "Implementer", agent: "Claude", model: "sonnet-4", diverse: false },
    { label: "Peer Reviewer", agent: "Codex", model: "o3", diverse: true },
  ];
  const sameAgent = applyStageOverride(stages, 1, { agent: "Claude", model: "sonnet-4" });
  assert.equal(sameAgent[1].agent, "Claude");
  assert.equal(sameAgent[1].model, "sonnet-4");
  assert.equal(sameAgent[1].diverse, false);

  const switchedAgent = applyStageOverride(stages, 0, { agent: "Codex" });
  assert.equal(switchedAgent[0].agent, "Codex");
  assert.equal(switchedAgent[0].model, null);
  assert.equal(switchedAgent[1].diverse, true, "Codex(null) vs Codex(o3) is distinct");
});

test("memoryBackendSummary distinguishes local trial vault vs paid remote S3", () => {
  assert.equal(
    memoryBackendSummary({ state: { kind: "ready" }, backend: { kind: "local" } }).detail,
    "Local trial vault",
  );
  assert.equal(
    memoryBackendSummary({
      state: { kind: "ready" },
      backend: { kind: "s3", bucket: "team-vault" },
    }).detail,
    "Remote S3 · team-vault",
  );
  assert.equal(memoryBackendSummary({ state: { kind: "missing" } }).kind, "absent");
  assert.equal(
    memoryBackendSummary({ state: { kind: "unconfigured", problem: "bad key" } }).kind,
    "missing",
  );
  assert.equal(memoryBackendSummary({ state: { kind: "unconfigured" } }).kind, "unchecked");
});

test("formatAge and itemKey format compact timestamps and stable keys", () => {
  const now = Date.UTC(2026, 10, 28, 14, 0, 0);
  assert.equal(formatAge(Math.floor(now / 1000) - 30, now), "just now");
  assert.equal(formatAge(Math.floor(now / 1000) - 7200, now), "2h ago");
  assert.equal(itemKey(ITEMS[0]), "/work/postil::issue::84");
});

test("fitHubPanes shrinks inspector then repos to preserve queue minimum", () => {
  assert.deepEqual(fitHubPanes({ repos: 300, inspector: 460 }, 1400), {
    repos: 300,
    inspector: 460,
  });
  // 300 + 460 + 260 + 16 = 1036; in a 900 px shell, 136 px comes from inspector first.
  assert.deepEqual(fitHubPanes({ repos: 300, inspector: 460 }, 900), {
    repos: 300,
    inspector: 324,
  });
  // In an 800 px shell, inspector stops at 280 (-180) and repos gives the remaining 56.
  assert.deepEqual(fitHubPanes({ repos: 300, inspector: 460 }, 800), {
    repos: 244,
    inspector: 280,
  });
  // Never below minimums.
  assert.deepEqual(fitHubPanes({ repos: 300, inspector: 460 }, 500), {
    repos: HUB_PANE_LIMITS.repos[0],
    inspector: HUB_PANE_LIMITS.inspector[0],
  });
});

test("dragHubPane, stepHubPane, and resetHubPane match main pane divider semantics", () => {
  const start = { ...HUB_PANE_DEFAULTS };
  assert.deepEqual(dragHubPane(start, "repos", 324.4, 1400), { repos: 320, inspector: 360 });
  assert.deepEqual(dragHubPane(start, "inspector", 996, 1400), { repos: 250, inspector: 400 });

  // Respects room left for queue minimum in a narrow shell.
  const narrow = dragHubPane(start, "repos", 600, 900);
  assert.equal(narrow.repos, 900 - HUB_QUEUE_MIN - HUB_DIVIDER_GAPS - HUB_PANE_LIMITS.inspector[0]);

  // Arrow keys: right widens repos and narrows inspector.
  assert.equal(stepHubPane(start, "repos", 16, 1400).repos, 266);
  assert.equal(stepHubPane(start, "inspector", 16, 1400).inspector, 344);
  assert.equal(stepHubPane(start, "inspector", -64, 1400).inspector, 424);

  // Double-click resets only the chosen pane.
  assert.deepEqual(resetHubPane({ repos: 400, inspector: 500 }, "repos"), {
    repos: HUB_PANE_DEFAULTS.repos,
    inspector: 500,
  });
  assert.deepEqual(resetHubPane({ repos: 400, inspector: 500 }, "inspector"), {
    repos: 400,
    inspector: HUB_PANE_DEFAULTS.inspector,
  });
});
