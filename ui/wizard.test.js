import assert from "node:assert/strict";
import test from "node:test";

import {
  STEPS,
  applyDetectedDefaults,
  clampStep,
  npxWarning,
  readinessItems,
  removeWizardCustomAgent,
  saveWizardCustomAgent,
  toggleWizardAgent,
  wizardAgentRows,
  wizardSummary,
} from "./wizard.js";

const BUILT_IN = ["Claude", "Codex", "Gemini", "Grok", "OpenCode"];

function defaults() {
  return {
    theme: "system",
    codeFontSize: 13,
    tabWidth: 4,
    reopenLastProject: false,
    customAgents: [],
    enabledAgents: [],
    agent: null,
    analyzer: null,
    reviewers: [],
  };
}

const slot = (name, model = null) => ({ name, model });

function scannedAgents() {
  return [
    {
      name: "Claude",
      command: "npx -y @agentclientprotocol/claude-agent-acp",
      builtIn: true,
      found: "/usr/local/bin/claude",
    },
    { name: "Codex", command: "npx -y @zed-industries/codex-acp", builtIn: true, found: null },
    { name: "Gemini", command: "agy acp", builtIn: true, found: "/Users/me/.local/bin/agy" },
    { name: "Grok", command: "grok agent stdio", builtIn: true, found: null },
    { name: "OpenCode", command: "opencode acp", builtIn: true, found: null },
  ];
}

test("step clamping stays inside the wizard's steps", () => {
  assert.equal(clampStep(-3), 0);
  assert.equal(clampStep(1), 1);
  assert.equal(clampStep(99), STEPS.length - 1);
});

test("first-run defaults enable every found built-in agent and pick the first as default", () => {
  const next = applyDetectedDefaults(defaults(), scannedAgents());
  assert.deepEqual(next.enabledAgents, ["Claude", "Gemini"]);
  assert.deepEqual(next.agent, slot("Claude"));

  // Does not overwrite choices once an agent is already enabled or selected.
  const custom = { ...defaults(), enabledAgents: ["Gemini"], agent: slot("Gemini") };
  assert.equal(applyDetectedDefaults(custom, scannedAgents()), custom);

  // Leaves the draft unchanged when no built-in agent is found.
  const none = scannedAgents().map((agent) => ({ ...agent, found: null }));
  const empty = defaults();
  assert.equal(applyDetectedDefaults(empty, none), empty);
});

test("toggling an agent in the wizard reconciles the default agent and reviewers", () => {
  let draft = defaults();
  draft = toggleWizardAgent(draft, "Claude", true);
  assert.deepEqual(draft.enabledAgents, ["Claude"]);
  assert.deepEqual(draft.agent, slot("Claude"), "first enabled agent becomes default");

  draft = toggleWizardAgent(draft, "Gemini", true);
  assert.deepEqual(draft.enabledAgents, ["Claude", "Gemini"]);
  assert.deepEqual(draft.agent, slot("Claude"), "existing default is preserved");

  draft = {
    ...draft,
    agent: slot("Claude", "opus"),
    analyzer: slot("Gemini", "gemini-4-carbon"),
    reviewers: [slot("Claude"), slot("Gemini", "gemini-4-neon"), slot("Claude", "haiku")],
  };
  const analyzerOff = toggleWizardAgent(draft, "Gemini", false);
  assert.equal(analyzerOff.analyzer, null, "analyses go to the default agent then");
  draft = toggleWizardAgent(draft, "Claude", false);
  assert.deepEqual(draft.analyzer, slot("Gemini", "gemini-4-carbon"), "another's stays");
  assert.deepEqual(draft.enabledAgents, ["Gemini"]);
  assert.deepEqual(
    draft.agent,
    slot("Gemini"),
    "default falls back to the next enabled agent, on its own default model",
  );
  assert.deepEqual(
    draft.reviewers,
    [slot("Gemini", "gemini-4-neon")],
    "disabled agent is removed from reviewers, the others keep their models",
  );

  draft = toggleWizardAgent(draft, "Gemini", false);
  assert.deepEqual(draft.enabledAgents, []);
  assert.equal(draft.agent, null);
  assert.deepEqual(draft.reviewers, []);
});

