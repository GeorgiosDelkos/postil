import assert from "node:assert/strict";
import test from "node:test";

import {
  agentSaid,
  fileStats,
  groupNotes,
  noteHistory,
  noteStatus,
  planProgress,
  runStatus,
  reviewOf,
  runOf,
  threadOf,
} from "./status.js";

const open = (id, path = "a.rs") => ({ id, path, state: { kind: "open" } });
const sent = (id, run, path = "a.rs") => ({ id, path, state: { kind: "sent", run } });
const resolved = (id, run) => ({ id, path: "a.rs", state: { kind: "resolved", run } });
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

test("a thread is its run of notes and the follow-ups after it, in order", () => {
  const follow = (id, of, status = "ended") => ({ ...run(id, status), followUp: { of, text: "more" } });
  const views = [
    follow("R5", "R3"),
    run("R1", "ended"),
    follow("R3", "R1"),
    { ...run("R4", "ended"), review: { of: "R3", reviewer: 1, reviewers: 1 } },
    run("R2", "ended"),
    follow("R7", "R6"),
  ];
  const ids = (id) => threadOf(id, views).map((view) => view.id);
  assert.deepEqual(ids("R1"), ["R1", "R3", "R5"]);
  assert.deepEqual(ids("R5"), ["R1", "R3", "R5"], "from any run of it");
  assert.deepEqual(ids("R2"), ["R2"]);
  assert.deepEqual(ids("R7"), ["R7"], "its earlier run not read yet");
  assert.deepEqual(ids("R9"), []);

  const live = follow("R3", "R1", "running");
  live.notes = ["N1"];
  assert.equal(runOf(sent("N1", "R1"), new Map(), live), live, "a follow-up under way shows on its notes");
  assert.equal(runOf(sent("N2", "R1"), new Map(), live), null);
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
  assert.equal(status(resolved("N1", "R1"), run("R1", "failed")), "Resolved", "the engineer's word wins");
  assert.equal(status(resolved("N1"), null), "Resolved");
  assert.equal(runStatus(resolved("N1", "R1"), run("R1", "failed")).label, "Failed in R1", "its run's own status");
  assert.equal(runOf(resolved("N1", "R1"), new Map([["R1", run("R1", "ended")]]), null)?.id, "R1");
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

test("notes are grouped as in the run, open, done newest run first, and resolved", () => {
  const notes = [sent("N1", "R1"), open("N2"), sent("N3", "R3"), sent("N4", "R2"), open("N5"), resolved("N6", "R3")];
  const kinds = { N1: "addressed", N2: "open", N3: "working", N4: "failed", N5: "open", N6: "resolved" };
  const groups = groupNotes(notes, (note) => ({ kind: kinds[note.id] }));
  const ids = (list) => list.map((note) => note.id);
  assert.deepEqual(ids(groups.inRun), ["N3"]);
  assert.deepEqual(ids(groups.open), ["N2", "N5"]);
  assert.deepEqual(ids(groups.done), ["N4", "N1"]);
  assert.deepEqual(ids(groups.resolved), ["N6"]);
});

test("a run's latest review: its stages, findings, and the judge's verdict on each", () => {
  const stage = (id, reviewer, reviewers, status, entries) => ({
    id, status, notes: ["N1"], entries, review: { of: "R1", reviewer, reviewers },
  });
  const found = (...findings) => ({ kind: "findings", findings, problem: null });
  const finding = (id, text) => ({ id, note: "N1", path: "a.rs", line: 1, text, fix: null });
  const old = stage("R2", 1, 1, "ended", [found(finding("F1", "an old review"))]);
  const proposer = stage("R4", 1, 2, "ended", [found(finding("F1", "too short"), finding("F2", "no test"))]);
  const judging = stage("R5", 2, 2, "running", [said("F1: accepted - said more")]);
  const other = { id: "R3", status: "ended", notes: [], entries: [], review: null };

  assert.equal(reviewOf("R1", [other]), null, "never reviewed");
  const live = reviewOf("R1", [old, other, judging, proposer]);
  assert.deepEqual(live.stages.map((view) => view.id), ["R4", "R5"], "the latest chain only");
  assert.deepEqual(
    live.findings.map((item) => [item.id, item.from, item.verdict.word]),
    [["F1", "R4", "accepted"], ["F2", "R4", "judging"]],
  );
  assert.equal(live.findings[0].verdict.text, "accepted - said more");

  const ended = reviewOf("R1", [proposer, { ...judging, status: "ended", entries: [said("F1: Rejected - fine as is\nF2: added one")] }]);
  assert.deepEqual(ended.findings.map((item) => item.verdict.word), ["rejected", "answered"]);
  const silent = reviewOf("R1", [proposer, { ...judging, status: "ended", entries: [] }]);
  assert.equal(silent.findings[0].verdict.word, "no word");
  assert.equal(reviewOf("R1", [proposer]).findings[0].verdict.word, "waiting", "the judge has not started");
  const failed = { ...proposer, status: "failed" };
  assert.deepEqual(
    reviewOf("R1", [failed]).findings[0].verdict,
    { word: "not judged", text: "the review stopped after R4" },
    "a stage that failed ends the review before its judge",
  );
  assert.equal(reviewOf("R1", [old]).findings[0].verdict.word, "not judged", "a single reviewer");

  const unreadable = stage("R6", 1, 1, "ended", [{ kind: "findings", findings: [], problem: "no block" }]);
  assert.deepEqual(reviewOf("R1", [unreadable]).problems, ["R6: no block"]);
});
