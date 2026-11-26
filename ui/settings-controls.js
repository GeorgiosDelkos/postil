// Shared controls for the Settings dialog and the Startup Wizard: the segmented control, font
// stepper, switch, agent rows, custom agent form, and reviewer list. Both views use the same
// markup and rules (settings.js) so settings look and behave identically in both places.

import { el } from "./dom.js";

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

// One row in the agent list. `idPrefix` keeps HTML ids unique when Settings and the Wizard
// both live in the document.
export function agentListRow(
  agent,
  { disabled = false, formOpen = false, onToggle, onEdit, onRemove, idPrefix = "" },
) {
  const node = el("div", "agent-row");
  // Keys of per-agent controls hold a `/`, which no fixed key does, so an agent named "add"
  // cannot take the key of the Add agent button.
  const key = `enable/${agent.name}`;
  const id = `${idPrefix}${key}`;
  const box = el("input");
  box.type = "checkbox";
  box.id = id;
  box.dataset.key = key;
  box.checked = agent.enabled;
  // An agent that is not installed cannot be turned on; one already on stays shown so it
  // can be turned off.
  box.disabled = disabled || (!agent.found && !agent.enabled && !agent.pendingCheck);
  box.addEventListener("change", () => onToggle(box.checked));

  const text = el("label", "agent-text");
  text.htmlFor = id;
  const name = el("span", "agent-name", agent.name);
  if (!agent.builtIn) name.append(el("span", "agent-badge", "custom"));
  // An agent that is on but missing is a problem; one that is off is only not installed.
  const status = agent.found
    ? el("span", "agent-found", agent.found)
    : agent.pendingCheck
      ? el("span", "agent-missing", "Checked on PATH when saved")
      : agent.enabled
        ? el("span", "agent-missing warning", "Not found on PATH")
        : el("span", "agent-missing", "Not installed");
  text.append(name, status, el("span", "agent-command", agent.command));
  if (agent.usage) {
    const used = agent.usage[0].toUpperCase() + agent.usage.slice(1);
    text.append(el("span", "agent-usage", used));
  }
  node.append(box, text);

  if (!agent.builtIn) {
    const actions = el("div", "agent-actions");
    const edit = controlButton("Edit", `edit/${agent.name}`, onEdit);
    const remove = controlButton("Remove", `remove/${agent.name}`, onRemove);
    edit.disabled = remove.disabled = disabled || formOpen;
    actions.append(edit, remove);
    node.append(actions);
  }
  return node;
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

export function reviewerControls(settings, { disabled = false, onMove, onRemove, onAdd }) {
  const list = el("ol", "reviewer-list");
  settings.reviewers.forEach((name, index) => {
    const item = el("li", "reviewer");
    item.append(el("span", "reviewer-name", name));
    const up = controlButton("↑", `up-${index}`, () => onMove(index, -1), "icon-button");
    const down = controlButton("↓", `down-${index}`, () => onMove(index, 1), "icon-button");
    const remove = controlButton("×", `unreview-${index}`, () => onRemove(index), "icon-button");
    up.setAttribute("aria-label", `Move ${name} up`);
    down.setAttribute("aria-label", `Move ${name} down`);
    remove.setAttribute("aria-label", `Remove ${name} as a reviewer`);
    up.disabled = disabled || index === 0;
    down.disabled = disabled || index === settings.reviewers.length - 1;
    remove.disabled = disabled;
    item.append(up, down, remove);
    list.append(item);
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

  const control = el("div", "reviewers");
  if (!settings.reviewers.length) {
    const none = "No review: the agent's work comes straight back to you.";
    control.append(el("p", "settings-hint", none));
  }
  control.append(list, add);
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
