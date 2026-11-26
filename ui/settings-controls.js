// Shared controls for the Settings dialog and the Startup Wizard: the segmented control, font
// stepper, switch, agent rows, custom agent form, and reviewer list. Both views use the same
// markup and rules (settings.js) so settings look and behave identically in both places.

import { agentLogoIcon, el } from "./dom.js";
import { agentState } from "./settings.js";

// How long a dialog waits before asking again while the agent scan runs. Arbitrary: short
// enough that the list appears as soon as the scan ends, long enough not to busy the process.
export const SCAN_POLL_MS = 250;

export const THEMES = [
  ["system", "System"],
  ["light", "Light"],
  ["dark", "Dark"],
];

export async function fetchSettingsJson(route = "api/settings", init = undefined) {
  const response = await fetch(route, init);
  const text = await response.text();
  if (!response.ok) throw new Error(text);
  return JSON.parse(text);
}

export function controlButton(text, key, onClick, className = "button") {
  const node = el("button", className, text);
  node.type = "button";
  node.dataset.key = key;
  node.addEventListener("click", onClick);
  return node;
}

export function settingSection(title, ...children) {
  const node = el("section", "settings-section");
  node.append(el("h3", "settings-heading", title), ...children);
  return node;
}

export function settingRow(label, hint, control) {
  const node = el("div", "settings-row");
  const text = el("div", "settings-label");
  text.append(el("div", "", label));
  if (hint) text.append(el("div", "settings-hint", hint));
  node.append(text, control);
  return node;
}

// A row of mutually exclusive choices, as macOS draws a segmented control.
export function segmented(key, options, current, choose, { disabled = false } = {}) {
  const node = el("div", "segmented");
  node.setAttribute("role", "radiogroup");
  for (const [value, label] of options) {
    const option = controlButton(label, `${key}-${value}`, () => choose(value), "segment");
    option.setAttribute("role", "radio");
    option.setAttribute("aria-checked", String(value === current));
    option.disabled = disabled;
    node.append(option);
  }
  return node;
}

export function fontStepper(size, [smallest, largest], onStep, { disabled = false } = {}) {
  const stepper = el("div", "stepper");
  const smaller = controlButton("−", "font-smaller", () => onStep(-1), "segment");
  const larger = controlButton("+", "font-larger", () => onStep(1), "segment");
  smaller.setAttribute("aria-label", "Smaller");
  larger.setAttribute("aria-label", "Larger");
  smaller.disabled = disabled || size <= smallest;
  larger.disabled = disabled || size >= largest;
  stepper.append(smaller, el("span", "stepper-value", `${size} px`), larger);
  return stepper;
}

export function toggleSwitch(key, on, flip, { label, disabled = false } = {}) {
  const node = controlButton("", key, flip, "switch");
  node.setAttribute("role", "switch");
  node.setAttribute("aria-checked", String(on));
  if (label) node.setAttribute("aria-label", label);
  node.disabled = disabled;
  return node;
}

export function selectBox(key, label, options, value, choose, { disabled = false } = {}) {
  const node = el("select", "select");
  node.dataset.key = key;
  node.setAttribute("aria-label", label);
  for (const [optionValue, text] of options) {
    const option = el("option", "", text);
    option.value = optionValue;
    option.selected = optionValue === value;
    node.append(option);
  }
  node.disabled = disabled;
  node.addEventListener("change", () => choose(node.value));
  return node;
}

// The mark before an agent's name in the list, one glyph per `agentState` kind; its shape and
// colour are CSS (`.agent-mark.<kind>`). Not installed is a hollow circle, with no glyph.
const AGENT_MARKS = { installed: "✓", missing: "!", unchecked: "–", absent: "" };

