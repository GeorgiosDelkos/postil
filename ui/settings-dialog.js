// The settings dialog: draws `api/settings` and saves every change as it is made, so there is
// no Save button and nothing to lose by closing. The rules for each change are in
// settings.js; the Rust process checks them again and owns the file (src/settings.rs).
//
// The dialog is redrawn whole after every change: it has a few dozen controls, so a redraw
// costs nothing noticeable, and one drawing function is easier to keep right than updates by
// hand. The control that had the focus gets it back by its `data-key`.

import {
  addReviewer,
  agentRows,
  moveReviewer,
  removeCustomAgent,
  removeReviewer,
  saveCustomAgent,
  setAgent,
  setEnabled,
} from "./settings.js";

// How long the dialog waits before asking again while the agent scan runs. Arbitrary: short
// enough that the list appears as soon as the scan ends, long enough not to busy the process.
const SCAN_POLL_MS = 250;

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function button(text, key, onClick, className = "button") {
  const node = el("button", className, text);
  node.type = "button";
  node.dataset.key = key;
  node.addEventListener("click", onClick);
  return node;
}

// `apply` is called with the settings in effect whenever they change (and once on opening),
// so the page can follow them; `closed` when the dialog closes.
export function settingsDialog(dialogEl, { apply, closed }) {
  const ui = {
    // The reply of api/settings, or null before the first one.
    view: null,
    // The custom agent form: { previous, name, command, error }, `previous` null for a new one.
    form: null,
    // A refusal or failure to show in the footer, or null.
    error: null,
    // Shown in the footer after a save, until the next change.
    saved: false,
    confirmReset: false,
    // Saves run one after another, in the order the changes were made.
    queue: Promise.resolve(),
    pollTimer: null,
  };

  async function load(route = "api/settings", init = undefined) {
    const response = await fetch(route, init);
    const text = await response.text();
    if (!response.ok) throw new Error(text);
    return JSON.parse(text);
  }

  function show(view) {
    ui.view = view;
    apply(view.settings);
    clearTimeout(ui.pollTimer);
    if (view.agents === null && dialogEl.open) {
      ui.pollTimer = setTimeout(() => refresh().catch(fail), SCAN_POLL_MS);
    }
    render();
  }

  function fail(error) {
    ui.error = String(error.message ?? error);
    ui.saved = false;
    render();
  }

  async function refresh() {
    show(await load());
  }

  // Shows `next` at once and saves it. A refusal puts the last saved settings back.
  function save(next) {
    if (next.error) {
      fail(new Error(next.error));
      return;
    }
    ui.error = null;
    ui.saved = false;
    ui.view = { ...ui.view, settings: next.settings };
    render();
    ui.queue = ui.queue.then(async () => {
      try {
        const body = JSON.stringify(next.settings);
        show(await load("api/settings", { method: "PUT", body }));
        ui.saved = true;
        render();
      } catch (error) {
        fail(error);
        await refresh().catch(() => {});
      }
    });
  }

  const change = (update) => () => save(update(ui.view.settings));

  // ---- Drawing --------------------------------------------------------------------------

  // Redraws the dialog where it was: same scroll position, same focused control.
  function render() {
    const focused = document.activeElement?.dataset?.key;
    const scrolled = dialogEl.querySelector(".settings-body")?.scrollTop ?? 0;
    dialogEl.replaceChildren(panel());
    dialogEl.querySelector(".settings-body").scrollTop = scrolled;
    if (focused) dialogEl.querySelector(`[data-key="${CSS.escape(focused)}"]`)?.focus();
  }

  function panel() {
    const node = el("div", "settings-panel");
    const header = el("header", "settings-header");
    const title = el("h2", "settings-title", "Settings");
    title.id = "settings-title";
    header.append(title, button("×", "close", () => dialogEl.close(), "icon-button"));
    header.lastChild.setAttribute("aria-label", "Close");

    const body = el("div", "settings-body");
    if (!ui.view) body.append(el("p", "settings-hint", "Loading…"));
    else {
      if (ui.view.problem) body.append(problem());
      body.append(appearance(), startup(), agents());
    }
    node.append(header, body, footer());
    return node;
  }

  function problem() {
    const node = el("div", "settings-problem");
    node.setAttribute("role", "alert");
    node.append(el("p", "", ui.view.problem));
    if (!ui.view.file) return node;
    const actions = el("div", "settings-actions");
    if (ui.confirmReset) {
      actions.append(
        el("span", "settings-hint", "Replace the file with the defaults?"),
        button("Cancel", "reset-cancel", () => {
          ui.confirmReset = false;
          render();
        }),
        button("Reset", "reset-confirm", resetSettings, "button danger"),
      );
    } else {
      actions.append(
        button("Reset to defaults…", "reset", () => {
          ui.confirmReset = true;
          render();
        }),
      );
    }
    node.append(actions);
    return node;
  }

  async function resetSettings() {
    ui.confirmReset = false;
    try {
      show(await load("api/settings/reset", { method: "POST" }));
    } catch (error) {
      fail(error);
    }
  }

  // Controls are off while the file cannot be saved over: a change would only be refused.
  const locked = () => Boolean(ui.view.problem);

  function section(title, ...children) {
    const node = el("section", "settings-section");
    node.append(el("h3", "settings-heading", title), ...children);
    return node;
  }

  function row(label, hint, control) {
    const node = el("div", "settings-row");
    const text = el("div", "settings-label");
    text.append(el("div", "", label));
    if (hint) text.append(el("div", "settings-hint", hint));
    node.append(text, control);
    return node;
  }

  // A row of mutually exclusive choices, as macOS draws a segmented control.
  function segmented(key, options, current, choose) {
    const node = el("div", "segmented");
    node.setAttribute("role", "radiogroup");
    for (const [value, label] of options) {
      const option = button(label, `${key}-${value}`, () => choose(value), "segment");
      option.setAttribute("role", "radio");
      option.setAttribute("aria-checked", String(value === current));
      option.disabled = locked();
      node.append(option);
    }
    return node;
  }

  function appearance() {
    const settings = ui.view.settings;
    const [smallest, largest] = ui.view.fontSizes;
    const size = settings.codeFontSize;
    const stepper = el("div", "stepper");
    const resize = (step) => change((s) => ({ settings: { ...s, codeFontSize: size + step } }));
    const smaller = button("−", "font-smaller", resize(-1), "segment");
    const larger = button("+", "font-larger", resize(1), "segment");
    smaller.setAttribute("aria-label", "Smaller");
    larger.setAttribute("aria-label", "Larger");
    smaller.disabled = locked() || size <= smallest;
    larger.disabled = locked() || size >= largest;
    stepper.append(smaller, el("span", "stepper-value", `${size} px`), larger);

    const themes = [["system", "System"], ["light", "Light"], ["dark", "Dark"]];
    const tabs = ui.view.tabWidths.map((width) => [width, String(width)]);
    return section(
      "Appearance",
      row(
        "Theme",
        null,
        segmented("theme", themes, settings.theme, (theme) => {
          change((s) => ({ settings: { ...s, theme } }))();
        }),
      ),
      row("Code font size", null, stepper),
      row(
        "Tab width",
        "Columns per tab character",
        segmented("tab", tabs, settings.tabWidth, (tabWidth) => {
          change((s) => ({ settings: { ...s, tabWidth } }))();
        }),
      ),
    );
  }

  function toggle(key, on, flip) {
    const node = button("", key, flip, "switch");
    node.setAttribute("role", "switch");
    node.setAttribute("aria-checked", String(on));
    node.disabled = locked();
    return node;
  }

  function startup() {
    const on = ui.view.settings.reopenLastProject;
    const flip = change((s) => ({ settings: { ...s, reopenLastProject: !on } }));
    const control = toggle("reopen", on, flip);
    control.setAttribute("aria-label", "Reopen the last project");
    return section(
      "Startup",
      row(
        "Reopen the last project",
        "Instead of the welcome screen, when Postil starts without a folder",
        control,
      ),
    );
  }

  function agents() {
    const node = section(
      "Agents",
      el(
        "p",
        "settings-hint",
        "Turn on the agents Postil may use. Notes go to one agent; reviewers then check its " +
          "work, one after another.",
      ),
    );
    if (ui.view.agents === null) {
      node.append(el("p", "settings-hint scanning", "Looking for installed agents…"));
      return node;
    }
    const list = el("div", "agent-list");
    for (const agent of agentRows(ui.view.agents, ui.view.settings)) list.append(agentRow(agent));
    const actions = el("div", "settings-actions");
    const add = button("Add agent…", "agent-add", () => {
      openForm({ previous: null, name: "", command: "" });
    });
    add.disabled = locked() || ui.form !== null;
    const rescan = button("Rescan", "rescan", async () => {
      try {
        show(await load("api/agents/rescan", { method: "POST" }));
      } catch (error) {
        fail(error);
      }
    });
    actions.append(add, rescan);
    node.append(list);
    if (ui.form) node.append(agentForm());
    node.append(actions, chosenAgent(), reviewers());
    return node;
  }

  function agentRow(agent) {
    const node = el("div", "agent-row");
    const id = `agent-${agent.name}`;
    const box = el("input");
    box.type = "checkbox";
    box.id = id;
    box.dataset.key = id;
    box.checked = agent.enabled;
    // An agent that is not installed cannot be turned on; one already on stays shown so it
    // can be turned off.
    box.disabled = locked() || (!agent.found && !agent.enabled);
    box.addEventListener("change", () => {
      save(setEnabled(ui.view.settings, agent.name, box.checked));
    });

    const text = el("label", "agent-text");
    text.htmlFor = id;
    const name = el("span", "agent-name", agent.name);
    if (!agent.builtIn) name.append(el("span", "agent-badge", "custom"));
    // An agent that is on but missing is a problem; one that is off is only not installed.
    const status = agent.found
      ? el("span", "agent-found", agent.found)
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
      const edit = button("Edit", `edit-${agent.name}`, () => {
        openForm({ previous: agent.name, name: agent.name, command: agent.command });
      });
      const remove = button("Remove", `remove-${agent.name}`, () => {
        save(removeCustomAgent(ui.view.settings, agent.name));
      });
      edit.disabled = remove.disabled = locked() || ui.form !== null;
      actions.append(edit, remove);
      node.append(actions);
    }
    return node;
  }

  // Opens the custom agent form below the list, scrolled into view, on its name field.
  function openForm(form) {
    ui.form = { ...form, error: null };
    ui.error = null;
    render();
    const name = dialogEl.querySelector('[data-key="form-name"]');
    name.closest("form").scrollIntoView({ block: "nearest" });
    name.focus();
  }

  function agentForm() {
    const form = ui.form;
    // A submit button, so Enter in either field submits: a form with two fields submits on
    // Enter only when it has one.
    const submit = el("button", "button primary", form.previous === null ? "Add" : "Save");
    submit.type = "submit";
    submit.dataset.key = "form-save";
    const node = el("form", "agent-form");
    const field = (label, key, value, placeholder) => {
      const wrap = el("label", "agent-field");
      const input = el("input");
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
    actions.append(
      button("Cancel", "form-cancel", () => {
        ui.form = null;
        render();
      }),
      submit,
    );
    node.append(actions);
    node.addEventListener("submit", (event) => {
      event.preventDefault();
      const builtIn = ui.view.agents.filter((agent) => agent.builtIn).map((agent) => agent.name);
      const next = saveCustomAgent(ui.view.settings, form, form.previous, builtIn);
      if (next.error) {
        form.error = next.error;
        render();
        return;
      }
      ui.form = null;
      save(next);
    });
    // Escape in the form closes the form, not the dialog.
    node.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      ui.form = null;
      render();
    });
    return node;
  }

  function select(key, label, options, value, choose) {
    const node = el("select", "select");
    node.dataset.key = key;
    node.setAttribute("aria-label", label);
    for (const [optionValue, text] of options) {
      const option = el("option", "", text);
      option.value = optionValue;
      option.selected = optionValue === value;
      node.append(option);
    }
    node.disabled = locked();
    node.addEventListener("change", () => choose(node.value));
    return node;
  }

  function chosenAgent() {
    const enabled = ui.view.settings.enabledAgents;
    const none = enabled.length ? "None" : "Turn on an agent first";
    const options = [["", none], ...enabled.map((name) => [name, name])];
    const chosen = ui.view.settings.agent ?? "";
    const control = select("agent", "Send notes to", options, chosen, (name) => {
      save(setAgent(ui.view.settings, name || null));
    });
    return row("Send notes to", "The agent that works on the notes", control);
  }

  function reviewers() {
    const settings = ui.view.settings;
    const list = el("ol", "reviewer-list");
    settings.reviewers.forEach((name, index) => {
      const item = el("li", "reviewer");
      item.append(el("span", "reviewer-name", name));
      const move = (step) => change((s) => moveReviewer(s, index, step));
      const up = button("↑", `up-${index}`, move(-1), "icon-button");
      const down = button("↓", `down-${index}`, move(1), "icon-button");
      const unreview = change((s) => removeReviewer(s, index));
      const remove = button("×", `unreview-${index}`, unreview, "icon-button");
      up.setAttribute("aria-label", `Move ${name} up`);
      down.setAttribute("aria-label", `Move ${name} down`);
      remove.setAttribute("aria-label", `Remove ${name} as a reviewer`);
      up.disabled = locked() || index === 0;
      down.disabled = locked() || index === settings.reviewers.length - 1;
      remove.disabled = locked();
      item.append(up, down, remove);
      list.append(item);
    });

    const enabled = settings.enabledAgents.map((name) => [name, name]);
    const options = [["", "Add a reviewer…"], ...enabled];
    const add = select("reviewer-add", "Add a reviewer", options, "", (name) => {
      if (name) save(addReviewer(ui.view.settings, name));
    });
    add.disabled = locked() || settings.enabledAgents.length === 0;

    const control = el("div", "reviewers");
    if (!settings.reviewers.length) {
      const none = "No review: the agent's work comes straight back to you.";
      control.append(el("p", "settings-hint", none));
    }
    control.append(list, add);
    const node = row("Reviewers", "Each one reviews the agent's work, in this order", control);
    node.classList.add("tall");
    return node;
  }

  function footer() {
    const node = el("footer", "settings-footer");
    // The path is laid out right to left so that a cut keeps its end, the file name; the
    // marks keep its own characters left to right, or the leading `/` would move to the end.
    const path = ui.view?.file ? `\u200E${ui.view.file}\u200E` : "";
    const file = el("span", "settings-file", path);
    file.title = ui.view?.file ?? "";
    const said = ui.error ?? (ui.saved ? "Saved" : "");
    const status = el("span", `settings-status${ui.error ? " failed" : ""}`, said);
    status.setAttribute("role", "status");
    node.append(file, status);
    return node;
  }

  dialogEl.addEventListener("close", () => {
    clearTimeout(ui.pollTimer);
    ui.form = null;
    ui.confirmReset = false;
    closed();
  });
  // A click on the backdrop lands on the dialog element itself, outside its panel.
  dialogEl.addEventListener("mousedown", (event) => {
    if (event.target === dialogEl) dialogEl.close();
  });

  return {
    // Reads the settings and applies them, without opening the dialog.
    async load() {
      show(await load());
    },
    async open() {
      if (dialogEl.open) return;
      ui.error = null;
      ui.saved = false;
      render();
      dialogEl.showModal();
      // `showModal` focuses the first control, the close button, which then shows a focus
      // ring no one asked for. The dialog itself takes the focus instead; Tab still reaches
      // every control, and Escape still closes it.
      dialogEl.focus();
      await refresh().catch(fail);
    },
  };
}
