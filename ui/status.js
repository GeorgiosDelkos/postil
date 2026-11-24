// What each note's card and detail show of the run it went to: its status, what the agent
// said about it, and the steps of the run that concern it. Plain functions of a note and a run
// view (`GET api/run`, src/run/log.rs `RunView`), kept apart from the document so `node --test`
// runs them.

import { covers } from "./note-place.js";
import {
  diffStats,
  foldRun,
  isLive,
  isOpen,
  isResolved,
  openQuestion,
  sentIn,
  statusLine,
  toolRow,
} from "./run.js";

// The run a note's status comes from, or null: the run it was marked sent in, else the live
// run when that one carries it (notes are marked sent only once the agent takes the turn).
// `runs` maps a run id to its view; `live` is the latest run of this window, or null.
export function runOf(note, runs, live) {
  // A follow-up under way works on its thread's notes, so they show it while it runs.
  if (isLive(live) && live.followUp && live.notes.includes(note.id)) return live;
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

// The steps of `run` that concern `note`, in order: its tool calls on the note's file (any file
// in a folder note's folder, note-place.js `covers`), and the plan steps that name the note's
// id. Each is `{ kind: "tool", verb, target, state, running }` (run.js `toolRow`) or
// `{ kind: "step", text, status }`.
export function noteHistory(note, run) {
  const { plan, items } = foldRun(run.entries);
  const live = isLive(run);
  const steps = [];
  for (const item of items) {
    if (item.type === "tool" && touches(item, note)) {
      steps.push({ kind: "tool", ...toolRow(item, live) });
    }
  }
  const named = new RegExp(String.raw`\b${note.id}\b`);
  for (const step of plan ?? []) {
    if (named.test(step.content))
      steps.push({ kind: "step", text: step.content, status: step.status });
  }
  return steps;
}

function touches(tool, note) {
  return (
    tool.places.some((place) => covers(note, place.path)) ||
    tool.output.some((part) => part.type === "diff" && covers(note, part.path))
  );
}

// Lines added and removed on the note's file, or in its folder, over the whole run, or null
// when none.
export function fileStats(note, run) {
  return diffStats(fileDiffs(note, run));
}

// The diffs the run's finished tool calls made to the note's file, or in its folder, in order.
export function fileDiffs(note, run) {
  return foldRun(run.entries)
    .items.filter((item) => item.type === "tool" && item.status === "completed")
    .flatMap((tool) =>
      tool.output.filter((part) => part.type === "diff" && covers(note, part.path)),
    );
}

// The files the run's finished tool calls changed under a folder note's folder, each once, in
// the order first changed: `{ path, added, removed }`. A folder note's dialog lists these
// rather than the diffs, which would be unlabeled and, for a large folder, unbounded.
export function changedFiles(note, run) {
  const byPath = new Map();
  for (const diff of fileDiffs(note, run)) {
    byPath.set(diff.path, [...(byPath.get(diff.path) ?? []), diff]);
  }
  return [...byPath].map(([path, diffs]) => ({ path, ...diffStats(diffs) }));
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

// The runs of the thread run `id` is in, among `views` (every run read so far): the run of
// notes it started from, then its follow-ups in the order they were sent (src/run.rs,
// `follow_up`). A follow-up whose earlier run is not read yet starts a thread of its own
// until it is.
export function threadOf(id, views) {
  const roots = rootsOf(views);
  const root = roots.get(id);
  if (root === undefined) return [];
  return views
    .filter((view) => !view.review && roots.get(view.id) === root)
    .sort((a, b) => runNumber(a) - runNumber(b));
}

// Each thread's latest run among `views`, by the id of the run it started from. The Run tab
// reads it once per draw, rather than `threadOf` once per card.
export function latestOfThreads(views) {
  const roots = rootsOf(views);
  const latest = new Map();
  for (const view of views) {
    if (view.review) continue;
    const root = roots.get(view.id);
    const known = latest.get(root);
    if (!known || runNumber(view) > runNumber(known)) latest.set(root, view);
  }
  return latest;
}

// The id of the run each of `views` started its thread from, by id. A follow-up names an
// older run, so each walk ends; the count only stops a log edited into a loop.
function rootsOf(views) {
  const byId = new Map(views.map((view) => [view.id, view]));
  const roots = new Map();
  for (const view of views) {
    let at = view;
    for (let seen = 0; at.followUp && byId.has(at.followUp.of) && seen < views.length; seen += 1) {
      at = byId.get(at.followUp.of);
    }
    roots.set(view.id, at.id);
  }
  return roots;
}

const runNumber = (view) => Number(view.id.slice(1));

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
      view.entries.some(
        (entry) => entry.kind === "findings" && /stops here/.test(entry.problem ?? ""),
      ) ||
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
      .flatMap((entry) =>
        entry.findings.map((finding) => ({ ...finding, from: view.id, verdict: verdict(finding) })),
      ),
  );
  const problems = chain.flatMap((view) =>
    view.entries
      .filter((entry) => entry.kind === "findings" && entry.problem)
      .map((entry) => `${view.id}: ${entry.problem}`),
  );
  return { stages: chain, reviewers, findings, problems };
}

