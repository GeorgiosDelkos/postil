// The settings dialog: draws `api/settings` and saves every change as it is made, so there is
// no Save button and nothing to lose by closing. The rules for each change are in
// settings.js; the Rust process checks them again and owns the file (src/settings.rs).
//
// The dialog is redrawn whole after every change: it has a few dozen controls, so a redraw
// costs nothing noticeable, and one drawing function is easier to keep right than updates by
// hand. The control that had the focus gets it back by its `data-key`.

import { el } from "./dom.js";
import {
  SCAN_POLL_MS,
  THEMES,
  agentListRow,
  agentSlotControl,
  controlButton,
  customAgentForm,
  dialogMenu,
  fetchSettingsJson,
  fontStepper,
  modelReports,
  restoreDialogFocus,
  reviewerControls,
  segmented,
  settingRow,
  settingSection,
  toggleSwitch,
} from "./settings-controls.js";
import {
  addReviewer,
  agentRows,
  moveReviewer,
  removeCustomAgent,
  removeReviewer,
  saveCustomAgent,
  setAgent,
  setAgentModel,
  setEnabled,
  setReviewerModel,
} from "./settings.js";

// `apply` is called with the settings in effect whenever they change (and once on opening),
// so the page can follow them. `openWizard` opens the startup wizard. Closing gives the focus
// back to where it was when the dialog opened, as `<dialog>` does: a note being written keeps it.
export function settingsDialog(dialogEl, { apply, openWizard }) {
  // The menu of agents that "+ Add reviewer" opens.
  const agentMenu = dialogMenu(dialogEl);
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
    // Saves not answered yet. While there are any, the settings on screen are ahead of the
    // process's: they hold changes still waiting to be saved.
    pending: 0,
    // The control to focus after the next redraw, when it is not the one focused now.
    focusNext: null,
    // Whether an agent scan runs. The agents of the last scan stay listed meanwhile, so the
    // dialog does not jump.
    scanning: false,
    pollTimer: null,
  };

  // The enabled agents' models, for the model selects. Asked for only while the dialog is
  // open: loading the settings at start-up must not start every agent.
  const models = modelReports({
    onChange: () => {
      if (dialogEl.open) render();
    },
    onError: fail,
  });

  // Takes a reply of api/settings. Its settings replace the ones on screen only when no save
  // is waiting: until then the screen holds changes the reply does not know of, and the next
  // change must start from them, or a quick second change would undo the first.
  //
  // While a scan runs, the reply is asked for again shortly; a reply that changes nothing is
  // not drawn, since a redraw replaces every control and a click pressed on one of them and
  // released on its replacement does nothing.
  function show(view) {
    const ahead = ui.pending > 0 && ui.view !== null;
    const scanning = view.agents === null;
    const next = {
      ...view,
      settings: ahead ? ui.view.settings : view.settings,
      agents: scanning ? (ui.view?.agents ?? null) : view.agents,
    };
    const unchanged = scanning === ui.scanning && JSON.stringify(next) === JSON.stringify(ui.view);
    ui.view = next;
    ui.scanning = scanning;
    if (!ahead) apply(view.settings);
    if (dialogEl.open && !view.problem) models.request(ui.view.settings);
    clearTimeout(ui.pollTimer);
    if (scanning && dialogEl.open) {
      ui.pollTimer = setTimeout(() => refresh().catch(fail), SCAN_POLL_MS);
    }
    if (!unchanged && dialogEl.open) render();
  }

  function fail(error) {
    ui.error = String(error.message ?? error);
    ui.saved = false;
    render();
  }

  async function refresh() {
    const view = await fetchSettingsJson();
    show(view);
    return view;
  }

  // Shows `next` at once and saves it, after the saves before it. Resolves to whether it was
  // saved. A refusal shows why; once no other save waits, the saved settings come back.
  function save(next) {
    if (next.error) {
      fail(new Error(next.error));
      return Promise.resolve(false);
    }
    ui.error = null;
    ui.saved = false;
    ui.pending += 1;
    ui.view = { ...ui.view, settings: next.settings };
    models.request(next.settings);
    render();
    ui.queue = ui.queue.then(async () => {
      try {
        // The revision of the last reply: the file as this save expects to find it. A save
        // queued behind another gets the revision that one's reply brought.
        const body = JSON.stringify({ revision: ui.view.revision, settings: next.settings });
        const reply = await fetchSettingsJson("api/settings", { method: "PUT", body });
        ui.pending -= 1;
        ui.saved = ui.pending === 0;
        // This save carried every change before it, so an earlier failure no longer holds.
        if (ui.saved) ui.error = null;
        show(reply);
        return true;
      } catch (error) {
        ui.pending -= 1;
        fail(error);
        if (ui.pending === 0) await refresh().catch(() => {});
        return false;
      }
    });
    return ui.queue;
  }

  const change = (update) => () => save(update(ui.view.settings));

  // ---- Drawing --------------------------------------------------------------------------

  // Redraws the dialog where it was: same scroll position, same focused control. When that
  // control is now disabled (+ at the largest size), the first enabled control beside it
  // takes the focus; when it is gone (a removed reviewer), the dialog does, so the keyboard
  // stays inside.
  function render() {
    const active = document.activeElement;
    const focused = ui.focusNext ?? (dialogEl.contains(active) ? active.dataset.key : undefined);
    ui.focusNext = null;
    const scrolled = dialogEl.querySelector(".settings-body")?.scrollTop ?? 0;
    // The panel is replaced and the menu after it kept, so an open menu keeps the focus.
    const next = panel();
    if (dialogEl.contains(agentMenu.element)) dialogEl.firstElementChild.replaceWith(next);
    else dialogEl.replaceChildren(next, agentMenu.element);
    dialogEl.querySelector(".settings-body").scrollTop = scrolled;
    restoreDialogFocus(dialogEl, focused);
  }

  function panel() {
    const node = el("div", "settings-panel");
    const header = el("header", "settings-header");
    const title = el("h2", "settings-title", "Settings");
    title.id = "settings-title";
    const close = controlButton("×", "close", () => dialogEl.close(), "icon-button");
    close.setAttribute("aria-label", "Close");
    header.append(title, close);

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
        controlButton("Cancel", "reset-cancel", () => {
          ui.confirmReset = false;
          render();
        }),
        controlButton("Reset", "reset-confirm", resetSettings, "button danger"),
      );
    } else {
      actions.append(
        controlButton("Reset to defaults…", "reset", () => {
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
      show(await fetchSettingsJson("api/settings/reset", { method: "POST" }));
    } catch (error) {
      fail(error);
    }
  }

  // Controls are off while the file cannot be saved over: a change would only be refused.
  const locked = () => Boolean(ui.view.problem);

  function appearance() {
    const settings = ui.view.settings;
    const size = settings.codeFontSize;
    const stepper = fontStepper(
      size,
      ui.view.fontSizes,
      (step) => change((s) => ({ settings: { ...s, codeFontSize: size + step } }))(),
      { disabled: locked() },
    );
    const tabs = ui.view.tabWidths.map((width) => [width, String(width)]);
    return settingSection(
      "Appearance",
      settingRow(
        "Theme",
        null,
        segmented(
          "theme",
          THEMES,
          settings.theme,
          (theme) => change((s) => ({ settings: { ...s, theme } }))(),
          { disabled: locked() },
        ),
      ),
      settingRow("Code font size", null, stepper),
      settingRow(
        "Tab width",
        "Columns per tab character",
        segmented(
          "tab",
          tabs,
          settings.tabWidth,
          (tabWidth) => change((s) => ({ settings: { ...s, tabWidth } }))(),
          { disabled: locked() },
        ),
      ),
    );
  }

  function startup() {
    const on = ui.view.settings.reopenLastProject;
    const flip = change((s) => ({ settings: { ...s, reopenLastProject: !on } }));
    const control = toggleSwitch("reopen", on, flip, {
      label: "Reopen the last project",
      disabled: locked(),
    });
    const rows = [
      settingRow(
        "Reopen the last project",
        "Instead of the welcome screen, when Postil starts without a folder",
        control,
      ),
    ];
    if (openWizard) {
      const launch = controlButton("Run Setup Wizard…", "run-wizard", () => {
        dialogEl.close();
        openWizard();
      });
      launch.disabled = locked();
      rows.push(
        settingRow("Setup wizard", "Walk through agents, appearance and startup settings", launch),
      );
    }
    return settingSection("Startup", ...rows);
  }

  function agents() {
    const node = settingSection(
      "Agents",
      el(
        "p",
        "settings-hint",
        "Turn on the agents Postil may use. Notes go to one agent; reviewers then check its " +
          "work, one after another.",
      ),
    );
    // Only before the first scan finished is there no list to show.
    if (ui.view.agents === null) {
      node.append(el("p", "settings-hint scanning", "Looking for installed agents…"));
      return node;
    }
    const list = el("div", "agent-list");
    for (const agent of agentRows(ui.view.agents, ui.view.settings)) {
      list.append(
        agentListRow(agent, {
          disabled: locked(),
          formOpen: ui.form !== null,
          onToggle: (checked) => save(setEnabled(ui.view.settings, agent.name, checked)),
          onEdit: () =>
            openForm({ previous: agent.name, name: agent.name, command: agent.command }),
          onRemove: () => save(removeCustomAgent(ui.view.settings, agent.name)),
        }),
      );
    }
    const actions = el("div", "settings-actions");
    const add = controlButton("Add agent…", "agent-add", () => {
      openForm({ previous: null, name: "", command: "" });
    });
    add.disabled = locked() || ui.form !== null;
    // One scan at a time: every click would start another login shell. The button is off
    // from the click until the scan's result arrives.
    const rescan = controlButton("Rescan", "rescan", async () => {
      ui.scanning = true;
      render();
      try {
        const view = await fetchSettingsJson("api/agents/rescan", { method: "POST" });
        // The rescan forgot every agent's models too: they are asked for again.
        models.reset();
        show(view);
      } catch (error) {
        ui.scanning = false;
        fail(error);
      }
    });
    rescan.disabled = ui.scanning;
    if (ui.scanning) actions.append(el("span", "settings-hint", "Looking for installed agents…"));
    actions.append(add, rescan);
    node.append(list);
    if (ui.form) {
      node.append(
        customAgentForm(ui.form, {
          onSubmit: submitAgentForm,
          onCancel: () => {
            ui.form = null;
            render();
          },
        }),
      );
    }
    node.append(actions, chosenAgent(), reviewers());
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

  // The form stays open until the process accepts the agent, and shows its refusal: the
  // page's rules are a first check only (a pasted name with an unusual space passes them).
  async function submitAgentForm() {
    const form = ui.form;
    const builtIn = ui.view.agents.filter((agent) => agent.builtIn).map((agent) => agent.name);
    const next = saveCustomAgent(ui.view.settings, form, form.previous, builtIn);
    if (next.error) {
      form.error = next.error;
      render();
      return;
    }
    if (await save(next)) ui.form = null;
    else {
      form.error = ui.error;
      ui.error = null;
    }
    render();
  }

  function chosenAgent() {
    const control = agentSlotControl("agent", ui.view.settings, models.models, {
      disabled: locked(),
      onAgent: (name) => save(setAgent(ui.view.settings, name)),
      onModel: (model) => save(setAgentModel(ui.view.settings, model)),
    });
    const node = settingRow(
      "Send notes to",
      "The agent that works on the notes, and its model",
      control,
    );
    node.classList.add("stacked");
    return node;
  }

  function reviewers() {
    const control = reviewerControls(ui.view.settings, models.models, {
      disabled: locked(),
      menu: agentMenu,
      onMove: (index, step) => {
        // The focus follows the reviewer it moved, so pressing again keeps moving it.
        ui.focusNext = `${step < 0 ? "up" : "down"}-${index + step}`;
        save(moveReviewer(ui.view.settings, index, step));
      },
      onRemove: (index) => save(removeReviewer(ui.view.settings, index)),
      onAdd: (name) => save(addReviewer(ui.view.settings, name)),
      onModel: (index, model) => save(setReviewerModel(ui.view.settings, index, model)),
    });
    const node = settingRow(
      "Reviewers",
      "Each one reviews the agent's work, in this order",
      control,
    );
    node.classList.add("stacked");
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
    models.reset();
    ui.form = null;
    ui.confirmReset = false;
  });
  // A click on the backdrop lands on the dialog element itself, outside its panel.
  dialogEl.addEventListener("mousedown", (event) => {
    if (event.target === dialogEl) dialogEl.close();
  });

  return {
    // Reads the settings, applies them without opening the dialog, and returns the view.
    async load() {
      const view = await fetchSettingsJson();
      show(view);
      return view;
    },
    // Updates the cached view when another flow (the startup wizard) saved settings.
    sync(view) {
      show(view);
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
    close() {
      if (dialogEl.open) dialogEl.close();
    },
  };
}
