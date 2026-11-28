// Native GitHub PR Review submission dialog for PR review pipelines.
// Collects agent findings from Easy/Middle/Hard PR review lanes alongside the engineer's
// own margin notes, lets the user pick which comments to include, and posts the review via `gh`.

import { el } from "./dom.js";
import { laneLabel } from "./hub.js";

export function prReviewDialog(dialogEl, { notes, onSubmitted, onError }) {
  let pipeline = null;
  let included = new Set();
  let verdict = "comment";
  let summary = "";
  let submitting = false;

  function open(nextPipeline) {
    pipeline = nextPipeline;
    included = new Set((pipeline.findings ?? []).map((f) => f.id));
    for (const note of notes()) {
      if (note.target === "lines" && note.intent !== "ask") included.add(note.id);
    }
    verdict = pipeline.findings?.length ? "requestChanges" : "approve";
    summary = "";
    submitting = false;
    render();
    if (!dialogEl.open) {
      dialogEl.showModal();
      dialogEl.focus();
    }
  }

  async function submit() {
    if (submitting || !pipeline) return;
    submitting = true;
    render();
    const comments = [];
    for (const finding of pipeline.findings ?? []) {
      if (!included.has(finding.id) || !finding.path || !finding.line) continue;
      let body = finding.text;
      if (finding.fix) body += `\n\nSuggested fix: ${finding.fix}`;
      if (finding.memory) body += `\n\n_(Team memory: \`${finding.memory}\`)_`;
      comments.push({ path: finding.path, line: finding.line, body });
    }
    for (const note of notes()) {
      if (!included.has(note.id) || note.target !== "lines" || !note.lines?.end) continue;
      comments.push({ path: note.path, line: note.lines.end, body: note.body });
    }
    try {
      const response = await fetch("api/pr/submit-review", {
        method: "POST",
        body: JSON.stringify({
          pipeline: pipeline.id,
          number: pipeline.number,
          verdict,
          body: summary.trim(),
          comments,
        }),
      });
      if (!response.ok) throw new Error(await response.text());
      dialogEl.close();
      onSubmitted(`Submitted review to PR #${pipeline.number} (${comments.length} inline comments).`);
    } catch (error) {
      onError(String(error.message ?? error));
    } finally {
      submitting = false;
      if (dialogEl.open) render();
    }
  }

  function render() {
    if (!pipeline) return;
    const panel = el("div", "settings-panel pr-review-panel");
    const header = el("header", "settings-header");
    const title = el("h2", "settings-title", `PR #${pipeline.number} Review · ${laneLabel(pipeline.lane)} Lane`);
    title.id = "pr-review-title";
    const close = el("button", "icon-button", "×");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => dialogEl.close());
    header.append(title, close);

    const body = el("div", "settings-body pr-review-body");
    body.append(el("p", "hub-insp-text", pipeline.title));

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
        const row = el("label", "proposal-item");
        const cb = el("input");
        cb.type = "checkbox";
        cb.checked = included.has(f.id);
        cb.disabled = !f.path || !f.line;
        cb.addEventListener("change", () => {
          if (cb.checked) included.add(f.id);
          else included.delete(f.id);
        });
        const where = f.path ? `${f.path}${f.line ? `:${f.line}` : ""}` : "(general)";
        const info = el("div", "proposal-text");
        info.append(
          el("div", "proposal-title", `${f.id} · ${where}${f.memory ? ` · ${f.memory}` : ""}`),
          el("div", "hub-insp-hint", f.text),
        );
        row.append(cb, info);
        body.append(row);
      }
    }

    const lineNotes = notes().filter((n) => n.target === "lines" && n.intent !== "ask");
    if (lineNotes.length > 0) {
      body.append(el("div", "hub-subhead", `Your Margin Notes (${lineNotes.length})`));
      for (const n of lineNotes) {
        const row = el("label", "proposal-item");
        const cb = el("input");
        cb.type = "checkbox";
        cb.checked = included.has(n.id);
        cb.addEventListener("change", () => {
          if (cb.checked) included.add(n.id);
          else included.delete(n.id);
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
    sumArea.placeholder = "Optional overall summary posted with the review…";
    sumArea.addEventListener("input", () => {
      summary = sumArea.value;
    });
    sumLabel.append(sumArea);
    body.append(sumLabel);

    const footer = el("footer", "settings-footer");
    const cancel = el("button", "button", "Cancel");
    cancel.type = "button";
    cancel.addEventListener("click", () => dialogEl.close());
    const postBtn = el(
      "button",
      "button primary",
      submitting ? "Posting…" : `Post Review to PR #${pipeline.number}`,
    );
    postBtn.type = "button";
    postBtn.disabled = submitting;
    postBtn.addEventListener("click", () => void submit());
    footer.append(cancel, postBtn);

    panel.append(header, body, footer);
    dialogEl.replaceChildren(panel);
  }

  dialogEl.addEventListener("mousedown", (event) => {
    if (event.target === dialogEl) dialogEl.close();
  });

  return { open };
}
