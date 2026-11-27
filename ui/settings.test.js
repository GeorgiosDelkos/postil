import assert from "node:assert/strict";
import test from "node:test";

import {
  addReviewer,
  agentRows,
  agentState,
  codeMetrics,
  formatSlot,
  canSetUp,
  memoryState,
  moveReviewer,
  removeCustomAgent,
  removeReviewer,
  saveCustomAgent,
  setAgent,
  setAgentModel,
  setAnalyzer,
  setAnalyzerModel,
  setAgentMemory,
  setEnabled,
  setReviewerModel,
} from "./settings.js";
import { modelSelectOptions } from "./settings-controls.js";

const BUILT_IN = ["Claude", "Codex", "Antigravity", "Grok", "OpenCode"];

// Settings as api/settings returns them, with one custom agent in use as the agent and Grok
// reviewing twice, once on a chosen model.
function settings() {
  return {
    theme: "system",
    codeFontSize: 13,
    tabWidth: 4,
    reopenLastProject: false,
    customAgents: [{ name: "Local", command: "local-agent --acp" }],
    enabledAgents: ["Grok", "Local", "Claude"],
    agent: { name: "Local", model: null },
    analyzer: null,
    reviewers: [
      { name: "Grok", model: "grok-4.7" },
      { name: "Grok", model: null },
    ],
    agentMemory: false,
  };
}

const slot = (name, model = null) => ({ name, model });

// Their names, for the tests that check only which agents a list holds.
const names = (slots) => slots.map((each) => each.name);

test("the line height keeps the 22 px to 13 px proportion at every offered size", () => {
  assert.deepEqual(codeMetrics(13), { fontSize: 13, lineHeight: 22 });
  assert.equal(codeMetrics(11).lineHeight, 19);
  assert.equal(codeMetrics(20).lineHeight, 34);
});

test("rows say which agents are enabled and what each one is used for", () => {
  const agents = [
    { name: "Claude", command: "c", builtIn: true, found: "/bin/claude" },
    { name: "Grok", command: "g", builtIn: true, found: null },
    { name: "Antigravity", command: "m", builtIn: true, found: null },
    { name: "Local", command: "l", builtIn: false, found: null },
  ];
  const rows = agentRows(agents, settings());
  assert.deepEqual(
    rows.map((row) => [row.name, row.enabled, row.usage]),
    [
      ["Claude", true, null],
      ["Grok", true, "it reviews the agent's work"],
      ["Antigravity", false, null],
      ["Local", true, "notes are sent to it"],
    ],
  );
  assert.equal(rows[0].found, "/bin/claude", "the scan's fields are kept");
});

test("a row's state: found first, then a pending check, then on but missing", () => {
  const state = (row) => agentState(row).kind;
  // Found wins over a pending check and over being on.
  assert.equal(state({ found: "/bin/x", pendingCheck: true, enabled: true }), "installed");
  assert.equal(state({ found: null, pendingCheck: true, enabled: true }), "unchecked");
  assert.equal(state({ found: null, enabled: true }), "missing");
  assert.equal(state({ found: null, enabled: false }), "absent");
  assert.equal(agentState({ found: null, enabled: true }).label, "Not found on PATH");
  assert.equal(agentState({ found: null, enabled: false }).label, "Not installed");
});

test("an agent can be turned off only while nothing uses it", () => {
  assert.deepEqual(setEnabled(settings(), "Claude", false).settings.enabledAgents, [
    "Grok",
    "Local",
  ]);
  assert.match(setEnabled(settings(), "Local", false).error, /while notes are sent to it/);
  assert.match(setEnabled(settings(), "Grok", false).error, /while it reviews/);
  assert.deepEqual(setEnabled(settings(), "Antigravity", true).settings.enabledAgents, [
    "Grok",
    "Local",
    "Claude",
    "Antigravity",
  ]);
  const same = settings();
  assert.equal(setEnabled(same, "Grok", true).settings, same, "no change, same object");
});

test("the analyzer is chosen among enabled agents and cannot be turned off or removed", () => {
  const chosen = setAnalyzer(settings(), "Claude").settings;
  assert.deepEqual(chosen.analyzer, slot("Claude"));
  assert.match(setEnabled(chosen, "Claude", false).error, /while it analyzes the repository/);
  assert.match(setAnalyzer(settings(), "Gemini").error, /Turn Gemini on first/);
  const fast = setAnalyzerModel(chosen, "fast").settings;
  assert.deepEqual(fast.analyzer, slot("Claude", "fast"));
  assert.deepEqual(setAnalyzer(fast, "Claude").settings.analyzer, slot("Claude", "fast"));
  assert.deepEqual(setAnalyzer(fast, "Grok").settings.analyzer, slot("Grok"));
  assert.equal(setAnalyzer(fast, null).settings.analyzer, null);
  assert.match(setAnalyzerModel(settings(), "x").error, /Choose the analyzer/);

  const custom = setAnalyzer(setAgent(settings(), "Claude").settings, "Local").settings;
  assert.match(removeCustomAgent(custom, "Local").error, /cannot be removed while it analyzes/);
  const renamed = saveCustomAgent(
    setAnalyzerModel(custom, "big").settings,
    { name: "Mine", command: "mine --acp" },
    "Local",
    BUILT_IN,
  ).settings;
  assert.deepEqual(renamed.analyzer, slot("Mine", "big"), "renamed, its model kept");
});

