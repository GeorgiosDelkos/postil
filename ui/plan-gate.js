// Workspace Pipeline Strip & Interactive Hard Lane Plan Gate.
//
// Shows the active Lane pipeline stepper (`Plan -> Plan Judge -> [Gate] -> Implement ->
// Review 1 -> Review 2 -> Final Judge`) inside the workspace using Postil's native `statusIcon`,
// `agentLogoIcon`, `postilCheckbox`, and inline SVG icons, and renders the interactive Plan &
// Plan Judge review surface when Hard Lane pauses for human approval.

import { agentLogoIcon, el, postilCheckbox, statusIcon, uiIcon } from "./dom.js";
import { laneLabel } from "./hub.js";

const STAGE_TO_STATUS_ICON = {
  completed: "resolved",
  running: "working",
  failed: "failed",
  skipped: "silent",
  pending: "pending",
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
  let dismissedIds = new Set();
  let checkedProposals = new Set();
  let extraText = "";
  let feedbackText = "";
  let busy = false;
  let gateCollapsed = false;
  let lastGateKey = null;
  let lastRenderedGateSig = null;
  let syncSeq = 0;

  async function fetchJson(route, init) {
    const response = await fetch(route, init);
    const text = await response.text();
    if (!response.ok) throw new Error(text);
    return JSON.parse(text);
  }

  function gateKeyFor(pipeline) {
    if (!pipeline) return null;
    const judgeRun = pipeline.stages?.find((s) => s.kind === "planJudge")?.runId ?? "";
    const planRun = pipeline.stages?.find((s) => s.kind === "plan")?.runId ?? "";
    return `${pipeline.id}:${planRun}:${judgeRun}:${pipeline.status}`;
  }

  function syncGateState(pipeline) {
    const key = gateKeyFor(pipeline);
    if (pipeline?.status === "awaitingGate" && key !== lastGateKey) {
      lastGateKey = key;
      checkedProposals = new Set(pipeline.acceptedProposals ?? []);
      extraText = pipeline.extraInstructions ?? "";
      feedbackText = "";
      gateCollapsed = false;
      lastRenderedGateSig = null;
    } else if (pipeline?.status !== "awaitingGate") {
      lastGateKey = key;
    }
  }

  async function sync() {
    const seq = ++syncSeq;
    try {
      const list = await fetchJson("api/pipelines");
      if (seq !== syncSeq) return;
      pipelines = Array.isArray(list) ? list : [];
      const active = current();
      if (active) {
        activeId = active.id;
        syncGateState(active);
      }
      render();
    } catch {
      if (seq !== syncSeq) return;
      pipelines = [];
      render();
    }
  }

  function visiblePipelines() {
    return pipelines.filter((p) => {
      const terminal = p.status === "completed" || p.status === "failed" || p.status === "stopped";
      return !(terminal && dismissedIds.has(p.id));
    });
  }

  function current() {
    const visible = visiblePipelines();
    if (visible.length === 0) return null;
    return visible.find((p) => p.id === activeId) ?? visible[0];
  }

  async function approve(pipeline) {
    if (busy) return;
    const seq = ++syncSeq;
    busy = true;
    lastRenderedGateSig = null;
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
      if (seq !== syncSeq) return;
      replace(updated);
      syncGateState(updated);
      onUpdated();
    } catch (error) {
      if (seq !== syncSeq) return;
      onError(String(error.message ?? error));
    } finally {
      busy = false;
      lastRenderedGateSig = null;
      render();
    }
  }

  async function revise(pipeline) {
    if (busy) return;
    if (!feedbackText.trim()) {
      onError("Enter feedback for the Planner before requesting a revision.");
      return;
    }
    const seq = ++syncSeq;
    busy = true;
    lastRenderedGateSig = null;
    render();
    try {
      const updated = await fetchJson("api/lane/gate/revise", {
        method: "POST",
        body: JSON.stringify({
          pipeline: pipeline.id,
          feedback: feedbackText.trim(),
        }),
      });
      if (seq !== syncSeq) return;
      feedbackText = "";
      extraText = "";
      checkedProposals.clear();
      replace(updated);
      syncGateState(updated);
      onUpdated();
    } catch (error) {
      if (seq !== syncSeq) return;
      onError(String(error.message ?? error));
    } finally {
      busy = false;
      lastRenderedGateSig = null;
      render();
    }
  }

  function replace(updated) {
    dismissedIds.delete(updated.id);
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
      lastRenderedGateSig = null;
      return;
    }
    renderStrip(pipeline);
    const shouldShowGate = pipeline.status === "awaitingGate" && !gateCollapsed;
    gateEl.hidden = !shouldShowGate;
    if (shouldShowGate) {
      const sig = JSON.stringify({
        key: gateKeyFor(pipeline),
        busy,
        plan: pipeline.plan,
        planReply: pipeline.planReply,
        planJudge: pipeline.planJudge,
      });
      if (sig !== lastRenderedGateSig) {
        lastRenderedGateSig = sig;
        renderGate(pipeline);
      }
    } else {
      lastRenderedGateSig = null;
    }
  }

  function renderStrip(pipeline) {
    stripEl.hidden = false;
    const visible = visiblePipelines();
    const head = el("div", "pipeline-head");
    const kindWord = pipeline.kind === "pullRequest" ? `PR #${pipeline.number}` : `Issue #${pipeline.number}`;
    head.append(
      el("span", `hub-lane-chip lane-${pipeline.lane}`, `${laneLabel(pipeline.lane)} Lane`),
      uiIcon(pipeline.kind === "pullRequest" ? "pr" : "issue", "hub-meta-icon"),
      el("span", "pipeline-title", `${kindWord}: ${pipeline.title}`),
      el("span", `pipeline-status status-${pipeline.status}`, statusLabel(pipeline.status)),
    );

    if (visible.length > 1) {
      const switcher = el("select", "select pipeline-switcher");
      switcher.setAttribute("aria-label", "Switch active pipeline");
      for (const each of visible) {
        const prefix = each.kind === "pullRequest" ? "PR" : "Issue";
        const opt = el("option", "", `${prefix} #${each.number} (${laneLabel(each.lane)})`);
        opt.value = each.id;
        opt.selected = each.id === pipeline.id;
        switcher.append(opt);
      }
      switcher.addEventListener("change", () => {
        activeId = switcher.value;
        syncGateState(current());
        lastRenderedGateSig = null;
        render();
      });
      head.append(switcher);
    }

    const stepper = el("div", "pipeline-stepper");
    for (const stage of pipeline.stages) {
      const pill = el("button", `pipeline-stage stage-${stage.status}`);
      pill.type = "button";
      pill.disabled = !stage.runId;
      const who = `${stage.agent}${stage.model ? ` · ${stage.model}` : ""}`;
      pill.title = stage.error
        ? `${stage.label} — ${who}\nError: ${stage.error}`
        : `${stage.label} — ${who}`;
      pill.append(
        statusIcon(STAGE_TO_STATUS_ICON[stage.status] ?? "pending"),
        el("span", "", `${stage.index + 1}. ${stage.label}`),
      );
      if (stage.runId) {
        pill.addEventListener("click", () => openRun(stage.runId));
      }
      stepper.append(pill);
    }

    const actions = el("div", "pipeline-actions");
    if (pipeline.status === "awaitingGate") {
      const btn = el("button", "button primary hub-mini-btn hub-icon-btn");
      btn.type = "button";
      btn.append(
        uiIcon(gateCollapsed ? "chevronRight" : "chevronDown"),
        el("span", "", gateCollapsed ? "Review Plan Gate" : "Hide Plan Gate"),
      );
      btn.addEventListener("click", () => {
        gateCollapsed = !gateCollapsed;
        lastRenderedGateSig = null;
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
    const terminal =
      pipeline.status === "completed" ||
      pipeline.status === "failed" ||
      pipeline.status === "stopped";
    if (terminal) {
      const dismiss = el("button", "icon-button pipeline-dismiss");
      dismiss.type = "button";
      dismiss.append(uiIcon("close"));
      dismiss.setAttribute("aria-label", "Dismiss pipeline bar");
      dismiss.title = "Dismiss pipeline bar";
      dismiss.addEventListener("click", () => {
        dismissedIds.add(pipeline.id);
        render();
      });
      actions.append(dismiss);
    }

    stripEl.replaceChildren(head, stepper, actions);
  }

  function renderStageSubhead(prefix, stage, fallbackName) {
    const row = el("div", "hub-subhead plan-stage-head");
    row.append(el("span", "", prefix));
    if (stage) {
      const who = el("span", "plan-stage-agent");
      const logo = agentLogoIcon(stage.agent);
      if (logo) who.append(logo);
      who.append(el("span", "", `${stage.agent}${stage.model ? ` · ${stage.model}` : ""}`));
      row.append(who);
    } else {
      row.append(el("span", "", fallbackName));
    }
    return row;
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
    planBox.append(renderStageSubhead("Stage 1 · Architect Plan", plannerStage, "Planner"));
    if (plannerStage?.error && !pipeline.plan) {
      planBox.append(el("div", "hub-roster-error", `Plan parse note: ${plannerStage.error}`));
    }
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
      if (pipeline.plan.risks?.length) {
        planBox.append(
          el("div", "hub-subhead", "Risks"),
          el("p", "hub-insp-hint", pipeline.plan.risks.join(" · ")),
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
    judgeBox.append(renderStageSubhead("Stage 2 · Plan Judge & Proposals", judgeStage, "Plan Judge"));
    if (judgeStage?.error && !pipeline.planJudge) {
      judgeBox.append(el("div", "hub-roster-error", `Judge parse note: ${judgeStage.error}`));
    }
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
          const cb = postilCheckbox({
            checked: checkedProposals.has(prop.id),
            ariaLabel: `Include proposal ${prop.id}: ${prop.title}`,
            onChange: (checked) => {
              if (checked) checkedProposals.add(prop.id);
              else checkedProposals.delete(prop.id);
            },
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
    extraBox.disabled = busy;
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
    revBox.disabled = busy;
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

    const approveBtn = el("button", "button primary hub-icon-btn");
    approveBtn.type = "button";
    approveBtn.disabled = busy;
    approveBtn.append(
      el("span", "", busy ? "Starting Implementer…" : "Approve Plan & Start Implementation"),
      uiIcon("arrowRight"),
    );
    approveBtn.addEventListener("click", () => void approve(pipeline));
    actions.append(reviseBtn, approveBtn);

    wrap.append(header, grid, inputs, actions);
    gateEl.replaceChildren(wrap);
  }

  return {
    sync,
    current,
    select(id) {
      dismissedIds.delete(id);
      activeId = id;
      syncGateState(current());
      lastRenderedGateSig = null;
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
