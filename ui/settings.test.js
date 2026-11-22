import assert from "node:assert/strict";
import test from "node:test";

import {
  addReviewer,
  agentRows,
  codeMetrics,
  moveReviewer,
  removeCustomAgent,
  removeReviewer,
  saveCustomAgent,
  setAgent,
  setEnabled,
} from "./settings.js";

const BUILT_IN = ["Claude", "Codex", "Gemini", "Grok", "OpenCode"];

// Settings as api/settings returns them, with one custom agent in use as the agent and Grok
// reviewing twice.
function settings() {
  return {
    theme: "system",
    codeFontSize: 13,
    tabWidth: 4,
    reopenLastProject: false,
    customAgents: [{ name: "Local", command: "local-agent --acp" }],
    enabledAgents: ["Grok", "Local", "Claude"],
    agent: "Local",
    reviewers: ["Grok", "Grok"],
  };
}

test("the line height keeps the 22 px to 13 px proportion at every offered size", () => {
  assert.deepEqual(codeMetrics(13), { fontSize: 13, lineHeight: 22 });
  assert.equal(codeMetrics(11).lineHeight, 19);
  assert.equal(codeMetrics(20).lineHeight, 34);
});

test("rows say which agents are enabled and what each one is used for", () => {
  const agents = [
    { name: "Claude", command: "c", builtIn: true, found: "/bin/claude" },
    { name: "Grok", command: "g", builtIn: true, found: null },
    { name: "Gemini", command: "m", builtIn: true, found: null },
    { name: "Local", command: "l", builtIn: false, found: null },
  ];
  const rows = agentRows(agents, settings());
  assert.deepEqual(
    rows.map((row) => [row.name, row.enabled, row.usage]),
    [
      ["Claude", true, null],
      ["Grok", true, "it reviews the agent's work"],
      ["Gemini", false, null],
      ["Local", true, "notes are sent to it"],
    ],
  );
  assert.equal(rows[0].found, "/bin/claude", "the scan's fields are kept");
});

test("an agent can be turned off only while nothing uses it", () => {
  assert.deepEqual(setEnabled(settings(), "Claude", false).settings.enabledAgents, [
    "Grok",
    "Local",
  ]);
  assert.match(setEnabled(settings(), "Local", false).error, /while notes are sent to it/);
  assert.match(setEnabled(settings(), "Grok", false).error, /while it reviews/);
  assert.deepEqual(setEnabled(settings(), "Gemini", true).settings.enabledAgents, [
    "Grok",
    "Local",
    "Claude",
    "Gemini",
  ]);
  const same = settings();
  assert.equal(setEnabled(same, "Grok", true).settings, same, "no change, same object");
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
  for (const command of ['run "a b"', "run 'a'", "run a\\ b"]) {
    assert.match(refused({ name: "New", command }), /cannot hold quotes/, command);
  }
});

test("renaming a custom agent renames it wherever it is chosen", () => {
  // Local is the agent and also the second of three reviewers.
  const before = { ...settings(), reviewers: ["Grok", "Local", "Claude"] };
  const renamed = saveCustomAgent(
    before,
    { name: "Mine", command: "mine --acp" },
    "Local",
    BUILT_IN,
  ).settings;
  assert.deepEqual(renamed.customAgents, [{ name: "Mine", command: "mine --acp" }]);
  assert.deepEqual(renamed.enabledAgents, ["Grok", "Mine", "Claude"]);
  assert.equal(renamed.agent, "Mine");
  assert.deepEqual(renamed.reviewers, ["Grok", "Mine", "Claude"]);

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
  assert.equal(setAgent(settings(), "Claude").settings.agent, "Claude");
  assert.equal(setAgent(settings(), null).settings.agent, null);
  assert.match(setAgent(settings(), "Gemini").error, /Turn Gemini on first/);
  const reviewers = addReviewer(settings(), "Claude").settings.reviewers;
  assert.deepEqual(reviewers, ["Grok", "Grok", "Claude"]);
  assert.match(addReviewer(settings(), "Gemini").error, /Turn Gemini on first/);
});

test("reviewers are removed by position and moved within the list's ends", () => {
  const three = addReviewer(settings(), "Claude").settings;
  assert.deepEqual(removeReviewer(three, 0).settings.reviewers, ["Grok", "Claude"]);
  assert.deepEqual(moveReviewer(three, 2, -1).settings.reviewers, ["Grok", "Claude", "Grok"]);
  assert.deepEqual(moveReviewer(three, 0, 1).settings.reviewers, ["Grok", "Grok", "Claude"]);
  assert.equal(moveReviewer(three, 0, -1).settings, three, "the first cannot move up");
  assert.equal(moveReviewer(three, 2, 1).settings, three, "the last cannot move down");
});