// Why a failed run failed, from its log, or null.
export function runFailure(run) {
  return run.entries.findLast((entry) => entry.kind === "failed")?.error ?? null;
}

// What the run and note dialogs show of a review (`reviewOf`), as plain data to compare:
// each stage's status and each finding with its verdict's word. A stage's entries are left
// out, and so is the judge's line (the verdict's text), which grows with every token while
// the judge writes it: a reviewer that streams leaves the dialogs alone until something shown
// changes. The note dialog, which shows the judge's line, adds it for its own findings.
export function reviewKey(review) {
  if (!review) return null;
  return [
    review.stages.map((stage) => [
      stage.id,
      stage.agent,
      stage.review.reviewer,
      stage.status,
      statusLine(stage),
    ]),
    review.findings.map(({ id, note, text, fix, path, line, verdict }) => [
      id,
      note,
      text,
      fix,
      path,
      line,
      verdict.word,
    ]),
    review.problems,
  ];
}

// The agent's reply in a run: its messages, joined.
export function replyText(run) {
  return foldRun(run.entries)
    .items.filter((item) => item.type === "message")
    .map((item) => item.text.trim())
    .join("\n\n");
}

// What the run dialog shows of a thread (`threadOf`), as one string, but for the text the
// agent streams: its replies and thoughts, which the dialog writes in place. The dialog is
// built again only when this changes, so while the agent writes its step cards, Changes and
// Review keep their clicks and its follow-up box keeps the focus.
export function threadKey(thread) {
  return JSON.stringify(thread.map(turnShown));
}

function turnShown(turn) {
  const live = isLive(turn);
  const { plan, items } = foldRun(turn.entries);
  return [
    turn.id,
    turn.agent,
    turn.status,
    statusLine(turn),
    turn.startedAt,
    turn.problem ?? null,
    turn.review ?? null,
    turn.followUp ?? null,
    turn.prompt === turn.followUp?.text,
    plan,
    items.map((item) => itemShown(item, live)),
    // Whether the reply shows at all: its text is written in place.
    replyText(turn) !== "",
    turn.entries.some((entry) => entry.kind === "snapshot"),
  ];
}

// What a step card shows of an item, its streamed text left out. A tool's output is not on
// its card, and can be large.
function itemShown(item, live) {
  switch (item.type) {
    case "message":
    case "thought":
      return [item.type];
    case "tool":
      return [item.id, item.title, toolRow(item, live)];
    default:
      return item;
  }
}

// What the note dialog shows, as one string: the dialog is drawn again only when it changes.
// A run's entries come many a second while the agent writes, and most change nothing the
// dialog shows; drawing it again on each would scroll it, take its focus, drop a click whose
// press and release land on two nodes, and reset the two-click Delete. `review` is the run's
// review (`reviewOf`); `shown` is the rest the dialog shows that the note and run do not hold
// (whether it can be sent, the agent, the reviewers, whether a run is under way).
export function noteDialogKey(note, run, review, shown) {
  if (!run) return JSON.stringify([note, shown]);
  const question = isLive(run) ? openQuestion(foldRun(run.entries).items) : null;
  return JSON.stringify([
    note,
    shown,
    run.id,
    run.agent,
    run.status,
    run.notes,
    run.problem ?? null,
    runFailure(run),
    statusLine(run),
    runStatus(note, run),
    agentSaid(run, note.id),
    question?.request ?? null,
    // A finished tool call's diffs do not change, so their count and size stand for them.
    fileDiffs(note, run).length,
    fileStats(note, run),
    noteHistory(note, run),
    reviewKey(review),
    review?.findings
      .filter((finding) => finding.note === note.id)
      .map((finding) => finding.verdict.text) ?? null,
  ]);
}