// One row in the agent list: a mark for where the agent stands, its logo and name, what it is
// used for, the command it starts with, and a switch. The state in words is the mark's label
// and, with the full path of the program, the row's tooltip. A row whose agent is not
// installed has its logo, name, use and command dimmed (`.agent-text`).
export function agentListRow(
  agent,
  { disabled = false, formOpen = false, onToggle, onEdit, onRemove },
) {
  const state = agentState(agent);
  const node = el("div", `agent-row ${state.kind}`);
  node.title = agent.found ? `${state.label}: ${agent.found}` : state.label;
  const mark = el("span", `agent-mark ${state.kind}`, AGENT_MARKS[state.kind]);
  mark.setAttribute("role", "img");
  mark.setAttribute("aria-label", state.label);

  const text = el("div", "agent-text");
  const name = el("span", "agent-name");
  const logo = agentLogoIcon(agent.name);
  if (logo) name.append(logo);
  name.append(agent.name);
  if (!agent.builtIn) name.append(el("span", "agent-badge", "custom"));
  text.append(name);
  if (agent.usage) {
    const used = agent.usage[0].toUpperCase() + agent.usage.slice(1);
    text.append(el("span", "agent-usage", used));
  }
  text.append(el("span", "agent-command", agent.command));
  node.append(mark, text);

  if (!agent.builtIn) {
    const actions = el("div", "agent-actions");
    const edit = controlButton("Edit", `edit/${agent.name}`, onEdit);
    const remove = controlButton("Remove", `remove/${agent.name}`, onRemove);
    edit.disabled = remove.disabled = disabled || formOpen;
    actions.append(edit, remove);
    node.append(actions);
  }

  // Keys of per-agent controls hold a `/`, which no fixed key does, so an agent named "add"
  // cannot take the key of the Add agent button.
  const toggle = toggleSwitch(
    `enable/${agent.name}`,
    agent.enabled,
    () => onToggle(!agent.enabled),
    {
      label: agent.name,
      // An agent that is not installed cannot be turned on; one already on stays shown so it
      // can be turned off.
      disabled: disabled || (!agent.found && !agent.enabled && !agent.pendingCheck),
    },
  );
  node.append(toggle);
  return node;
}

// The choices of a model select, as `[value, label]` pairs: "Agent default" (value "") first,
// then the models `report` (the agent's reply of api/agents/models) offers. The `saved` model
// is always among them, so a redraw while the agent is asked, or after it failed to answer,
// never drops it; one the agent answered without is marked as no longer offered.
export function modelSelectOptions(report, saved) {
  const options = [["", "Agent default"]];
  if (report?.status !== "ready") {
    if (saved !== null) options.push([saved, saved]);
    return options;
  }
  options.push(...report.options.map((option) => [option.value, option.name]));
  if (saved !== null && !report.options.some((option) => option.value === saved)) {
    options.push([saved, `${saved} (not offered)`]);
  }
  return options;
}

// A select of `slot`'s model, among those `report` says its agent offers, with a dim line
// under it while the agent is asked or when it could not say. `choose` gets the model id, or
// null for the agent's default.
export function modelSelect(key, slot, report, choose, { disabled = false } = {}) {
  const node = el("div", "model-select");
  const options = modelSelectOptions(report, slot.model);
  const select = selectBox(
    key,
    `Model for ${slot.name}`,
    options,
    slot.model ?? "",
    (value) => choose(value || null),
    { disabled },
  );
  node.append(select);
  if (report?.status === "loading") {
    node.append(el("span", "settings-hint model-hint", `Asking ${slot.name} for its models…`));
  } else if (report?.status === "failed") {
    const hint = el("span", "settings-hint model-hint", report.error);
    hint.title = report.error;
    node.append(hint);
  }
  return node;
}

