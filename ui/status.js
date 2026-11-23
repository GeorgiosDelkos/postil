// What each note's card and detail show of the run it went to: its status, what the agent
// said about it, and the steps of the run that concern it. Plain functions of a note and a run
// view (`GET api/run`, src/run.rs `RunView`), kept apart from the document so `node --test`
// runs them.

import { diffStats, foldRun, isLive, isOpen, sentIn, toolRow } from "./run.js";

// The run a note's status comes from, or null: the run it was marked sent in, else the live
// run when that one carries it (notes are marked sent only once the agent takes the turn).
// `runs` maps a run id to its view; `live` is the latest run of this window, or null.
export function runOf(note, runs, live) {
  const id = sentIn(note);
  if (id !== null) return live?.id === id ? live : (runs.get(id) ?? null);
  return isLive(live) && live.notes.includes(note.id) ? live : null;
}

// A note's status: `kind` names its icon and group, `label` is its words, `tone` its color.
// A note sent in a run not read yet is "Sent in R2" until it is.
export function noteStatus(note, run) {
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
// `prompt`). Agents write that list in Markdown: `N3: …`, `- N3 — …`, `**N3**: …`, `1. [N3] …`.
// Null when no line does.
export function agentSaid(run, id) {
  const line = new RegExp(
    String.raw`^\s*(?:[-*+]|\d+[.)])?\s*(?:\*\*|__|\x60)?\[?${id}\]?(?:\*\*|__|\x60)?` +
      String.raw`\s*(?::\*\*|:__|[:.)\-–—])\s*(?:\*\*|__)?\s*(\S.*)$`,
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

// The latest tool call of `run` on the note's file, for the card of a note being worked on.
export function latestOnFile(note, run) {
  const tool = foldRun(run.entries).items.findLast(
    (item) => item.type === "tool" && touches(item, note.path),
  );
  return tool ? toolRow(tool, isLive(run)) : null;
}

function touches(tool, path) {
  return (
    tool.places.some((place) => place.path === path) ||
    tool.output.some((part) => part.type === "diff" && part.path === path)
  );
}

// Lines added and removed on the note's file over the whole run, or null when none.
export function fileStats(note, run) {
  const parts = foldRun(run.entries)
    .items.filter((item) => item.type === "tool" && item.status === "completed")
    .flatMap((tool) => tool.output.filter((part) => part.type === "diff" && part.path === note.path));
  return diffStats(parts);
}

// The tray's groups, each in the tray's order: the notes of the live run, the open ones, and
// the ones whose run is over, newest run first. `statusOf(note)` gives a note's status.
export function groupNotes(notes, statusOf) {
  const inRun = [];
  const open = [];
  const done = [];
  for (const note of notes) {
    const { kind } = statusOf(note);
    if (["starting", "working", "needs"].includes(kind)) inRun.push(note);
    else if (kind === "open") open.push(note);
    else done.push(note);
  }
  const number = (note) => Number(sentIn(note)?.slice(1) ?? 0);
  done.sort((a, b) => number(b) - number(a));
  return { inRun, open, done };
}
