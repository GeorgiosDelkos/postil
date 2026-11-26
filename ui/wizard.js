// The startup wizard's pure logic, without the page: step order, smart first-run defaults,
// draft agent toggling (which cleans up dependent selections instead of refusing), environment
// warnings, and the final summary. Tested with `node --test ui/`.

import { formatSlot, removeCustomAgent, saveCustomAgent, setEnabled } from "./settings.js";

export const STEPS = [
  { id: "agents", label: "Agents", title: "Connect your coding agents" },
  { id: "workflow", label: "Workflow", title: "Default agent and reviewers" },
  { id: "appearance", label: "Appearance", title: "Appearance and startup" },
  { id: "ready", label: "Ready", title: "Ready to review" },
];

// Clamps a step index into the wizard's steps.
export function clampStep(index) {
  return Math.max(0, Math.min(STEPS.length - 1, index));
}

// On a first run, once the agent scan finishes and before the user has toggled anything,
// turns on every installed built-in agent and picks the first one as the default.
export function applyDetectedDefaults(draft, agents) {
  if (!agents || draft.enabledAgents.length > 0 || draft.agent !== null) return draft;
  const installed = agents.filter((agent) => agent.builtIn && agent.found).map((a) => a.name);
  if (installed.length === 0) return draft;
  return {
    ...draft,
    enabledAgents: installed,
    agent: { name: installed[0], model: null },
  };
}

// Turns an agent on or off inside the wizard's draft. Unlike the Settings dialog (which refuses
// turning off an agent that is chosen as the default or a reviewer), the wizard reconciles the
// draft automatically: turning off the default moves it to the next enabled agent (or null) and
// removes it from reviewers and from the analyzer; turning on an agent while none is chosen
// picks it as the default.
// An agent that becomes the default runs its own default model.
export function toggleWizardAgent(draft, name, enabled) {
  if (enabled) {
    const next = setEnabled(draft, name, true).settings;
    return next.agent === null ? { ...next, agent: { name, model: null } } : next;
  }
  const enabledAgents = draft.enabledAgents.filter((each) => each !== name);
  const reviewers = draft.reviewers.filter((slot) => slot.name !== name);
  const fallback = enabledAgents.length ? { name: enabledAgents[0], model: null } : null;
  const agent = draft.agent?.name === name ? fallback : draft.agent;
  // Analyses then go to the default agent.
  const analyzer = draft.analyzer?.name === name ? null : (draft.analyzer ?? null);
  return { ...draft, enabledAgents, agent, analyzer, reviewers };
}

// Adds or edits a custom agent in the wizard's draft. Returns `{ settings }` or `{ error }`.
// When adding a new agent while no default agent is chosen, selects it as the default.
export function saveWizardCustomAgent(draft, agent, previous, builtInNames) {
  const result = saveCustomAgent(draft, agent, previous, builtInNames);
  if (result.error) return result;
  const settings = result.settings;
  if (previous === null && settings.agent === null) {
    const added = settings.customAgents.at(-1);
    return { settings: { ...settings, agent: { name: added.name, model: null } } };
  }
  return result;
}

// Removes a custom agent from the wizard's draft, clearing any references to it first.
export function removeWizardCustomAgent(draft, name) {
  const disabled = toggleWizardAgent(draft, name, false);
  return removeCustomAgent(disabled, name).settings;
}

// Builds the agent rows for the wizard's Step 1 by combining the server's scanned built-in
// agents with the draft's custom agents (which may include newly added or edited entries not
// saved to the Rust process yet).
export function wizardAgentRows(serverAgents, draft) {
  if (!serverAgents) return [];
  const builtIns = serverAgents.filter((agent) => agent.builtIn);
  const knownCustom = new Map(
    serverAgents.filter((agent) => !agent.builtIn).map((agent) => [agent.command, agent.found]),
  );
  const customs = draft.customAgents.map((agent) => {
    const checked = knownCustom.has(agent.command);
    return {
      name: agent.name,
      command: agent.command,
      builtIn: false,
      found: checked ? knownCustom.get(agent.command) : null,
      pendingCheck: !checked,
    };
  });
  return [...builtIns, ...customs].map((agent) => ({
    ...agent,
    enabled: draft.enabledAgents.includes(agent.name),
    usage: null,
  }));
}

// Warns when an enabled agent runs through `npx` (such as Claude or Codex) and `npx` was not
// found on PATH. Returns null when everything needed is present or the scan has not finished.
export function npxWarning(draft, serverAgents, tools) {
  if (!tools || tools.npx || !serverAgents) return null;
  const rows = wizardAgentRows(serverAgents, draft);
  const needsNpx = rows
    .filter((agent) => agent.enabled && agent.command.trim().split(/\s+/)[0] === "npx")
    .map((agent) => agent.name);
  if (needsNpx.length === 0) return null;
  const names =
    needsNpx.length === 1
      ? needsNpx[0]
      : `${needsNpx.slice(0, -1).join(", ")} and ${needsNpx.at(-1)}`;
  const verb = needsNpx.length === 1 ? "uses" : "use";
  const pronoun = needsNpx.length === 1 ? "it" : "them";
  return (
    `${names} ${verb} npx, which was not found on PATH. ` +
    `Install Node.js so Postil can start ${pronoun}.`
  );
}

// Formats the chosen draft settings for the Ready summary step. `models` are the agents'
// replies of api/agents/models, by name, for the models' display names.
export function wizardSummary(draft, models) {
  const themeLabel = { system: "System", light: "Light", dark: "Dark" }[draft.theme] ?? draft.theme;
  const slot = (each) => formatSlot(each, models[each.name]);
  return [
    {
      label: "Enabled agents",
      value: draft.enabledAgents.length ? draft.enabledAgents.join(", ") : "None",
      muted: draft.enabledAgents.length === 0,
    },
    {
      label: "Send notes to",
      value: draft.agent ? slot(draft.agent) : "Not chosen yet",
      muted: draft.agent === null,
    },
    {
      label: "Reviewers",
      value: draft.reviewers.length
        ? draft.reviewers.map(slot).join(" → ")
        : "None (review changes yourself)",
      muted: draft.reviewers.length === 0,
    },
    {
      label: "Appearance",
      value: `${themeLabel} · ${draft.codeFontSize} px · ${draft.tabWidth}-space tabs`,
      muted: false,
    },
    {
      label: "Startup",
      value: draft.reopenLastProject ? "Reopen the last project" : "Show the welcome screen",
      muted: false,
    },
  ];
}

// Summarizes external CLI tools found on PATH (`git`, `npx`, `gh`) for Step 4.
export function readinessItems(tools, needsNpx) {
  if (!tools) return [];
  return [
    {
      name: "git",
      found: tools.git,
      detail: tools.git ?? "Not found on PATH — required to open repositories",
      status: tools.git ? "ok" : "warning",
    },
    {
      name: "npx",
      found: tools.npx,
      detail:
        tools.npx ??
        (needsNpx
          ? "Not found on PATH — needed by your selected agent"
          : "Not installed — optional, needed for Claude and Codex adapters"),
      status: tools.npx ? "ok" : needsNpx ? "warning" : "muted",
    },
    {
      name: "gh",
      found: tools.gh,
      detail: tools.gh ?? "Not installed — optional, used to open draft pull requests",
      status: tools.gh ? "ok" : "muted",
    },
  ];
}