// One row of a slot card: the reviewer's number, the agent, its model, and the row's buttons,
// each in its column of the grid every slot row shares (`.slot-row`), so the model selects of
// all rows line up whatever the agents' names. A number, model or actions given as null leave
// their cell empty, to keep the columns.
function slotRow(tag, number, agent, model, actions) {
  const node = el(tag, "slot-row");
  const numberCell = el("span", "slot-number", number ?? "");
  const actionCell = el("div", "slot-actions");
  if (actions) actionCell.append(...actions);
  node.append(numberCell, agent, model ?? el("div"), actionCell);
  return node;
}

// The agent notes are sent to and its model, as one row of a slot card. `key` names the agent
// select; the model select is `${key}-model`. `onAgent` gets a name, or null for none.
export function agentSlotControl(key, settings, models, { disabled = false, onAgent, onModel }) {
  const enabled = settings.enabledAgents;
  const none = enabled.length ? "None" : "Turn on an agent first";
  const options = [["", none], ...enabled.map((name) => [name, name])];
  const chosen = settings.agent;
  const agent = selectBox(
    key,
    "Send notes to",
    options,
    chosen?.name ?? "",
    (name) => onAgent(name || null),
    { disabled: disabled || enabled.length === 0 },
  );
  agent.classList.add("slot-agent");
  const model = chosen
    ? modelSelect(`${key}-model`, chosen, models[chosen.name], onModel, { disabled })
    : null;
  const node = el("div", "slot-card");
  node.append(slotRow("div", null, agent, model, null));
  return node;
}

// How long a dialog waits before asking again for an agent whose probe failed (one not signed
// in yet, say): the process probes a failed agent again only after 5 s (src/models.rs,
// FAILED_TTL), so asking sooner would only get the same failure.
const FAILED_POLL_MS = 5000;

// The models of a dialog's enabled agents, from api/agents/models, for its model selects. It
// asks again every SCAN_POLL_MS while an agent is being asked, after FAILED_POLL_MS while one
// failed, and asks anew when the agents asked for change (`request`). `onChange(models)` gets
// the reports by agent name whenever a reply changes them; `onError(error)` a request that
// failed.
export function modelReports({ onChange, onError }) {
  let asked = null;
  let timer = null;
  let models = {};
  // Counts requests, so the reply of one that a newer request replaced is dropped.
  let generation = 0;

  async function poll(body, id) {
    try {
      const reply = await fetchSettingsJson("api/agents/models", { method: "POST", body });
      if (id !== generation) return;
      const changed = JSON.stringify(reply.models) !== JSON.stringify(models);
      models = reply.models;
      const failed = Object.values(models).some((report) => report.status === "failed");
      if (reply.loading) timer = setTimeout(() => poll(body, id), SCAN_POLL_MS);
      else if (failed) timer = setTimeout(() => poll(body, id), FAILED_POLL_MS);
      if (changed) onChange(models);
    } catch (error) {
      if (id !== generation) return;
      asked = null;
      onError(error);
    }
  }

  return {
    get models() {
      return models;
    },
    // Asks for the models of `settings`' enabled agents, its custom agents as they are (saved
    // or not), unless the last request asked for the same.
    request(settings) {
      const agents = settings.enabledAgents;
      const body = JSON.stringify({ agents, customAgents: settings.customAgents });
      if (body === asked) return;
      asked = body;
      generation += 1;
      clearTimeout(timer);
      if (agents.length > 0) poll(body, generation);
    },
    // Forgets what was asked and stops asking, so the next request asks again: after a
    // rescan, or when the dialog closes.
    reset() {
      asked = null;
      generation += 1;
      clearTimeout(timer);
    },
  };
}

