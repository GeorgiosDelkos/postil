// The startup wizard dialog: guides new users (or anyone re-running setup from Settings or the
// Postil menu) through agents, default workflow, appearance (with live preview), and a final
// readiness summary. All edits live in an in-memory draft and commit in one `PUT /api/settings`
// on Finish (or Skip on first run); cancelling restores the prior appearance cleanly.

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
  toggleSwitch,
} from "./settings-controls.js";
import {
  addReviewer,
  moveReviewer,
  removeReviewer,
  setAgent,
  setAgentModel,
  setReviewerModel,
} from "./settings.js";
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

// A short syntax-highlighted snippet (using Postil's own token classes and real tabs) so Theme,
// Code font size, and Tab width update visibly inside Step 3.
const PREVIEW_LINES = [
  [["m", "// Preview how code looks in Postil"]],
  [
    ["k", "fn"],
    ["", " "],
    ["f", "review"],
    ["p", "("],
    ["", "note"],
    ["p", ":"],
    ["", " "],
    ["p", "&"],
    ["t", "Note"],
    ["p", ")"],
    ["", " "],
    ["p", "{"],
  ],
  [
    ["", "\t"],
    ["k", "if"],
    ["", " note"],
    ["p", "."],
    ["f", "is_open"],
    ["p", "()"],
    ["", " "],
    ["p", "{"],
  ],
  [
    ["", "\t\t"],
    ["f", "send_to_agent"],
    ["p", "("],
    ["s", '"N1"'],
    ["p", ","],
    ["", " note"],
    ["p", ");"],
  ],
  [
    ["", "\t"],
    ["p", "}"],
  ],
  [["p", "}"]],
];

