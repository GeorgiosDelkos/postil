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

// What an agent is used for, in words, or null when it is free to disable or remove.
export function usage(settings, name) {
  if (settings.agent === name) return "notes are sent to it";
  if (settings.reviewers.includes(name)) return "it reviews the agent's work";
  return null;
}

// The agents as the dialog lists them, in the scan's order: built-in ones, then custom ones.
export function agentRows(agents, settings) {
  return agents.map((agent) => ({
    ...agent,
    enabled: settings.enabledAgents.includes(agent.name),
    usage: usage(settings, agent.name),
  }));
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
  return {
    settings: {
      ...settings,
      customAgents: settings.customAgents.map((each) => (each.name === previous ? saved : each)),
      enabledAgents: settings.enabledAgents.map(rename),
      agent: settings.agent === null ? null : rename(settings.agent),
      reviewers: settings.reviewers.map(rename),
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

// The agent notes are sent to; null for none. Only an enabled agent can be chosen.
export function setAgent(settings, name) {
  if (name !== null && !settings.enabledAgents.includes(name)) {
    return { error: `Turn ${name} on first.` };
  }
  return { settings: { ...settings, agent: name } };
}

// Adds a reviewer at the end. The same agent may review more than once: each entry is one
// independent review.
export function addReviewer(settings, name) {
  if (!settings.enabledAgents.includes(name)) return { error: `Turn ${name} on first.` };
  return { settings: { ...settings, reviewers: [...settings.reviewers, name] } };
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
