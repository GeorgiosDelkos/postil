// The settings dialog's logic, without the page: what each change does to the settings, and
// which changes are refused before they are sent. The Rust process checks every rule again
// (src/settings.rs) and is the one that decides; the checks here only let the dialog explain
// a refusal next to the control that caused it, before any request.
//
// Every function takes the settings object as `api/settings` returns it and gives back a new
// one, `{ settings }`, or the reason the change is refused, `{ error }`. Nothing is changed in
// place, so a refused change leaves the dialog as it was.

// The longest custom agent name, in characters (src/settings.rs, MAX_NAME).
const MAX_NAME = 40;

// Text and line height of the code view for a font size. Lines keep the proportion the view
// was designed at, 22 px lines for 13 px text: enough room for underscores and the selection
// band without the lines drifting apart at large sizes.
export function codeMetrics(fontSize) {
  return { fontSize, lineHeight: Math.round((fontSize * 22) / 13) };
}

// The agent notes are sent to, the analyzer and each reviewer are slots, `{ name, model }`: an
// agent and the model it runs, `model` null for the agent's own default (src/settings.rs,
// AgentSlot). The analyzer is null when analyses go to the agent notes are sent to.

export const CAPABILITY_TIERS = [
  ["frontier", "Tier 3 · Frontier", "Hard Lane planning, plan judging, deep implementation & final judge"],
  ["standard", "Tier 2 · Standard", "Day-to-day implementation & Middle Lane peer reviews"],
  ["fast", "Tier 1 · Fast", "Queue triage, quick checks & Easy Lane trivial tasks"],
];

function tiersOf(settings) {
  return settings.modelTiers ?? { fast: [], standard: [], frontier: [] };
}

function lanesOf(settings) {
  return settings.lanes ?? { hardHumanGate: true, autoTriage: true };
}

// What an agent is used for, in words, or null when it is free to disable or remove.
export function usage(settings, name) {
  if (settings.agent?.name === name) return "notes are sent to it";
  if (settings.analyzer?.name === name) return "it analyzes the repository";
  if (settings.reviewers.some((slot) => slot.name === name)) return "it reviews the agent's work";
  const tiers = tiersOf(settings);
  for (const [tier, label] of [
    ["frontier", "Tier 3 (Frontier)"],
    ["standard", "Tier 2 (Standard)"],
    ["fast", "Tier 1 (Fast)"],
  ]) {
    if ((tiers[tier] ?? []).some((slot) => slot.name === name)) {
      return `it is in ${label}`;
    }
  }
  return null;
}

// A slot in words: "Claude · Opus 5.5". The model's name is the agent's own when `report`, the
// agent's reply of api/agents/models, offers it; else the id. The agent's default names none.
export function formatSlot(slot, report) {
  if (slot.model === null) return slot.name;
  const offered = report?.status === "ready" ? report.options : [];
  const option = offered.find((each) => each.value === slot.model);
  return `${slot.name} · ${option?.name ?? slot.model}`;
}

// The agents as the dialog lists them, in the scan's order: built-in ones, then custom ones.
export function agentRows(agents, settings) {
  return agents.map((agent) => ({
    ...agent,
    enabled: settings.enabledAgents.includes(agent.name),
    usage: usage(settings, agent.name),
  }));
}

// Where a row's agent stands, as the mark beside its name shows it (`kind`, a class in
// app.css) and says it (`label`). An agent that is on but missing is a problem; one that is
// off is only not installed.
export function agentState(agent) {
  if (agent.found) return { kind: "installed", label: "Installed" };
  if (agent.pendingCheck) return { kind: "unchecked", label: "Checked on PATH when saved" };
  if (agent.enabled) return { kind: "missing", label: "Not found on PATH" };
  return { kind: "absent", label: "Not installed" };
}

// Ticks or unticks an agent. An agent in use cannot be unticked: the settings would name an
// agent that is not offered, which the Rust process refuses.
export function setEnabled(settings, name, enabled) {
  const listed = settings.enabledAgents.includes(name);
  if (enabled === listed) return { settings };
  if (!enabled) {
    const used = usage(settings, name);
    if (used) return { error: `${name} cannot be turned off while ${used}: choose another first.` };
    const enabledAgents = settings.enabledAgents.filter((each) => each !== name);
    return { settings: { ...settings, enabledAgents } };
  }
  return { settings: { ...settings, enabledAgents: [...settings.enabledAgents, name] } };
}