test("custom agents added in the wizard become the default when none is set and remove cleanly", () => {
  const added = saveWizardCustomAgent(
    defaults(),
    { name: " Local ", command: " local-agent --acp " },
    null,
    BUILT_IN,
  );
  assert.equal(added.error, undefined);
  assert.deepEqual(added.settings.customAgents, [{ name: "Local", command: "local-agent --acp" }]);
  assert.deepEqual(added.settings.enabledAgents, ["Local"]);
  assert.deepEqual(added.settings.agent, slot("Local"));

  const withReviewer = { ...added.settings, reviewers: [slot("Local", "big")] };
  const renamed = saveWizardCustomAgent(
    withReviewer,
    { name: "Renamed", command: "renamed-agent --acp" },
    "Local",
    BUILT_IN,
  );
  assert.equal(renamed.error, undefined);
  assert.deepEqual(renamed.settings.agent, slot("Renamed"));
  assert.deepEqual(renamed.settings.reviewers, [slot("Renamed", "big")]);

  const removed = removeWizardCustomAgent(renamed.settings, "Renamed");
  assert.deepEqual(removed.customAgents, []);
  assert.deepEqual(removed.enabledAgents, []);
  assert.equal(removed.agent, null);
  assert.deepEqual(removed.reviewers, []);
});

test("wizardAgentRows merges unsaved custom agents and marks them pending PATH check", () => {
  const server = [
    ...scannedAgents(),
    { name: "Saved", command: "saved-agent --acp", builtIn: false, found: "/bin/saved-agent" },
  ];
  const draft = {
    ...defaults(),
    customAgents: [
      { name: "Saved", command: "saved-agent --acp" },
      { name: "Fresh", command: "fresh-agent --acp" },
    ],
    enabledAgents: ["Claude", "Saved", "Fresh"],
    agent: slot("Claude"),
  };
  const rows = wizardAgentRows(server, draft);
  const savedRow = rows.find((row) => row.name === "Saved");
  const freshRow = rows.find((row) => row.name === "Fresh");
  assert.equal(savedRow.found, "/bin/saved-agent");
  assert.equal(savedRow.pendingCheck, false);
  assert.equal(freshRow.found, null);
  assert.equal(freshRow.pendingCheck, true);
  assert.equal(freshRow.enabled, true);
});

test("npxWarning fires only when an enabled agent uses npx and npx is missing", () => {
  const agents = scannedAgents();
  const withClaude = { ...defaults(), enabledAgents: ["Claude"], agent: slot("Claude") };
  const withGemini = { ...defaults(), enabledAgents: ["Gemini"], agent: slot("Gemini") };
  const withBoth = { ...defaults(), enabledAgents: ["Claude", "Codex"], agent: slot("Claude") };

  assert.equal(
    npxWarning(withClaude, agents, { git: "/usr/bin/git", npx: "/usr/bin/npx", gh: null }),
    null,
  );
  assert.equal(npxWarning(withGemini, agents, { git: "/usr/bin/git", npx: null, gh: null }), null);
  assert.match(
    npxWarning(withClaude, agents, { git: "/usr/bin/git", npx: null, gh: null }),
    /Claude uses npx.*start it\./,
  );
  assert.match(
    npxWarning(withBoth, agents, { git: "/usr/bin/git", npx: null, gh: null }),
    /Claude and Codex use npx.*start them\./,
  );
  assert.deepEqual(readinessItems(null, false), []);
});

test("wizardSummary and readinessItems describe the chosen settings and tools", () => {
  const draft = {
    ...defaults(),
    theme: "dark",
    codeFontSize: 15,
    tabWidth: 2,
    reopenLastProject: true,
    enabledAgents: ["Claude", "Gemini"],
    agent: slot("Claude", "opus"),
    reviewers: [slot("Gemini", "gemini-4-neon"), slot("Claude")],
  };
  // Claude reported its models; Gemini's probe is still running, so its id stands.
  const models = {
    Claude: { status: "ready", current: "opus", options: [{ value: "opus", name: "Opus 5.5" }] },
    Gemini: { status: "loading" },
  };
  assert.deepEqual(
    wizardSummary(draft, models).map((item) => [item.label, item.value]),
    [
      ["Enabled agents", "Claude, Gemini"],
      ["Send notes to", "Claude · Opus 5.5"],
      ["Reviewers", "Gemini · gemini-4-neon → Claude"],
      ["Appearance", "Dark · 15 px · 2-space tabs"],
      ["Startup", "Reopen the last project"],
    ],
  );
  const unset = wizardSummary(defaults(), {});
  assert.deepEqual(
    unset.filter((item) => item.muted).map((item) => [item.label, item.value]),
    [
      ["Enabled agents", "None"],
      ["Send notes to", "Not chosen yet"],
      ["Reviewers", "None (review changes yourself)"],
    ],
  );

  const tools = readinessItems({ git: "/usr/bin/git", npx: null, gh: null }, true);
  assert.deepEqual(
    tools.map((tool) => [tool.name, tool.status]),
    [
      ["git", "ok"],
      ["npx", "warning"],
      ["gh", "muted"],
    ],
  );
});