export function wizardDialog(
  dialogEl,
  { apply, onSaved, openFolder, canOpenFolder, openSettings, quotaBadge },
) {
  // The menu of agents that "+ Add reviewer" opens.
  const agentMenu = dialogMenu(dialogEl);
  const ui = {
    view: null,
    baseline: null,
    draft: null,
    step: 0,
    firstRun: false,
    touchedAgents: false,
    form: null,
    error: null,
    saving: false,
    committed: false,
    scanning: false,
    pollTimer: null,
    focusNext: null,
    // Incremented on each open/close so stale async replies from an earlier session are ignored.
    session: 0,
  };

  // The enabled agents' models, asked for from the first step on, so the workflow step's
  // selects are filled by the time it shows: Claude's adapter takes seconds to start. A reply
  // after the dialog closed is dropped: closing resets it.
  const models = modelReports({
    onChange: () => {
      if (dialogEl.open) render();
    },
    // Not `fail`, which also ends a save under way: a models request runs beside a save.
    onError: (error) => {
      if (!dialogEl.open) return;
      ui.error = String(error.message ?? error);
      render();
    },
  });

  // A reply whose revision differs from the one on screen means the file changed since the
  // draft was made (another window, the settings dialog, a refused save). The draft then starts
  // over from the file: a save names the reply's revision, so keeping the draft would let
  // Finish or Skip overwrite that change unseen, which the revision check exists to prevent.
  function ingestView(view, { resetDraft = false, session = ui.session } = {}) {
    if (session !== ui.session) return;
    const fileChanged = ui.view !== null && view.revision !== ui.view.revision;
    const scanning = view.agents === null;
    const nextView = {
      ...view,
      agents: scanning ? (ui.view?.agents ?? null) : view.agents,
      tools: scanning ? (ui.view?.tools ?? null) : view.tools,
    };
    const unchanged =
      !resetDraft &&
      scanning === ui.scanning &&
      JSON.stringify(nextView) === JSON.stringify(ui.view);
    ui.view = nextView;
    ui.scanning = scanning;
    // Once the file exists, this is no longer a first run, so Skip goes away.
    ui.firstRun = ui.firstRun && view.firstRun;
    if (resetDraft || ui.draft === null || fileChanged) {
      ui.baseline = structuredClone(view.settings);
      ui.draft = structuredClone(view.settings);
    }
    if (ui.firstRun && !ui.touchedAgents && nextView.agents !== null) {
      ui.draft = applyDetectedDefaults(ui.draft, nextView.agents);
    }
    if (fileChanged) apply(ui.draft);
    if (dialogEl.open && !view.problem) models.request(ui.draft);
    clearTimeout(ui.pollTimer);
    if (scanning && dialogEl.open) {
      ui.pollTimer = setTimeout(
        () => refresh({ session }).catch((e) => fail(e, session)),
        SCAN_POLL_MS,
      );
    }
    if (!unchanged && dialogEl.open) render();
  }

  async function refresh({ resetDraft = false, session = ui.session } = {}) {
    const view = await fetchSettingsJson();
    ingestView(view, { resetDraft, session });
    return view;
  }

  function fail(error, session = ui.session) {
    if (session !== ui.session || !dialogEl.open) return;
    ui.error = String(error.message ?? error);
    ui.saving = false;
    render();
  }

  function updateDraft(next, { preview = false } = {}) {
    ui.draft = next;
    ui.error = null;
    if (preview) apply(ui.draft);
    models.request(ui.draft);
    render();
  }

  function goTo(index) {
    ui.step = clampStep(index);
    ui.form = null;
    ui.error = null;
    render();
    const body = dialogEl.querySelector(".wizard-body");
    if (body) body.scrollTop = 0;
  }

  async function commit(settingsToSave, { andOpenFolder = false } = {}) {
    if (ui.saving || !ui.view) return;
    const session = ui.session;
    ui.saving = true;
    ui.error = null;
    render();
    try {
      const body = JSON.stringify({ revision: ui.view.revision, settings: settingsToSave });
      const reply = await fetchSettingsJson("api/settings", { method: "PUT", body });
      if (session !== ui.session) return;
      ui.committed = true;
      ui.saving = false;
      onSaved(reply);
      dialogEl.close();
      if (andOpenFolder) openFolder();
    } catch (error) {
      if (session !== ui.session) return;
      ui.saving = false;
      ui.error = String(error.message ?? error);
      // A refusal for a changed file (409) is shown with the file as it now is, as its text
      // says; `ingestView` starts the draft over when the revision moved.
      await fetchSettingsJson()
        .then((latest) => {
          if (session !== ui.session) return;
          // The settings dialog's copy follows the file, then the window goes back to the
          // draft's preview, which `onSaved` replaced with the file's settings.
          onSaved(latest);
          ingestView(latest, { session });
          if (latest.problem) ui.error = null;
          apply(ui.draft);
        })
        .catch(() => {});
      if (session === ui.session && dialogEl.open) render();
    }
  }

  // ---- Drawing --------------------------------------------------------------------------

  function render() {
    const active = document.activeElement;
    const focused = ui.focusNext ?? (dialogEl.contains(active) ? active.dataset.key : undefined);
    ui.focusNext = null;
    const scrolled = dialogEl.querySelector(".wizard-body")?.scrollTop ?? 0;
    // The panel is replaced and the menu after it kept, so an open menu keeps the focus.
    const next = panel();
    if (dialogEl.contains(agentMenu.element)) dialogEl.firstElementChild.replaceWith(next);
    else dialogEl.replaceChildren(next, agentMenu.element);
    const body = dialogEl.querySelector(".wizard-body");
    if (body) body.scrollTop = scrolled;
    restoreDialogFocus(dialogEl, focused);
  }

  function panel() {
    const node = el("div", "wizard-panel");
    // One compact bar, as the main window's header: the title, the steps, the close button.
    const header = el("header", "wizard-header");
    const title = el("h2", "wizard-title", ui.firstRun ? "Welcome to Postil" : "Setup Wizard");
    title.id = "wizard-title";
    const close = controlButton("×", "wizard-close", () => dialogEl.close(), "icon-button");
    close.setAttribute("aria-label", "Close");
    close.disabled = ui.saving;
    header.append(title, stepBar(), close);

    const body = el("div", "wizard-body");
    if (!ui.view || !ui.draft) {
      body.append(el("p", "settings-hint", "Loading…"));
    } else if (ui.view.problem) {
      const problem = el("div", "settings-problem");
      problem.setAttribute("role", "alert");
      problem.append(el("p", "", ui.view.problem));
      body.append(problem);
    } else {
      body.append(currentStep());
    }

    node.append(header, body, footer());
    return node;
  }

  function stepBar() {
    const nav = el("nav", "wizard-steps");
    nav.setAttribute("aria-label", "Setup steps");
    STEPS.forEach((step, index) => {
      const state = index === ui.step ? "current" : index < ui.step ? "done" : "upcoming";
      const item = controlButton("", `step-${step.id}`, () => goTo(index), `wizard-step ${state}`);
      if (index === ui.step) item.setAttribute("aria-current", "step");
      item.disabled = ui.saving || !ui.draft || Boolean(ui.view?.problem);
      if (state === "done") {
        const mark = el("span", "wizard-step-mark", "✓");
        mark.setAttribute("aria-hidden", "true");
        item.append(mark);
      }
      item.append(step.label);
      nav.append(item);
    });
    return nav;
  }

  function currentStep() {
    const step = STEPS[ui.step];
    const wrap = el("div", "wizard-step-content");
    wrap.append(el("h3", "wizard-step-title", step.title));
    if (step.id === "agents") wrap.append(...agentsStep());
    else if (step.id === "workflow") wrap.append(...workflowStep());
    else if (step.id === "appearance") wrap.append(...appearanceStep());
    else wrap.append(...readyStep());
    return wrap;
  }

  function agentsStep() {
    const parts = [
      el(
        "p",
        "wizard-intro",
        "Postil sends your review notes to a coding agent over the Agent Client Protocol (ACP). " +
          "Turn on the agents you want to use, or add a custom command.",
      ),
    ];
    if (ui.view.agents === null) {
      parts.push(el("p", "settings-hint scanning", "Looking for installed agents…"));
      return parts;
    }

    const rows = wizardAgentRows(ui.view.agents, ui.draft);
    const list = el("div", "agent-list");
    for (const agent of rows) {
      list.append(
        agentListRow(agent, {
          disabled: ui.saving,
          formOpen: ui.form !== null,
          onToggle: (checked) => {
            ui.touchedAgents = true;
            updateDraft(toggleWizardAgent(ui.draft, agent.name, checked));
          },
          onEdit: () =>
            openForm({ previous: agent.name, name: agent.name, command: agent.command }),
          onRemove: () => {
            ui.touchedAgents = true;
            updateDraft(removeWizardCustomAgent(ui.draft, agent.name));
          },
        }),
      );
    }
    parts.push(list);

    const anyFound = rows.some((agent) => agent.found || !agent.builtIn);
    if (!anyFound) {
      parts.push(
        el(
          "p",
          "wizard-callout",
          "No built-in agents were found on your PATH. Install Claude, Codex, Antigravity (agy), " +
            "Grok, or OpenCode and click Rescan, add a custom ACP agent below, or continue and " +
            "set one up later in Settings (⌘,).",
        ),
      );
    }

    const warning = npxWarning(ui.draft, ui.view.agents, ui.view.tools);
    if (warning) {
      const note = el("p", "wizard-callout warning", warning);
      note.setAttribute("role", "status");
      parts.push(note);
    }

    if (ui.form) {
      parts.push(
        customAgentForm(ui.form, {
          onSubmit: submitAgentForm,
          onCancel: () => {
            ui.form = null;
            ui.focusNext = "wizard-agent-add";
            render();
          },
        }),
      );
    }

    const actions = el("div", "settings-actions");
    const add = controlButton("Add agent…", "wizard-agent-add", () => {
      openForm({ previous: null, name: "", command: "" });
    });
    add.disabled = ui.saving || ui.form !== null;
    const rescan = controlButton("Rescan", "wizard-rescan", async () => {
      const session = ui.session;
      ui.scanning = true;
      render();
      try {
        const view = await fetchSettingsJson("api/agents/rescan", { method: "POST" });
        // A reply after the dialog closed or opened again belongs to no one on screen.
        if (session !== ui.session) return;
        // The rescan forgot every agent's models too: they are asked for again.
        models.reset();
        ingestView(view, { session });
      } catch (error) {
        if (session !== ui.session) return;
        ui.scanning = false;
        fail(error, session);
      }
    });
    rescan.disabled = ui.scanning || ui.saving;
    if (ui.scanning) actions.append(el("span", "settings-hint", "Looking for installed agents…"));
    actions.append(add, rescan);
    parts.push(actions);
    return parts;
  }

  function openForm(form) {
    ui.form = { ...form, error: null };
    ui.error = null;
    render();
    const name = dialogEl.querySelector('[data-key="form-name"]');
    name?.closest("form")?.scrollIntoView({ block: "nearest" });
    name?.focus();
  }

  function submitAgentForm() {
    const form = ui.form;
    const builtIn = ui.view.agents.filter((agent) => agent.builtIn).map((agent) => agent.name);
    const result = saveWizardCustomAgent(ui.draft, form, form.previous, builtIn);
    if (result.error) {
      form.error = result.error;
      render();
      return;
    }
    ui.touchedAgents = true;
    ui.form = null;
    ui.focusNext = "wizard-agent-add";
    updateDraft(result.settings);
  }

  function workflowStep() {
    const enabled = ui.draft.enabledAgents;
    const parts = [
      el(
        "p",
        "wizard-intro",
        "Choose which agent receives your notes by default. You can also add reviewers to check " +
          "an agent's work automatically when you ask for a review.",
      ),
    ];

    if (enabled.length === 0) {
      parts.push(
        el(
          "p",
          "wizard-callout",
          "No agents are turned on yet. Go back to Agents to turn one on, or continue and pick " +
            "one later in Settings (⌘,).",
        ),
      );
    }

    const slot = agentSlotControl("wizard-agent", ui.draft, models.models, {
      disabled: ui.saving,
      onAgent: (name) => {
        const next = setAgent(ui.draft, name);
        if (next.settings) updateDraft(next.settings);
      },
      onModel: (model) => updateDraft(setAgentModel(ui.draft, model).settings),
    });
    const sendRow = settingRow("Send notes to", "The default agent, and the model it runs", slot);
    sendRow.classList.add("stacked");
    parts.push(sendRow);

    const reviewers = reviewerControls(ui.draft, models.models, {
      disabled: ui.saving,
      menu: agentMenu,
      quotaBadge,
      onMove: (index, step) => {
        ui.focusNext = `${step < 0 ? "up" : "down"}-${index + step}`;
        updateDraft(moveReviewer(ui.draft, index, step).settings);
      },
      onRemove: (index) => updateDraft(removeReviewer(ui.draft, index).settings),
      onAdd: (name) => {
        const next = addReviewer(ui.draft, name);
        if (next.settings) updateDraft(next.settings);
      },
      onModel: (index, model) => updateDraft(setReviewerModel(ui.draft, index, model).settings),
    });
    const reviewerRow = settingRow(
      "Reviewers (optional)",
      "Each one reviews the agent's work, in this order",
      reviewers,
    );
    reviewerRow.classList.add("stacked");
    parts.push(reviewerRow);

    parts.push(
      el(
        "p",
        "wizard-tip",
        "Each agent signs in on its own. If your first run fails to start, run the agent's " +
          "command in a terminal once to sign in.",
      ),
    );
    return parts;
  }

  function appearanceStep() {
    const size = ui.draft.codeFontSize;
    const tabs = ui.view.tabWidths.map((width) => [width, String(width)]);
    return [
      el(
        "p",
        "wizard-intro",
        "Pick a look and code size. Changes preview live and can be adjusted any time in Settings.",
      ),
      settingRow(
        "Theme",
        "Follows macOS appearance on System",
        segmented(
          "wizard-theme",
          THEMES,
          ui.draft.theme,
          (theme) => updateDraft({ ...ui.draft, theme }, { preview: true }),
          { disabled: ui.saving },
        ),
      ),
      settingRow(
        "Code font size",
        null,
        fontStepper(
          size,
          ui.view.fontSizes,
          (step) => updateDraft({ ...ui.draft, codeFontSize: size + step }, { preview: true }),
          { disabled: ui.saving },
        ),
      ),
      settingRow(
        "Tab width",
        "Columns per tab character",
        segmented(
          "wizard-tab",
          tabs,
          ui.draft.tabWidth,
          (tabWidth) => updateDraft({ ...ui.draft, tabWidth }, { preview: true }),
          { disabled: ui.saving },
        ),
      ),
      codePreview(),
      settingRow(
        "Reopen the last project",
        "Instead of the welcome screen, when Postil starts without a folder",
        toggleSwitch(
          "wizard-reopen",
          ui.draft.reopenLastProject,
          () => updateDraft({ ...ui.draft, reopenLastProject: !ui.draft.reopenLastProject }),
          { label: "Reopen the last project", disabled: ui.saving },
        ),
      ),
    ];
  }

  function codePreview() {
    const box = el("div", "wizard-preview");
    box.setAttribute("role", "region");
    box.setAttribute("aria-label", "Code preview");
    PREVIEW_LINES.forEach((tokens, index) => {
      const row = el("div", "wizard-preview-line");
      row.append(el("span", "wizard-preview-num", String(index + 1)));
      const code = el("span", "wizard-preview-code");
      for (const [kind, text] of tokens) {
        code.append(kind ? el("span", kind, text) : text);
      }
      row.append(code);
      box.append(row);
    });
    return box;
  }

  function readyStep() {
    const parts = [
      el(
        "p",
        "wizard-intro",
        "Review your choices below. You can change any of these later in Settings (⌘,).",
      ),
    ];

    const summary = el("dl", "wizard-summary");
    for (const item of wizardSummary(ui.draft, models.models)) {
      const row = el("div", "wizard-summary-row");
      row.append(
        el("dt", "wizard-summary-label", item.label),
        el("dd", `wizard-summary-value${item.muted ? " muted" : ""}`, item.value),
      );
      summary.append(row);
    }
    parts.push(summary);

    const needsNpx = Boolean(npxWarning(ui.draft, ui.view.agents, ui.view.tools));
    const items = readinessItems(ui.view.tools, needsNpx);
    if (items.length) {
      const section = el("div", "wizard-readiness");
      section.append(el("h4", "settings-heading", "Environment"));
      const list = el("ul", "wizard-tool-list");
      for (const tool of items) {
        const li = el("li", `wizard-tool ${tool.status}`);
        const detail = el("span", "wizard-tool-detail", tool.detail);
        if (tool.found) detail.title = tool.found;
        li.append(el("span", "wizard-tool-name", tool.name), detail);
        list.append(li);
      }
      section.append(list);
      parts.push(section);
    }
    return parts;
  }

  function footer() {
    const node = el("footer", "wizard-footer");
    const left = el("div", "wizard-footer-left");
    if (ui.firstRun && !ui.view?.problem) {
      // Skip drops the edits but keeps what step 1 showed as found: the installed agents on.
      const skip = controlButton("Skip", "wizard-skip", () =>
        commit(applyDetectedDefaults(ui.baseline, ui.view.agents)),
      );
      skip.disabled = ui.saving || !ui.baseline;
      left.append(skip);
    }
    if (ui.error) {
      const status = el("span", "settings-status failed", ui.error);
      status.setAttribute("role", "alert");
      left.append(status);
    }

    const right = el("div", "wizard-footer-right");
    const lastStep = ui.step === STEPS.length - 1;
    if (ui.step > 0 && !ui.view?.problem) {
      const back = controlButton("Back", "wizard-back", () => goTo(ui.step - 1));
      back.disabled = ui.saving;
      right.append(back);
    }
    if (ui.view?.problem) {
      if (ui.view.file && openSettings) {
        right.append(
          controlButton("Open Settings…", "wizard-open-settings", () => {
            dialogEl.close();
            openSettings();
          }),
        );
      }
      right.append(
        controlButton("Close", "wizard-done-close", () => dialogEl.close(), "button primary"),
      );
    } else if (!lastStep) {
      const next = controlButton("Next", "wizard-next", () => goTo(ui.step + 1), "button primary");
      next.disabled = ui.saving || !ui.draft;
      right.append(next);
    } else {
      if (canOpenFolder()) {
        const finishOpen = controlButton("Finish & Open Folder…", "wizard-finish-open", () =>
          commit(ui.draft, { andOpenFolder: true }),
        );
        finishOpen.disabled = ui.saving || !ui.draft;
        right.append(finishOpen);
      }
      const finish = controlButton(
        ui.saving ? "Saving…" : "Finish",
        "wizard-finish",
        () => commit(ui.draft),
        "button primary",
      );
      finish.disabled = ui.saving || !ui.draft;
      right.append(finish);
    }

    node.append(left, right);
    return node;
  }

  dialogEl.addEventListener("cancel", (event) => {
    if (ui.saving) event.preventDefault();
  });

  dialogEl.addEventListener("close", () => {
    ui.session += 1;
    clearTimeout(ui.pollTimer);
    models.reset();
    if (!ui.committed && ui.baseline) apply(ui.baseline);
    ui.form = null;
    ui.error = null;
    ui.saving = false;
  });

  // A click on the backdrop lands on the dialog element itself, outside its panel.
  dialogEl.addEventListener("mousedown", (event) => {
    if (event.target === dialogEl && !ui.saving) dialogEl.close();
  });

  return {
    async open({ firstRun = false, view = null } = {}) {
      if (dialogEl.open) return;
      const session = ++ui.session;
      ui.firstRun = firstRun;
      ui.step = 0;
      ui.form = null;
      ui.error = null;
      ui.saving = false;
      ui.committed = false;
      ui.view = null;
      ui.draft = null;
      ui.baseline = null;
      ui.touchedAgents = !firstRun;
      if (view) {
        ui.touchedAgents = !firstRun || view.settings.enabledAgents.length > 0;
        ingestView(view, { resetDraft: true, session });
      }
      render();
      dialogEl.showModal();
      dialogEl.focus();
      // Fetch fresh settings/revision and start polling if an agent scan is still running.
      await refresh({ resetDraft: !view, session }).catch((error) => fail(error, session));
    },
    close() {
      if (dialogEl.open && !ui.saving) dialogEl.close();
    },
  };
}