test("a new custom agent is trimmed, enabled, and checked as the Rust process checks it", () => {
  const typed = { name: " Aider ", command: " aider --acp " };
  const added = saveCustomAgent(settings(), typed, null, BUILT_IN);
  assert.deepEqual(added.settings.customAgents.at(-1), { name: "Aider", command: "aider --acp" });
  assert.deepEqual(added.settings.enabledAgents.at(-1), "Aider");

  const refused = (agent) => saveCustomAgent(settings(), agent, null, BUILT_IN).error;
  assert.match(refused({ name: "  ", command: "x" }), /Give the agent a name/);
  assert.match(refused({ name: "a".repeat(41), command: "x" }), /at most 40 characters/);
  assert.equal(refused({ name: "a".repeat(40), command: "x" }), undefined, "40 is allowed");
  assert.match(refused({ name: "grok", command: "x" }), /already an agent called grok/);
  assert.match(refused({ name: "LOCAL", command: "x" }), /already an agent called LOCAL/);
  assert.match(refused({ name: "New", command: "   " }), /Give the command/);
  assert.match(
    refused({ name: "New", command: "./bin/agent --acp" }),
    /on PATH or an absolute path, not \.\/bin\/agent/,
  );
  assert.equal(refused({ name: "New", command: "/opt/bin/agent --acp" }), undefined);
  for (const command of ['run "a b"', "run 'a'", "run a\\ b"]) {
    assert.match(refused({ name: "New", command }), /cannot hold quotes/, command);
  }
});

test("renaming a custom agent renames it wherever it is chosen", () => {
  // Local is the agent, on a model, and also the second of three reviewers, on another.
  const before = {
    ...settings(),
    agent: slot("Local", "big"),
    reviewers: [slot("Grok"), slot("Local", "small"), slot("Claude")],
  };
  const renamed = saveCustomAgent(
    before,
    { name: "Mine", command: "mine --acp" },
    "Local",
    BUILT_IN,
  ).settings;
  assert.deepEqual(renamed.customAgents, [{ name: "Mine", command: "mine --acp" }]);
  assert.deepEqual(renamed.enabledAgents, ["Grok", "Mine", "Claude"]);
  assert.deepEqual(renamed.agent, slot("Mine", "big"), "the model goes with the agent");
  assert.deepEqual(renamed.reviewers, [slot("Grok"), slot("Mine", "small"), slot("Claude")]);

  const kept = saveCustomAgent(settings(), { name: "Local", command: "other" }, "Local", BUILT_IN);
  assert.equal(kept.error, undefined, "keeping its own name is not a clash");
  assert.deepEqual(kept.settings.customAgents, [{ name: "Local", command: "other" }]);
});

test("a custom agent in use cannot be removed; a free one leaves every list", () => {
  assert.match(removeCustomAgent(settings(), "Local").error, /cannot be removed while notes/);
  const free = setAgent(settings(), null).settings;
  const removed = removeCustomAgent(free, "Local").settings;
  assert.deepEqual(removed.customAgents, []);
  assert.deepEqual(removed.enabledAgents, ["Grok", "Claude"]);
});

test("the agent and reviewers are chosen among enabled agents only", () => {
  assert.deepEqual(setAgent(settings(), "Claude").settings.agent, slot("Claude"));
  assert.equal(setAgent(settings(), null).settings.agent, null);
  assert.match(setAgent(settings(), "Antigravity").error, /Turn Antigravity on first/);
  const reviewers = addReviewer(settings(), "Claude").settings.reviewers;
  assert.deepEqual(reviewers.at(-1), slot("Claude"), "a new reviewer runs its default model");
  assert.deepEqual(names(reviewers), ["Grok", "Grok", "Claude"]);
  assert.match(addReviewer(settings(), "Antigravity").error, /Turn Antigravity on first/);
});

test("switching the agent resets its model, and choosing the same one keeps it", () => {
  const chosen = setAgentModel(settings(), "fast").settings;
  assert.deepEqual(chosen.agent, slot("Local", "fast"));
  assert.deepEqual(setAgent(chosen, "Local").settings.agent, slot("Local", "fast"));
  assert.deepEqual(setAgent(chosen, "Claude").settings.agent, slot("Claude"));
  assert.deepEqual(setAgentModel(chosen, null).settings.agent, slot("Local"));
  assert.match(setAgentModel(setAgent(chosen, null).settings, "x").error, /Choose the agent/);
});

test("each reviewer entry has its own model, even for the same agent", () => {
  const both = setReviewerModel(settings(), 1, "grok-4.6").settings;
  assert.deepEqual(both.reviewers, [slot("Grok", "grok-4.7"), slot("Grok", "grok-4.6")]);
  assert.deepEqual(setReviewerModel(both, 0, null).settings.reviewers[0], slot("Grok"));
  assert.deepEqual(settings().reviewers[1], slot("Grok"), "the settings given are not changed");
  // A move carries the model with its reviewer.
  assert.deepEqual(moveReviewer(both, 1, -1).settings.reviewers[0], slot("Grok", "grok-4.6"));
});