// Adds a custom agent, or replaces the one called `previous` (renaming it everywhere it is
// named). `builtInNames` are the catalog's names, which a custom agent cannot take. A new
// agent is enabled at once: it was added to be used.
export function saveCustomAgent(settings, agent, previous, builtInNames) {
  const name = agent.name.trim();
  const command = agent.command.trim();
  if (!name) return { error: "Give the agent a name." };
  if ([...name].length > MAX_NAME) {
    return { error: `A name can be at most ${MAX_NAME} characters.` };
  }
  const others = [
    ...builtInNames,
    ...settings.customAgents.map((each) => each.name).filter((each) => each !== previous),
  ];
  if (others.some((each) => each.toLowerCase() === name.toLowerCase())) {
    return { error: `There is already an agent called ${name}.` };
  }
  if (!command) return { error: "Give the command that starts the agent." };
  const program = command.split(/\s+/)[0];
  if (program.includes("/") && !program.startsWith("/")) {
    return { error: `The program must be on PATH or an absolute path, not ${program}.` };
  }
  if (/["'\\]/.test(command)) {
    return { error: "The command is split on spaces as it is: it cannot hold quotes or \\." };
  }

  const saved = { name, command };
  if (previous === null) {
    return {
      settings: {
        ...settings,
        customAgents: [...settings.customAgents, saved],
        enabledAgents: [...settings.enabledAgents, name],
      },
    };
  }
  const rename = (each) => (each === previous ? name : each);
  // The model stays: the agent is the same one under another name.
  const renameSlot = (slot) => ({ ...slot, name: rename(slot.name) });
  const prevTiers = settings.modelTiers;
  const modelTiers = prevTiers
    ? {
        fast: (prevTiers.fast ?? []).map(renameSlot),
        standard: (prevTiers.standard ?? []).map(renameSlot),
        frontier: (prevTiers.frontier ?? []).map(renameSlot),
      }
    : undefined;
  return {
    settings: {
      ...settings,
      customAgents: settings.customAgents.map((each) => (each.name === previous ? saved : each)),
      enabledAgents: settings.enabledAgents.map(rename),
      agent: settings.agent === null ? null : renameSlot(settings.agent),
      analyzer: settings.analyzer ? renameSlot(settings.analyzer) : null,
      reviewers: settings.reviewers.map(renameSlot),
      ...(modelTiers ? { modelTiers } : {}),
    },
  };
}

// Removes a custom agent, unless it is in use.
export function removeCustomAgent(settings, name) {
  const used = usage(settings, name);
  if (used) return { error: `${name} cannot be removed while ${used}: choose another first.` };
  return {
    settings: {
      ...settings,
      customAgents: settings.customAgents.filter((each) => each.name !== name),
      enabledAgents: settings.enabledAgents.filter((each) => each !== name),
    },
  };
}

// The agent notes are sent to; null for none. Only an enabled agent can be chosen. Another
// agent starts on its default model, since models are the agent's own; the same agent keeps
// the model it has.
export function setAgent(settings, name) {
  if (name === null) return { settings: { ...settings, agent: null } };
  if (!settings.enabledAgents.includes(name)) return { error: `Turn ${name} on first.` };
  const model = settings.agent?.name === name ? settings.agent.model : null;
  return { settings: { ...settings, agent: { name, model } } };
}

// The model of the agent notes are sent to; null for the agent's default.
export function setAgentModel(settings, model) {
  if (settings.agent === null) return { error: "Choose the agent notes are sent to first." };
  return { settings: { ...settings, agent: { ...settings.agent, model } } };
}

// The agent that analyzes the repository for the map; null for the agent notes are sent to. As
// for that agent, only an enabled one can be chosen, and another agent starts on its default
// model.
export function setAnalyzer(settings, name) {
  if (name === null) return { settings: { ...settings, analyzer: null } };
  if (!settings.enabledAgents.includes(name)) return { error: `Turn ${name} on first.` };
  const model = settings.analyzer?.name === name ? settings.analyzer.model : null;
  return { settings: { ...settings, analyzer: { name, model } } };
}

// The analyzer's model; null for the agent's default.
export function setAnalyzerModel(settings, model) {
  if (!settings.analyzer) return { error: "Choose the analyzer first." };
  return { settings: { ...settings, analyzer: { ...settings.analyzer, model } } };
}

// Adds a reviewer at the end, on its default model. The same agent may review more than once:
// each entry is one independent review, with a model of its own.
export function addReviewer(settings, name) {
  if (!settings.enabledAgents.includes(name)) return { error: `Turn ${name} on first.` };
  const reviewers = [...settings.reviewers, { name, model: null }];
  return { settings: { ...settings, reviewers } };
}

// The model of the reviewer at `index`; null for the agent's default.
export function setReviewerModel(settings, index, model) {
  const reviewers = settings.reviewers.map((slot, at) =>
    at === index ? { ...slot, model } : slot,
  );
  return { settings: { ...settings, reviewers } };
}

export function removeReviewer(settings, index) {
  return { settings: { ...settings, reviewers: settings.reviewers.toSpliced(index, 1) } };
}

// Moves the reviewer at `index` by `step` places (-1 up, 1 down); past either end it stays.
export function moveReviewer(settings, index, step) {
  const target = index + step;
  if (target < 0 || target >= settings.reviewers.length) return { settings };
  const reviewers = [...settings.reviewers];
  [reviewers[index], reviewers[target]] = [reviewers[target], reviewers[index]];
  return { settings: { ...settings, reviewers } };
}

// Team memory (hippius-mem) as the mark beside it shows it (`kind`, an `.agent-mark` class in
// app.css) and says it (`label`), from `memory`, the `memory` of api/settings. Missing while the
// switch is on is a problem; while it is off, only not installed.
export function memoryState(memory, agentMemory) {
  const state = memory.state;
  if (state === null) return { kind: "unchecked", label: "Checking hippius-mem…" };
  if (state.kind === "ready") return { kind: "installed", label: "Ready" };
  if (state.kind === "unconfigured") {
    if (canSetUp(state)) return { kind: "unchecked", label: "Installed, not set up" };
    return { kind: "missing", label: "Installed, doctor did not pass" };
  }
  if (agentMemory) return { kind: "missing", label: "Not found on PATH" };
  return { kind: "absent", label: "Not installed" };
}

// Whether Set up a trial vault is offered for `state`: only when doctor found no
// configuration. hippius-mem exits 1 for every error, so its message tells this apart from a
// configured vault whose check failed, where `quickstart` would only refuse ("a config already
// exists"). The words are doctor's with no configuration (hippius-mem 0.7.1, spike S3), pinned
// by the test of the real program in src/site/tests/memory.rs. Doctor printing nothing says
// nothing against it.
export function canSetUp(state) {
  if (state?.kind !== "unconfigured") return false;
  return state.problem === null || state.problem.includes("failed to load configuration");
}

// Turns team memory on or off. It turns on only once hippius-mem is ready (`state`, the state in
// api/settings' `memory`); it turns off whatever the state, so a removed hippius-mem can be let
// go. The Rust process checks the same rule against its own latest check (src/site.rs).
export function setAgentMemory(settings, enabled, state) {
  if (enabled && state?.kind !== "ready") {
    return { error: "Set up hippius-mem before giving agents team memory." };
  }
  return { settings: { ...settings, agentMemory: enabled } };
}

// Adds an enabled agent to a capability tier ("fast" | "standard" | "frontier").
export function addTierSlot(settings, tier, name, model = null) {
  if (!settings.enabledAgents.includes(name)) return { error: `Turn ${name} on first.` };
  const tiers = tiersOf(settings);
  const next = [...(tiers[tier] ?? []), { name, model }];
  return { settings: { ...settings, modelTiers: { ...tiers, [tier]: next } } };
}

// Updates the model of a slot inside a capability tier.
export function setTierSlotModel(settings, tier, index, model) {
  const tiers = tiersOf(settings);
  const next = (tiers[tier] ?? []).map((slot, at) => (at === index ? { ...slot, model } : slot));
  return { settings: { ...settings, modelTiers: { ...tiers, [tier]: next } } };
}

// Removes a slot from a capability tier.
export function removeTierSlot(settings, tier, index) {
  const tiers = tiersOf(settings);
  const next = (tiers[tier] ?? []).toSpliced(index, 1);
  return { settings: { ...settings, modelTiers: { ...tiers, [tier]: next } } };
}

// Toggles whether Hard Lane pauses after Stage 2 (Plan Judge) for human approval.
export function setHardHumanGate(settings, enabled) {
  return { settings: { ...settings, lanes: { ...lanesOf(settings), hardHumanGate: enabled } } };
}

// Toggles whether the Hub runs automatic cached triage on synced items.
export function setAutoTriage(settings, enabled) {
  return { settings: { ...settings, lanes: { ...lanesOf(settings), autoTriage: enabled } } };
}
