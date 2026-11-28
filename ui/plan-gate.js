// Workspace Pipeline Strip & Interactive Hard Lane Plan Gate.
//
// Shows the active Lane pipeline stepper (`Plan ✓ -> Plan Judge ✓ -> [Gate] -> Implement ->
// Review 1 -> Review 2 -> Final Judge`) inside the workspace and renders the interactive
// Plan & Plan Judge review surface when Hard Lane pauses for human approval.

import { el } from "./dom.js";
import { laneLabel } from "./hub.js";

const STAGE_ICON = {
  completed: "✓",
  running: "●",
  failed: "×",
  skipped: "–",
  pending: "○",
};

export function planGateView({
  stripEl,
  gateEl,
  openRun,
  showChanges,
  openPrReview,
  onUpdated,
  onError,
}) {
  let pipelines = [];
  let activeId = null;
  let checkedProposals = new Set();
  let extraText = "";
  let feedbackText = "";
  let busy = false;
  let gateShown = false;

  async function fetchJson(route, init) {
    const response = await fetch(route, init);
    const text = await response.text();
    if (!response.ok) throw new Error(text);
    return JSON.parse(text);
  }

  async function sync() {
    try {
      const list = await fetchJson("api/pipelines");
      pipelines = Array.isArray(list) ? list : [];
      const active = current();
      if (active?.status === "awaitingGate" && active.id !== activeId) {
        activeId = active.id;
        checkedProposals = new Set(active.acceptedProposals ?? []);
        extraText = active.extraInstructions ?? "";
        feedbackText = "";
      } else if (active) {
        activeId = active.id;
      }
      render();
    } catch {
      // No open project yet: hide silently.
      pipelines = [];
      render();
    }
  }

  function current() {
    if (pipelines.length === 0) return null;
    return pipelines.find((p) => p.id === activeId) ?? pipelines[0];
  }

  async function approve(pipeline) {
    if (busy) return;
    busy = true;
    render();
    try {
      const updated = await fetchJson("api/lane/gate/approve", {
        method: "POST",
        body: JSON.stringify({
          pipeline: pipeline.id,
          acceptedProposals: [...checkedProposals],
          extraInstructions: extraText.trim() || null,
        }),
      });
      replace(updated);
      gateShown = false;
      onUpdated();
    } catch (error) {
      onError(String(error.message ?? error));
    } finally {
      busy = false;
      render();
    }
  }

  async function revise(pipeline) {
    if (busy) return;
    if (!feedbackText.trim()) {
      onError("Enter feedback for the Planner before requesting a revision.");
      return;
    }
    busy = true;
    render();
    try {
      const updated = await fetchJson("api/lane/gate/revise", {
        method: "POST",
        body: JSON.stringify({
          pipeline: pipeline.id,
          feedback: feedbackText.trim(),
        }),
      });
      replace(updated);
      gateShown = false;
      onUpdated();
    } catch (error) {
      onError(String(error.message ?? error));
    } finally {
      busy = false;
      render();
    }
  }

  function replace(updated) {
    const idx = pipelines.findIndex((p) => p.id === updated.id);
    if (idx >= 0) pipelines[idx] = updated;
    else pipelines.unshift(updated);
    activeId = updated.id;
  }

  function render() {
    const pipeline = current();
    if (!pipeline) {
      stripEl.hidden = true;
      gateEl.hidden = true;
      return;
    }
    renderStrip(pipeline);
    const shouldShowGate = pipeline.status === "awaitingGate" || gateShown;
    gateEl.hidden = !shouldShowGate;
    if (shouldShowGate) renderGate(pipeline);
  }

  function renderStrip(pipeline) {
    stripEl.hidden = false;
    const head = el("div", "pipeline-head");
    const kindWord = pipeline.kind === "pullRequest" ? `PR #${pipeline.number}` : `Issue #${pipeline.number}`;
    head.append(
      el("span", `hub-lane-chip lane-${pipeline.lane}`, `${laneLabel(pipeline.lane)} Lane`),
      el("span", "pipeline-title", `${kindWord}: ${pipeline.title}`),
      el("span", `pipeline-status status-${pipeline.status}`, statusLabel(pipeline.status)),
    );

    const stepper = el("div", "pipeline-stepper");
    for (const stage of pipeline.stages) {
      const pill = el("button", `pipeline-stage stage-${stage.status}`);
      pill.type = "button";
      pill.disabled = !stage.runId;
      pill.title = `${stage.label} — ${stage.agent}${stage.model ? ` · ${stage.model}` : ""}`;
      pill.append(
        el("span", "stage-glyph", STAGE_ICON[stage.status] ?? "○"),
        el("span", "", `${stage.index + 1}. ${stage.label}`),
      );
      if (stage.runId) {
        pill.addEventListener("click", () => openRun(stage.runId));
      }
      stepper.append(pill);
    }

    const actions = el("div", "pipeline-actions");
    if (pipeline.status === "awaitingGate") {
      const btn = el("button", "button primary hub-mini-btn", "Review Plan Gate");
      btn.type = "button";
      btn.addEventListener("click", () => {
        gateShown = !gateShown;
        render();
      });
      actions.append(btn);
    }
    if (pipeline.kind === "issue" && pipeline.implementRunId) {
      const lastStageWithRun = [...pipeline.stages].reverse().find((s) => s.runId && !s.readOnly);
      const toId = lastStageWithRun?.runId ?? pipeline.implementRunId;
      const diffBtn = el("button", "button hub-mini-btn", "Walkthrough");
      diffBtn.type = "button";
      diffBtn.addEventListener("click", () => showChanges(pipeline.implementRunId, toId));
      actions.append(diffBtn);
    }
    if (pipeline.kind === "pullRequest") {
      const prBtn = el("button", "button primary hub-mini-btn", "PR Review & Submit");
      prBtn.type = "button";
      prBtn.addEventListener("click", () => openPrReview(pipeline));
      actions.append(prBtn);
    }

    stripEl.replaceChildren(head, stepper, actions);
  }

  function renderGate(pipeline) {
    const wrap = el("div", "plan-gate-card");
    const header = el("div", "plan-gate-header");
    header.append(
      el("h2", "plan-gate-title", `Plan Approval Gate · Issue #${pipeline.number}`),
      el("span", "hub-lane-chip lane-hard", "Hard Lane · Stage 2 Complete"),
    );

    const grid = el("div", "plan-gate-grid");

    // Left: Planner (Model A)
    const planBox = el("section", "plan-box");
    const plannerStage = pipeline.stages.find((s) => s.kind === "plan");
    const plannerName = plannerStage
      ? `${plannerStage.agent}${plannerStage.model ? ` · ${plannerStage.model}` : ""}`
      : "Planner";
    planBox.append(el("div", "hub-subhead", `Stage 1 · Architect Plan (${plannerName})`));
    if (pipeline.plan) {
      planBox.append(el("p", "plan-summary", pipeline.plan.summary));
      if (pipeline.plan.approach) {
        planBox.append(el("p", "hub-insp-text", pipeline.plan.approach));
      }
      if (pipeline.plan.steps?.length) {
        const ol = el("ol", "plan-steps");
        for (const step of pipeline.plan.steps) {
          const li = el("li");
          if (step.path) li.append(el("code", "plan-path", step.path), " — ");
          li.append(step.change);
          ol.append(li);
        }
        planBox.append(ol);
      }
      if (pipeline.plan.invariants?.length) {
        planBox.append(
          el("div", "hub-subhead", "Invariants"),
          el("p", "hub-insp-hint", pipeline.plan.invariants.join(" · ")),
        );
      }
      if (pipeline.plan.tests?.length) {
        planBox.append(
          el("div", "hub-subhead", "Planned Tests"),
          el("p", "hub-insp-hint", pipeline.plan.tests.join(" · ")),
        );
      }
    } else {
      planBox.append(el("pre", "hub-insp-desc", pipeline.planReply || "No structured plan captured."));
    }

    // Right: Plan Judge (Model B)
    const judgeBox = el("section", "plan-box");
    const judgeStage = pipeline.stages.find((s) => s.kind === "planJudge");
    const judgeName = judgeStage
      ? `${judgeStage.agent}${judgeStage.model ? ` · ${judgeStage.model}` : ""}`
      : "Plan Judge";
    judgeBox.append(el("div", "hub-subhead", `Stage 2 · Plan Judge & Proposals (${judgeName})`));
    if (pipeline.planJudge) {
      judgeBox.append(
        el("div", "plan-verdict", `Verdict: ${pipeline.planJudge.verdict.toUpperCase()}`),
      );
      if (pipeline.planJudge.critique) {
        judgeBox.append(el("p", "hub-insp-text", pipeline.planJudge.critique));
      }
      if (pipeline.planJudge.proposals?.length) {
        const list = el("div", "proposal-list");
        for (const prop of pipeline.planJudge.proposals) {
          const row = el("label", "proposal-item");
          const cb = el("input");
          cb.type = "checkbox";
          cb.checked = checkedProposals.has(prop.id);
          cb.addEventListener("change", () => {
            if (cb.checked) checkedProposals.add(prop.id);
            else checkedProposals.delete(prop.id);
          });
          const text = el("div", "proposal-text");
          text.append(
            el("div", "proposal-title", `${prop.id} · ${prop.title} (${prop.priority})`),
            el("div", "hub-insp-hint", prop.detail),
          );
          row.append(cb, text);
          list.append(row);
        }
        judgeBox.append(list);
      } else {
        judgeBox.append(el("p", "hub-insp-hint", "Judge proposed no additional changes."));
      }
    } else {
      judgeBox.append(el("p", "hub-insp-hint", "Judge produced no structured proposals."));
    }

    grid.append(planBox, judgeBox);

    // Extra instructions + revision feedback
    const inputs = el("div", "plan-gate-inputs");
    const extraLabel = el("label", "plan-gate-field");
    extraLabel.append(el("span", "hub-subhead", "Extra instructions for Implementer (optional)"));
    const extraBox = el("textarea", "field");
    extraBox.rows = 2;
    extraBox.value = extraText;
    extraBox.placeholder = "Any constraints, naming preferences, or scope limits…";
    extraBox.addEventListener("input", () => {
      extraText = extraBox.value;
    });
    extraLabel.append(extraBox);

    const revLabel = el("label", "plan-gate-field");
    revLabel.append(el("span", "hub-subhead", "Or ask Planner to revise (optional)"));
    const revBox = el("input", "field");
    revBox.type = "text";
    revBox.value = feedbackText;
    revBox.placeholder = "Describe what the Planner should rethink…";
    revBox.addEventListener("input", () => {
      feedbackText = revBox.value;
    });
    revLabel.append(revBox);
    inputs.append(extraLabel, revLabel);

    const actions = el("div", "plan-gate-actions");
    const reviseBtn = el("button", "button", busy ? "Working…" : "Ask Planner to Revise");
    reviseBtn.type = "button";
    reviseBtn.disabled = busy;
    reviseBtn.addEventListener("click", () => void revise(pipeline));

    const approveBtn = el(
      "button",
      "button primary",
      busy ? "Starting Implementer…" : "Approve Plan & Start Implementation →",
    );
    approveBtn.type = "button";
    approveBtn.disabled = busy;
    approveBtn.addEventListener("click", () => void approve(pipeline));
    actions.append(reviseBtn, approveBtn);

    wrap.append(header, grid, inputs, actions);
    gateEl.replaceChildren(wrap);
  }

  return {
    sync,
    current,
    select(id) {
      activeId = id;
      render();
    },
  };
}

function statusLabel(status) {
  if (status === "awaitingGate") return "Awaiting Plan Approval";
  if (status === "running") return "Running";
  if (status === "completed") return "Completed";
  if (status === "failed") return "Failed";
  return "Stopped";
}
