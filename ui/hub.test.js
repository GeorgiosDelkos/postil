import assert from "node:assert/strict";
import test from "node:test";

import {
  filterHubItems,
  formatAge,
  groupByPriority,
  itemKey,
  laneLabel,
  laneSummary,
  memoryBackendSummary,
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
  assert.equal(laneLabel("easy"), "Easy");
  assert.equal(laneLabel("middle"), "Middle");
  assert.equal(laneLabel("hard"), "Hard");
  assert.match(laneSummary("easy", "issue"), /no review/);
  assert.match(laneSummary("hard", "issue"), /2 Frontier/);
  assert.match(laneSummary("hard", "pullRequest"), /2 Frontier audits/);
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
});

test("formatAge and itemKey format compact timestamps and stable keys", () => {
  const now = Date.UTC(2026, 10, 28, 14, 0, 0);
  assert.equal(formatAge(Math.floor(now / 1000) - 30, now), "just now");
  assert.equal(formatAge(Math.floor(now / 1000) - 7200, now), "2h ago");
  assert.equal(itemKey(ITEMS[0]), "/work/postil::issue::84");
});
