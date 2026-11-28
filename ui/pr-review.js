// Native GitHub PR Review submission dialog for PR review pipelines.
// Collects agent findings from Easy/Middle/Hard PR review lanes alongside open, anchored margin
// notes, lets the user pick which comments to include, and posts the review via `gh`.

import { el, postilCheckbox, uiIcon } from "./dom.js";
import { laneLabel } from "./hub.js";

export function prReviewDialog(dialogEl, { notes, onSubmitted }) {
  let pipeline = null;
  let included = new Set();
  let verdict = "comment";
  let summary = "";
  let submitting = false;
  let submitError = null;

  function open(nextPipeline) {
    pipeline = nextPipeline;
    included = new Set((pipeline.findings ?? []).map((f) => f.id));
    // Margin notes stay unchecked by default so out-of-diff workspace notes never trigger 422
    // unless the engineer explicitly includes them.
    if (pipeline.status === "running") {
      verdict = "comment";
    } else {
      verdict = pipeline.findings?.length ? "requestChanges" : "approve";
    }
    summary = "";
    submitting = false;
    submitError = null;
    render();
    if (!dialogEl.open) {
      dialogEl.showModal();
      dialogEl.focus();
    }
  }

  function eligibleNotes() {
    return notes().filter(
      (n) =>
        n.target === "lines" &&
        n.intent !== "ask" &&
        n.state !== "resolved" &&
        n.anchor !== "lost" &&
        n.lines?.end,
    );
  }

  async function submit() {
    if (submitting || !pipeline) return;
    submitting = true;
    submitError = null;
    render();
    const comments = [];
    const generalFindings = [];
    for (const finding of pipeline.findings ?? []) {
      if (!included.has(finding.id)) continue;
      let body = finding.text;
      if (finding.fix) body += `\n\nSuggested fix: ${finding.fix}`;
      if (finding.memory) body += `\n\n_(Team memory: \`${finding.memory}\`)_`;
      if (finding.path && finding.line && finding.line >= 1) {
        comments.push({ path: finding.path, line: finding.line, body });
      } else {
        generalFindings.push(`- **${finding.id}**: ${body}`);
      }
    }
    for (const note of eligibleNotes()) {
      if (!included.has(note.id)) continue;
      comments.push({ path: note.path, line: note.lines.end, body: note.body });
    }
    let finalBody = summary.trim();
    if (generalFindings.length > 0) {
      const section = `### Additional Findings\n${generalFindings.join("\n")}`;
      finalBody = finalBody ? `${finalBody}\n\n${section}` : section;
    }
    try {
      const response = await fetch("api/pr/submit-review", {
        method: "POST",
        body: JSON.stringify({
          pipeline: pipeline.id,
          number: pipeline.number,
          verdict,
          body: finalBody,
          comments,
        }),
      });
      if (!response.ok) throw new Error(await response.text());
      submitting = false;
      dialogEl.close();
      onSubmitted(`Submitted review to PR #${pipeline.number} (${comments.length} inline comments).`);
    } catch (error) {
      submitting = false;
      submitError = String(error.message ?? error);
      if (dialogEl.open) render();
    }
  }

  function render() {
    if (!pipeline) return;
    const panel = el("div", "settings-panel pr-review-panel");
    const header = el("header", "settings-header");
    const title = el("h2", "settings-title", `PR #${pipeline.number} Review · ${laneLabel(pipeline.lane)} Lane`);
    title.id = "pr-review-title";
    const close = el("button", "icon-button");
    close.type = "button";
    close.append(uiIcon("close"));
    close.setAttribute("aria-label", "Close");
    close.disabled = submitting;
    close.addEventListener("click", () => {
      if (!submitting) dialogEl.close();
    });
    header.append(title, close);

    const body = el("div", "settings-body pr-review-body");
    body.append(el("p", "hub-insp-text", pipeline.title));

    if (pipeline.status === "running") {
      body.append(
        el(
          "div",
          "hub-banner",
          "This PR review pipeline is still running. Wait for all stages to finish before approving, or submit a comment now.",
        ),
      );
    }

    if (pipeline.prReviewSubmitted) {
      body.append(
        el(
          "div",
          "hub-banner",
          `Previously submitted (${pipeline.prReviewSubmitted.verdict}, ${pipeline.prReviewSubmitted.commentsCount} comments).`,
        ),
      );
    }

    const findings = pipeline.findings ?? [];
    body.append(el("div", "hub-subhead", `Agent Findings (${findings.length})`));
    if (findings.length === 0) {
      body.append(el("p", "hub-insp-hint", "No structured agent findings recorded yet."));
    } else {
      for (const f of findings) {
        const hasLine = Boolean(f.path && f.line && f.line >= 1);
        const row = el("label", "proposal-item");
        const cb = postilCheckbox({
          checked: included.has(f.id),
          disabled: submitting,
          ariaLabel: `Include finding ${f.id}`,
          onChange: (checked) => {
            if (checked) included.add(f.id);
            else included.delete(f.id);
          },
        });
        const where = hasLine ? `${f.path}:${f.line}` : "(included in summary)";
        const info = el("div", "proposal-text");
        info.append(
          el("div", "proposal-title", `${f.id} · ${where}${f.memory ? ` · ${f.memory}` : ""}`),
          el("div", "hub-insp-hint", f.text),
        );
        row.append(cb, info);
        body.append(row);
      }
    }

    const lineNotes = eligibleNotes();
    if (lineNotes.length > 0) {
      body.append(el("div", "hub-subhead", `Your Open Margin Notes (${lineNotes.length})`));
      for (const n of lineNotes) {
        const row = el("label", "proposal-item");
        const cb = postilCheckbox({
          checked: included.has(n.id),
          disabled: submitting,
          ariaLabel: `Include note ${n.id}`,
          onChange: (checked) => {
            if (checked) included.add(n.id);
            else included.delete(n.id);
          },
        });
        const info = el("div", "proposal-text");
        info.append(
          el("div", "proposal-title", `${n.id} · ${n.path}:${n.lines.end}`),
          el("div", "hub-insp-hint", n.body),
        );
        row.append(cb, info);
        body.append(row);
      }
    }

    const verdictWrap = el("div", "pr-verdict-row");
    verdictWrap.append(el("span", "hub-subhead", "GitHub Verdict"));
    const select = el("select", "select");
    select.setAttribute("aria-label", "GitHub review verdict");
    select.disabled = submitting;
    for (const [val, label] of [
      ["comment", "Comment"],
      ["approve", "Approve"],
      ["requestChanges", "Request Changes"],
    ]) {
      const opt = el("option", "", label);
      opt.value = val;
      opt.selected = verdict === val;
      select.append(opt);
    }
    select.addEventListener("change", () => {
      verdict = select.value;
    });
    verdictWrap.append(select);
    body.append(verdictWrap);

    const sumLabel = el("label", "plan-gate-field");
    sumLabel.append(el("span", "hub-subhead", "Overall review summary"));
    const sumArea = el("textarea", "field");
    sumArea.rows = 3;
    sumArea.value = summary;
    sumArea.disabled = submitting;
    sumArea.placeholder = "Optional overall summary posted with the review…";
    sumArea.addEventListener("input", () => {
      summary = sumArea.value;
    });
    sumLabel.append(sumArea);
    body.append(sumLabel);

    const footer = el("footer", "settings-footer pr-review-footer");
    const statusEl = el("span", `settings-status${submitError ? " failed" : ""}`, submitError ?? "");
    statusEl.setAttribute("role", "status");
    const btns = el("div", "pr-review-actions");
    const cancel = el("button", "button", "Cancel");
    cancel.type = "button";
    cancel.disabled = submitting;
    cancel.addEventListener("click", () => {
      if (!submitting) dialogEl.close();
    });
    const postBtn = el("button", "button primary hub-icon-btn");
    postBtn.type = "button";
    postBtn.disabled = submitting;
    postBtn.append(
      el("span", "", submitting ? "Posting…" : `Post Review to PR #${pipeline.number}`),
      uiIcon("arrowRight"),
    );
    postBtn.addEventListener("click", () => void submit());
    btns.append(cancel, postBtn);
    footer.append(statusEl, btns);

    panel.append(header, body, footer);
    dialogEl.replaceChildren(panel);
  }

  dialogEl.addEventListener("cancel", (event) => {
    if (submitting) event.preventDefault();
  });
  dialogEl.addEventListener("mousedown", (event) => {
    if (event.target === dialogEl && !submitting) dialogEl.close();
  });

  return { open };
}
