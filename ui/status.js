// What each note's card and detail show of the run it went to: its status, what the agent
// said about it, and the steps of the run that concern it. Plain functions of a note and a run
// view (`GET api/run`, src/run.rs `RunView`), kept apart from the document so `node --test`
// runs them.

import { diffStats, foldRun, isLive, isOpen, isResolved, sentIn, toolRow } from "./run.js";

// The run a note's status comes from, or null: the run it was marked sent in, else the live
// run when that one carries it (notes are marked sent only once the agent takes the turn).
// `runs` maps a run id to its view; `live` is the latest run of this window, or null.
export function runOf(note, runs, live) {
  const id = sentIn(note);
  if (id !== null) return live?.id === id ? live : (runs.get(id) ?? null);
  // A reviewer's run carries the notes it reviews, but they were not sent in it.
  return isLive(live) && !live.review && live.notes.includes(note.id) ? live : null;
}

// A note's status: `kind` names its icon and group, `label` is its words, `tone` its color.
// A note sent in a run not read yet is "Sent in R2" until it is.
export function noteStatus(note, run) {
  // The engineer's word ends the loop, whatever the run says.
  if (isResolved(note)) return { kind: "resolved", label: "Resolved", tone: "done" };
  return runStatus(note, run);
}

// A note's status from its run alone, as `noteStatus` gives it for a note not resolved: a
// resolved note's dialog still says how its run went.
export function runStatus(note, run) {
  const id = sentIn(note);
  if (run === null) {
    return id === null
      ? { kind: "open", label: "Open", tone: "idle" }
      : { kind: "sent", label: `Sent in ${id}`, tone: "idle" };
  }
  // A note the agent never took (a refused turn) is open again, though its run is known.
  if (isOpen(note) && !isLive(run)) return { kind: "open", label: "Open", tone: "idle" };
  switch (run.status) {
    case "starting":
      return { kind: "starting", label: "Starting", tone: "busy" };
    case "running":
      return { kind: "working", label: "Working", tone: "busy" };
    case "cancelling":
      return { kind: "working", label: "Stopping", tone: "busy" };
    case "waiting":
      return { kind: "needs", label: "Needs you", tone: "attention" };
    case "ended":
      if (endReason(run) !== "end_turn") {
        return { kind: "stopped", label: `Stopped in ${run.id}`, tone: "idle" };
      }
      return agentSaid(run, note.id) === null
        ? { kind: "silent", label: `No word in ${run.id}`, tone: "idle" }
        : { kind: "addressed", label: `Addressed in ${run.id}`, tone: "done" };
    case "failed":
      return { kind: "failed", label: `Failed in ${run.id}`, tone: "failed" };
    default:
      return { kind: "stopped", label: `Unfinished in ${run.id}`, tone: "idle" };
  }
}

function endReason(run) {
  return run.entries.findLast((entry) => entry.kind === "ended")?.reason ?? null;
}

// What the agent said about note `id`: the last line of its messages that starts with the id,
// as the prompt asks it to end ("list each note id with one line on what you did", src/notes.rs
// `prompt`). Agents write that list in Markdown: `N3: …`, `- N3 — …`, `**N3**: …`, `1. [N3] …`,
// and often put the note's place in brackets after the id, as Claude did on 2026-11-23:
// "- **N1** (`src/acp.rs:36-37`): No change…". Null when no line does.
export function agentSaid(run, id) {
  const line = new RegExp(
    String.raw`^\s*(?:[-*+]|\d+[.)])?\s*(?:\*\*|__|\x60)?\[?${id}\]?(?:\*\*|__|\x60)?` +
      String.raw`(?:\s*\([^)]*\))?\s*(?::\*\*|:__|[:.)\-–—])\s*(?:\*\*|__)?\s*(\S.*)$`,
  );
  let said = null;
  for (const item of foldRun(run.entries).items) {
    if (item.type !== "message") continue;
    for (const text of item.text.split("\n")) {
      const match = line.exec(text);
      if (match) said = match[1].trim();
    }
  }
  return said;
}

// The run's plan as steps done of steps, or null when it has none.
export function planProgress(run) {
  const { plan } = foldRun(run.entries);
  if (!plan?.length) return null;
  return { done: plan.filter((step) => step.status === "completed").length, total: plan.length };
}