export function customAgentForm(form, { onSubmit, onCancel }) {
  // A submit button, so Enter in either field submits: a form with two fields submits on
  // Enter only when it has one.
  const submit = el("button", "button primary", form.previous === null ? "Add" : "Save");
  submit.type = "submit";
  submit.dataset.key = "form-save";
  const node = el("form", "agent-form");
  const field = (label, key, value, placeholder) => {
    const wrap = el("label", "agent-field");
    const input = el("input", "field");
    input.type = "text";
    input.value = value;
    input.placeholder = placeholder;
    input.dataset.key = key;
    input.spellcheck = false;
    input.autocomplete = "off";
    input.addEventListener("input", () => {
      form[key === "form-name" ? "name" : "command"] = input.value;
    });
    wrap.append(el("span", "settings-hint", label), input);
    return wrap;
  };
  node.append(
    field("Name", "form-name", form.name, "My agent"),
    field(
      "Command that starts it as an ACP server",
      "form-command",
      form.command,
      "my-agent --acp",
    ),
  );
  if (form.error) node.append(el("p", "form-error", form.error));
  const actions = el("div", "settings-actions");
  actions.append(controlButton("Cancel", "form-cancel", onCancel), submit);
  node.append(actions);
  node.addEventListener("submit", (event) => {
    event.preventDefault();
    onSubmit();
  });
  // Escape in the form closes the form, not the dialog.
  node.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    event.preventDefault();
    event.stopPropagation();
    onCancel();
  });
  return node;
}

// The reviewers in order as rows of a slot card, each with its model select (`models` are the
// agents' reports, by name), and a last row with a select that adds one.
export function reviewerControls(
  settings,
  models,
  { disabled = false, onMove, onRemove, onAdd, onModel },
) {
  const list = el("ol", "reviewer-list");
  settings.reviewers.forEach((slot, index) => {
    const { name } = slot;
    // The name is cut to its column; the tooltip shows it whole.
    const agent = el("span", "slot-agent");
    agent.title = name;
    const logo = agentLogoIcon(name);
    if (logo) agent.append(logo);
    agent.append(el("span", "slot-agent-name", name));
    const model = modelSelect(
      `reviewer-model-${index}`,
      slot,
      models[name],
      (chosen) => onModel(index, chosen),
      { disabled },
    );
    const up = controlButton("↑", `up-${index}`, () => onMove(index, -1), "icon-button");
    const down = controlButton("↓", `down-${index}`, () => onMove(index, 1), "icon-button");
    const remove = controlButton("×", `unreview-${index}`, () => onRemove(index), "icon-button");
    up.setAttribute("aria-label", `Move ${name} up`);
    down.setAttribute("aria-label", `Move ${name} down`);
    remove.setAttribute("aria-label", `Remove ${name} as a reviewer`);
    up.disabled = disabled || index === 0;
    down.disabled = disabled || index === settings.reviewers.length - 1;
    remove.disabled = disabled;
    list.append(slotRow("li", `${index + 1}.`, agent, model, [up, down, remove]));
  });

  const enabled = settings.enabledAgents.map((name) => [name, name]);
  const options = [["", "Add a reviewer…"], ...enabled];
  const add = selectBox(
    "reviewer-add",
    "Add a reviewer",
    options,
    "",
    (name) => {
      if (name) onAdd(name);
    },
    { disabled: disabled || settings.enabledAgents.length === 0 },
  );
  add.classList.add("quiet");
  const addRow = el("div", "slot-row slot-add");
  addRow.append(el("span", "slot-number"), add);

  const card = el("div", "slot-card");
  card.append(list, addRow);
  const control = el("div", "reviewers");
  if (!settings.reviewers.length) {
    const none = "No review: the agent's work comes straight back to you.";
    control.append(el("p", "settings-hint", none));
  }
  control.append(card);
  return control;
}

// Restores focus inside `dialogEl` after a full redraw, using each control's `data-key`.
export function restoreDialogFocus(dialogEl, focusedKey) {
  if (!focusedKey) return;
  const target = dialogEl.querySelector(`[data-key="${CSS.escape(focusedKey)}"]`);
  const enabled = "button:not(:disabled), select:not(:disabled), input:not(:disabled)";
  if (target && !target.disabled) target.focus();
  else (target?.parentElement?.querySelector(enabled) ?? dialogEl).focus();
}
