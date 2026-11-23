import assert from "node:assert/strict";
import test from "node:test";

import {
  agentSaid,
  fileStats,
  groupNotes,
  noteHistory,
  noteStatus,
  planProgress,
  runOf,
} from "./status.js";

const open = (id, path = "a.rs") => ({ id, path, state: { kind: "open" } });
const sent = (id, run, path = "a.rs") => ({ id, path, state: { kind: "sent", run } });
const run = (id, status, entries = [], notes = []) => ({ id, status, entries, notes });
const said = (text) => ({ kind: "message", text, message: null });
const ended = (reason) => ({ kind: "ended", reason });
const tool = (id, kind, path, output = [], status = "completed") => ({
  kind: "tool",
  id,
  title: `${kind} ${path}`,
  tool: kind,
  status,
  places: [{ path, line: null }],
  output,
});
const diff = (path, oldText, newText) => ({ type: "diff", path, oldText, newText });

test("a note's run is the one it was sent in, or the live run carrying it", () => {
  const r1 = run("R1", "ended");
  const r2 = run("R2", "running", [], ["N2"]);
  const runs = new Map([["R1", r1]]);
  assert.equal(runOf(sent("N1", "R1"), runs, r2), r1);
  assert.equal(runOf(sent("N1", "R2"), runs, r2), r2, "the live view wins over the cache");
  assert.equal(runOf(open("N2"), runs, r2), r2, "not marked sent yet");
  assert.equal(runOf(open("N3"), runs, r2), null, "not in the run");
  assert.equal(runOf(open("N2"), runs, run("R2", "ended", [], ["N2"])), null, "a refused turn");
  assert.equal(runOf(sent("N1", "R9"), runs, r2), null, "not read yet");
});

test("a note's status follows its run", () => {
  const status = (note, r) => noteStatus(note, r).label;
  assert.equal(status(open("N1"), null), "Open");
  assert.equal(status(sent("N1", "R4"), null), "Sent in R4");
  assert.equal(status(open("N1"), run("R1", "starting")), "Starting");
  assert.equal(status(sent("N1", "R1"), run("R1", "running")), "Working");
  assert.equal(status(sent("N1", "R1"), run("R1", "cancelling")), "Stopping");
  assert.equal(status(sent("N1", "R1"), run("R1", "waiting")), "Needs you");
  assert.equal(status(sent("N1", "R1"), run("R1", "failed")), "Failed in R1");
  assert.equal(status(sent("N1", "R1"), run("R1", "unfinished")), "Unfinished in R1");
  const done = [said("All done.\nN1: fixed it"), ended("end_turn")];
  assert.equal(status(sent("N1", "R1"), run("R1", "ended", done)), "Addressed in R1");
  assert.equal(status(sent("N2", "R1"), run("R1", "ended", done)), "No word in R1");
  const stopped = [said("N1: half done"), ended("cancelled")];
  assert.equal(status(sent("N1", "R1"), run("R1", "ended", stopped)), "Stopped in R1");
  assert.equal(
    status(open("N1"), run("R1", "ended", [ended("refusal")])),
    "Open",
    "a note the agent never took is open again",
  );
});

test("the agent's line about a note is read in the forms agents write", () => {
  const line = (text, id = "N3") => agentSaid(run("R1", "ended", [said(text)]), id);
  assert.equal(line("N3: made it return Option"), "made it return Option");
  assert.equal(line("- N3 — made it return Option"), "made it return Option");
  assert.equal(line("* **N3**: made it return Option"), "made it return Option");
  assert.equal(line("**N3:** made it return Option"), "made it return Option");
  assert.equal(line("1. [N3] - made it return Option"), "made it return Option");
  assert.equal(line("2) `N3`: made it return Option"), "made it return Option");
  // Claude's own line, 2026-11-23: the place in brackets between the id and the colon.
  assert.equal(
    line("- **N1** (`src/acp.rs:36-37`): No change. These two lines declare.", "N1"),
    "No change. These two lines declare.",
  );
  assert.equal(line("N3 (src/math.rs:7) — returns Option now"), "returns Option now");
  assert.equal(line("N30: something else"), null, "N3 is not N30");
  assert.equal(line("See N3: it was hard"), null, "only at the start of a line");
  assert.equal(line("N3:"), null, "an empty line says nothing");
  assert.equal(line("N3: first\n\nSummary:\nN3: last"), "last", "the last line wins");
  const parts = [said("Summary\n- N"), said("3: split across parts")];
  assert.equal(agentSaid(run("R1", "ended", parts), "N3"), "split across parts");
});

test("plan progress counts the completed steps of the latest plan", () => {
  const steps = (...statuses) => statuses.map((status, i) => ({ content: `s${i}`, status }));
  const plan = { kind: "plan", steps: steps("completed", "in_progress", "pending") };
  assert.deepEqual(planProgress(run("R1", "running", [plan])), { done: 1, total: 3 });
  assert.equal(planProgress(run("R1", "running", [])), null);
});

test("a note's history is the run's tool calls on its file and the plan steps naming it", () => {
  const note = sent("N3", "R1", "src/math.rs");
  const entries = [
    tool("t1", "read", "src/math.rs"),
    tool("t2", "read", "src/other.rs"),
    tool("t3", "edit", "src/math.rs", [diff("src/math.rs", "a\nb\n", "a\nc\nd\n")]),
    { kind: "plan", steps: [
      { content: "Fix N3 in divide", status: "completed" },
      { content: "Fix N30", status: "pending" },
    ] },
  ];
  const r = run("R1", "ended", entries);
  assert.deepEqual(
    noteHistory(note, r).map((step) => (step.kind === "tool" ? `${step.verb} ${step.state}` : step.text)),
    ["read done", "edit +2 -1", "Fix N3 in divide"],
  );
  assert.deepEqual(fileStats(note, r), { added: 2, removed: 1 });
  assert.equal(fileStats(sent("N4", "R1", "src/none.rs"), r), null);

  // A call that names the file only in its diff concerns it too.
  const moved = { ...tool("t4", "other", "x"), places: [], output: [diff("src/math.rs", null, "z\n")] };
  assert.equal(noteHistory(note, run("R1", "ended", [moved])).length, 1);
});

test("notes are grouped as in the run, open, and done, newest run first", () => {
  const notes = [sent("N1", "R1"), open("N2"), sent("N3", "R3"), sent("N4", "R2"), open("N5")];
  const kinds = { N1: "addressed", N2: "open", N3: "working", N4: "failed", N5: "open" };
  const groups = groupNotes(notes, (note) => ({ kind: kinds[note.id] }));
  const ids = (list) => list.map((note) => note.id);
  assert.deepEqual(ids(groups.inRun), ["N3"]);
  assert.deepEqual(ids(groups.open), ["N2", "N5"]);
  assert.deepEqual(ids(groups.done), ["N4", "N1"]);
});