test("a slot reads as its agent, then its model's name when the agent reported it", () => {
  const ready = {
    status: "ready",
    current: "opus",
    options: [{ value: "opus", name: "Opus 5.5" }],
  };
  assert.equal(formatSlot(slot("Claude", "opus"), ready), "Claude · Opus 5.5");
  assert.equal(formatSlot(slot("Claude", "opus"), undefined), "Claude · opus");
  assert.equal(formatSlot(slot("Claude", "sonnet"), ready), "Claude · sonnet");
  assert.equal(formatSlot(slot("Claude"), ready), "Claude", "the agent's default names no model");
});

test("a model select never drops the saved model, and says when the agent stopped offering it", () => {
  const ready = {
    status: "ready",
    current: "opus",
    options: [
      { value: "opus", name: "Opus 5.5" },
      { value: "sonnet", name: "Sonnet 5.5" },
    ],
  };
  assert.deepEqual(modelSelectOptions(ready, null), [
    ["", "Agent default"],
    ["opus", "Opus 5.5"],
    ["sonnet", "Sonnet 5.5"],
  ]);
  assert.deepEqual(modelSelectOptions(ready, "sonnet"), modelSelectOptions(ready, null));
  // A stored id the agent no longer offers stays chosen, marked, so the run's failure is no
  // surprise and the engineer picks another (team note: such ids broke runs silently).
  assert.deepEqual(modelSelectOptions(ready, "gemini-2.5-pro").at(-1), [
    "gemini-2.5-pro",
    "gemini-2.5-pro (not offered)",
  ]);
  for (const report of [{ status: "loading" }, { status: "failed", error: "x" }, undefined]) {
    assert.deepEqual(modelSelectOptions(report, "opus"), [
      ["", "Agent default"],
      ["opus", "opus"],
    ]);
    assert.deepEqual(modelSelectOptions(report, null), [["", "Agent default"]]);
  }
  const none = { status: "ready", current: "m", options: [] };
  assert.deepEqual(modelSelectOptions(none, "pro").at(-1), ["pro", "pro (not offered)"]);
});

test("reviewers are removed by position and moved within the list's ends", () => {
  const three = addReviewer(settings(), "Claude").settings;
  assert.deepEqual(names(removeReviewer(three, 0).settings.reviewers), ["Grok", "Claude"]);
  assert.deepEqual(names(moveReviewer(three, 2, -1).settings.reviewers), [
    "Grok",
    "Claude",
    "Grok",
  ]);
  assert.deepEqual(names(moveReviewer(three, 0, 1).settings.reviewers), ["Grok", "Grok", "Claude"]);
  assert.equal(moveReviewer(three, 0, -1).settings, three, "the first cannot move up");
  assert.equal(moveReviewer(three, 2, 1).settings, three, "the last cannot move down");
});

test("team memory turns on only when hippius-mem is ready, and off in any state", () => {
  const ready = { kind: "ready", path: "/bin/hippius-mem" };
  const unconfigured = { kind: "unconfigured", path: "/bin/hippius-mem", problem: null };
  assert.equal(setAgentMemory(settings(), true, ready).settings.agentMemory, true);
  for (const state of [unconfigured, { kind: "missing" }, null]) {
    assert.match(setAgentMemory(settings(), true, state).error, /Set up hippius-mem/);
    const on = { ...settings(), agentMemory: true };
    assert.equal(setAgentMemory(on, false, state).settings.agentMemory, false);
  }
});

test("hippius-mem's mark says missing only while team memory is on", () => {
  const at = (state, on = false) => memoryState({ state }, on).kind;
  assert.equal(at(null), "unchecked");
  assert.equal(at({ kind: "ready", path: "/p" }), "installed");
  assert.equal(at({ kind: "unconfigured", path: "/p", problem: null }), "unchecked");
  assert.equal(at({ kind: "missing" }), "absent");
  assert.equal(at({ kind: "missing" }, true), "missing");
});

test("Set up is offered only when doctor found no configuration, whatever its exit code", () => {
  const unconfigured = (problem) => ({ kind: "unconfigured", path: "/p", problem });
  const noConfig =
    "Error: failed to load configuration; run `hippius-mem quickstart` to set up a trial vault";
  const probeFailed = "INFO profile: team\nError: live encryption-boundary probe failed";
  assert.equal(canSetUp(unconfigured(null)), true);
  assert.equal(canSetUp(unconfigured(noConfig)), true);
  // A configured vault whose check failed: quickstart would only refuse.
  assert.equal(canSetUp(unconfigured(probeFailed)), false);
  assert.equal(canSetUp(unconfigured("hippius-mem doctor did not answer within 10 s")), false);
  assert.equal(memoryState({ state: unconfigured(probeFailed) }, false).kind, "missing");
  for (const state of [null, { kind: "missing" }, { kind: "ready", path: "/p" }]) {
    assert.equal(canSetUp(state), false);
  }
});