// The steps of `run` that concern `note`, in order: its tool calls on the note's file, and the
// plan steps that name the note's id. Each is `{ kind: "tool", verb, target, state, running }`
// (run.js `toolRow`) or `{ kind: "step", text, status }`.
export function noteHistory(note, run) {
  const { plan, items } = foldRun(run.entries);
  const live = isLive(run);
  const steps = [];
  for (const item of items) {
    if (item.type === "tool" && touches(item, note.path)) {
      steps.push({ kind: "tool", ...toolRow(item, live) });
    }
  }
  const named = new RegExp(String.raw`\b${note.id}\b`);
  for (const step of plan ?? []) {
    if (named.test(step.content)) steps.push({ kind: "step", text: step.content, status: step.status });
  }
  return steps;
}

function touches(tool, path) {
  return (
    tool.places.some((place) => place.path === path) ||
    tool.output.some((part) => part.type === "diff" && part.path === path)
  );
}

// Lines added and removed on the note's file over the whole run, or null when none.
export function fileStats(note, run) {
  return diffStats(fileDiffs(note, run));
}

// The diffs the run's finished tool calls made to the note's file, in order.
export function fileDiffs(note, run) {
  return foldRun(run.entries)
    .items.filter((item) => item.type === "tool" && item.status === "completed")
    .flatMap((tool) => tool.output.filter((part) => part.type === "diff" && part.path === note.path));
}

// The tray's groups, each in the tray's order: the notes of the live run, the open ones, the
// ones whose run is over, newest run first, and the resolved ones last. `statusOf(note)`
// gives a note's status.
export function groupNotes(notes, statusOf) {
  const inRun = [];
  const open = [];
  const done = [];
  const resolved = [];
  for (const note of notes) {
    const { kind } = statusOf(note);
    if (["starting", "working", "needs"].includes(kind)) inRun.push(note);
    else if (kind === "open") open.push(note);
    else if (kind === "resolved") resolved.push(note);
    else done.push(note);
  }
  const number = (note) => Number(sentIn(note)?.slice(1) ?? 0);
  done.sort((a, b) => number(b) - number(a));
  return { inRun, open, done, resolved };
}

// The latest review of run `id` among `views` (every run read so far; src/review.rs): its
// stages, reviewer 1 first, and each finding with the judge's verdict. Null when the run was
// never reviewed. A run reviewed twice shows its second review: a chain starts at reviewer 1.
//
// A verdict is `{ word, text }`: "accepted" or "rejected" with the judge's line about it
// (`agentSaid`, the judge answers "F1: accepted - ..."), "answered" for a line that says
// neither, "judging" while the judge runs, "waiting" before it starts, "no word" when it
// ended without a line, and "not judged" when a single reviewer only proposed or the review
// stopped before its judge (a stage failed or was stopped).
export function reviewOf(id, views) {
  const stages = views
    .filter((view) => view?.review?.of === id)
    .sort((a, b) => Number(a.id.slice(1)) - Number(b.id.slice(1)));
  const start = stages.findLastIndex((view) => view.review.reviewer === 1);
  if (start < 0) return null;
  const chain = stages.slice(start);
  const reviewers = chain[0].review.reviewers;
  // A stage that did not end well, or a proposer whose findings say why it stopped the
  // review, ends the chain before its judge.
  const stopped = chain.some(
    (view) =>
      (!isLive(view) && view.status !== "ended") ||
      view.entries.some((entry) => entry.kind === "findings" && /stops here/.test(entry.problem ?? "")) ||
      view.entries.some((entry) => entry.kind === "ended" && entry.reason !== "end_turn"),
  );
  const judge = chain.find((view) => reviewers > 1 && view.review.reviewer === reviewers) ?? null;
  const verdict = (finding) => {
    if (reviewers === 1) return { word: "not judged", text: "" };
    if (!judge) {
      const last = chain.at(-1);
      // Between stages the next reviewer is about to start; a stopped chain has no judge.
      return !stopped && (isLive(last) || chain.length < reviewers)
        ? { word: "waiting", text: "" }
        : { word: "not judged", text: `the review stopped after ${last.id}` };
    }
    const said = agentSaid(judge, finding.id);
    if (said !== null) {
      const word = /^(accepted|rejected)\b/i.exec(said)?.[1]?.toLowerCase() ?? "answered";
      return { word, text: said };
    }
    return { word: isLive(judge) ? "judging" : "no word", text: "" };
  };
  const findings = chain.flatMap((view) =>
    view.entries
      .filter((entry) => entry.kind === "findings")
      .flatMap((entry) => entry.findings.map((finding) => ({ ...finding, from: view.id, verdict: verdict(finding) }))),
  );
  const problems = chain.flatMap((view) =>
    view.entries.filter((entry) => entry.kind === "findings" && entry.problem).map((entry) => `${view.id}: ${entry.problem}`),
  );
  return { stages: chain, reviewers, findings, problems };
}
