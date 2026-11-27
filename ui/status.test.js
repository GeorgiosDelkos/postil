import assert from "node:assert/strict";
import test from "node:test";

import {
  agentSaid,
  changedFiles,
  fileStats,
  freshAgent,
  replyShown,
  reportNotice,
  withoutReport,
  groupNotes,
  noteDialogKey,
  noteHistory,
  noteStatus,
  planProgress,
  runStatus,
  replyText,
  reviewKey,
  reviewOf,
  latestOfThreads,
  runOf,
  threadKey,
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
  assert.equal(runOf(sent("N1", "R1"), runs, [r2]), r1);
  assert.equal(runOf(sent("N1", "R2"), runs, [r2]), r2, "the live view wins over the cache");
  assert.equal(runOf(open("N2"), runs, [r2]), r2, "not marked sent yet");
  assert.equal(runOf(open("N3"), runs, [r2]), null, "not in the run");
  assert.equal(runOf(open("N2"), runs, [run("R2", "ended", [], ["N2"])]), null, "a refused turn");
  assert.equal(runOf(sent("N1", "R9"), runs, [r2]), null, "not read yet");
  // Threads run side by side: a note of an older run under way shows that run, not the latest.
  const r3 = run("R3", "running", [], ["N3"]);
  assert.equal(runOf(open("N2"), runs, [r2, r3]), r2);
});

test("a thread is its run of notes and the follow-ups after it, in order", () => {
  const follow = (id, of, status = "ended") => ({
    ...run(id, status),
    followUp: { of, text: "more" },
  });
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
  const latest = latestOfThreads(views);
  assert.deepEqual(
    Object.fromEntries([...latest].map(([root, view]) => [root, view.id])),
    { R1: "R5", R2: "R2", R7: "R7" },
    "each thread's latest run, reviews left out",
  );

  const live = follow("R3", "R1", "running");
  live.notes = ["N1"];
  assert.equal(
    runOf(sent("N1", "R1"), new Map(), [live]),
    live,
    "a follow-up under way shows on its notes",
  );
  assert.equal(runOf(sent("N2", "R1"), new Map(), [live]), null);
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
  assert.equal(
    status(resolved("N1", "R1"), run("R1", "failed")),
    "Resolved",
    "the engineer's word wins",
  );
  assert.equal(status(resolved("N1"), null), "Resolved");
  assert.equal(
    runStatus(resolved("N1", "R1"), run("R1", "failed")).label,
    "Failed in R1",
    "its run's own status",
  );
  assert.equal(runOf(resolved("N1", "R1"), new Map([["R1", run("R1", "ended")]]), [])?.id, "R1");
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
    {
      kind: "plan",
      steps: [
        { content: "Fix N3 in divide", status: "completed" },
        { content: "Fix N30", status: "pending" },
      ],
    },
  ];
  const r = run("R1", "ended", entries);
  assert.deepEqual(
    noteHistory(note, r).map((step) =>
      step.kind === "tool" ? `${step.verb} ${step.state}` : step.text,
    ),
    ["read done", "edit +2 -1", "Fix N3 in divide"],
  );
  assert.deepEqual(fileStats(note, r), { added: 2, removed: 1 });
  assert.equal(fileStats(sent("N4", "R1", "src/none.rs"), r), null);

  // A call that names the file only in its diff concerns it too.
  const moved = {
    ...tool("t4", "other", "x"),
    places: [],
    output: [diff("src/math.rs", null, "z\n")],
  };
  assert.equal(noteHistory(note, run("R1", "ended", [moved])).length, 1);
});

test("a folder note's history is the run's tool calls on any file below its folder", () => {
  const note = { ...sent("N1", "R1", "src/acp"), target: "folder" };
  const entries = [
    tool("t1", "read", "src/acp/wire.rs"),
    tool("t2", "edit", "src/acp.rs", [diff("src/acp.rs", "a\n", "b\n")]),
    tool("t3", "edit", "src/acp/deep/x.rs", [diff("src/acp/deep/x.rs", "a\n", "a\nb\n")]),
  ];
  const r = run("R1", "ended", entries);
  assert.deepEqual(
    noteHistory(note, r).map((step) => `${step.verb} ${step.target}`),
    ["read src/acp/wire.rs", "edit src/acp/deep/x.rs"],
  );
  assert.deepEqual(fileStats(note, r), { added: 1, removed: 0 });
});

test("a folder note's changed files are listed once each, in the order first changed", () => {
  const note = { ...sent("N1", "R1", "src"), target: "folder" };
  const entries = [
    tool("t1", "edit", "src/b.rs", [diff("src/b.rs", "a\n", "a\nb\n")]),
    tool("t2", "edit", "src/a.rs", [diff("src/a.rs", "a\nb\n", "c\n")]),
    tool("t3", "edit", "src/b.rs", [diff("src/b.rs", "a\nb\n", "a\nb\nc\n")]),
    tool("t4", "edit", "lib/x.rs", [diff("lib/x.rs", null, "x\n")]),
    // An edit that did not finish changed nothing yet.
    tool("t5", "edit", "src/c.rs", [diff("src/c.rs", null, "c\n")], "in_progress"),
  ];
  assert.deepEqual(changedFiles(note, run("R1", "ended", entries)), [
    { path: "src/b.rs", added: 2, removed: 0 },
    { path: "src/a.rs", added: 1, removed: 2 },
  ]);
});

test("notes are grouped as in the run, open, done newest run first, and resolved", () => {
  const notes = [
    sent("N1", "R1"),
    open("N2"),
    sent("N3", "R3"),
    sent("N4", "R2"),
    open("N5"),
    resolved("N6", "R3"),
  ];
  const kinds = {
    N1: "addressed",
    N2: "open",
    N3: "working",
    N4: "failed",
    N5: "open",
    N6: "resolved",
  };
  const groups = groupNotes(notes, (note) => ({ kind: kinds[note.id] }));
  const ids = (list) => list.map((note) => note.id);
  assert.deepEqual(ids(groups.inRun), ["N3"]);
  assert.deepEqual(ids(groups.open), ["N2", "N5"]);
  assert.deepEqual(ids(groups.done), ["N4", "N1"]);
  assert.deepEqual(ids(groups.resolved), ["N6"]);
});

test("a run's latest review: its stages, findings, and the judge's verdict on each", () => {
  const stage = (id, reviewer, reviewers, status, entries) => ({
    id,
    status,
    notes: ["N1"],
    entries,
    review: { of: "R1", reviewer, reviewers },
  });
  const found = (...findings) => ({ kind: "findings", findings, problem: null });
  const finding = (id, text) => ({ id, note: "N1", path: "a.rs", line: 1, text, fix: null });
  const old = stage("R2", 1, 1, "ended", [found(finding("F1", "an old review"))]);
  const proposer = stage("R4", 1, 2, "ended", [
    found(finding("F1", "too short"), finding("F2", "no test")),
  ]);
  const judging = stage("R5", 2, 2, "running", [said("F1: accepted - said more")]);
  const other = { id: "R3", status: "ended", notes: [], entries: [], review: null };

  assert.equal(reviewOf("R1", [other]), null, "never reviewed");
  const live = reviewOf("R1", [old, other, judging, proposer]);
  assert.deepEqual(
    live.stages.map((view) => view.id),
    ["R4", "R5"],
    "the latest chain only",
  );
  assert.deepEqual(
    live.findings.map((item) => [item.id, item.from, item.verdict.word]),
    [
      ["F1", "R4", "accepted"],
      ["F2", "R4", "judging"],
    ],
  );
  assert.equal(live.findings[0].verdict.text, "accepted - said more");

  const ended = reviewOf("R1", [
    proposer,
    { ...judging, status: "ended", entries: [said("F1: Rejected - fine as is\nF2: added one")] },
  ]);
  assert.deepEqual(
    ended.findings.map((item) => item.verdict.word),
    ["rejected", "answered"],
  );
  const silent = reviewOf("R1", [proposer, { ...judging, status: "ended", entries: [] }]);
  assert.equal(silent.findings[0].verdict.word, "no word");
  assert.equal(
    reviewOf("R1", [proposer]).findings[0].verdict.word,
    "waiting",
    "the judge has not started",
  );
  const failed = { ...proposer, status: "failed" };
  assert.deepEqual(
    reviewOf("R1", [failed]).findings[0].verdict,
    { word: "not judged", text: "the review stopped after R4" },
    "a stage that failed ends the review before its judge",
  );
  assert.equal(reviewOf("R1", [old]).findings[0].verdict.word, "not judged", "a single reviewer");

  const unreadable = stage("R6", 1, 1, "ended", [
    { kind: "findings", findings: [], problem: "no block" },
  ]);
  assert.deepEqual(reviewOf("R1", [unreadable]).problems, ["R6: no block"]);
});

// `total` is set as the Rust process sets it (src/run/log.rs, `RunView`): a key that read it
// would change on every entry.
const streaming = (id, status, entries, extra = {}) => ({
  ...run(id, status, entries, ["N1"]),
  agent: "Claude",
  total: entries.length,
  ...extra,
});

test("an ask is answered when its run ends well, with no line naming it", () => {
  const ask = { ...sent("N1", "R1"), intent: "ask" };
  const status = (r) => runStatus(ask, r);
  const answer = [said("It returns early because the list is empty."), ended("end_turn")];
  assert.deepEqual(status(run("R1", "ended", answer)), {
    kind: "answered",
    label: "Answered in R1",
    tone: "done",
  });
  assert.equal(status(run("R1", "ended", [ended("cancelled")])).label, "Stopped in R1");
  assert.equal(status(run("R1", "failed")).label, "Failed in R1");
  assert.equal(status(run("R1", "running")).label, "Working");
  assert.equal(
    noteStatus({ ...ask, state: { kind: "resolved", run: "R1" } }, run("R1", "ended", answer))
      .label,
    "Resolved",
  );
  const groups = groupNotes([ask], () => status(run("R1", "ended", answer)));
  assert.deepEqual(groups.done, [ask], "an answered ask is done");
});

test("an ask's dialog is drawn again as its answer streams, and on an undone edit", () => {
  const ask = { ...sent("N1", "R1"), intent: "ask" };
  const shown = [false, "Claude", [], true];
  const key = (entries, problem = null) =>
    noteDialogKey(ask, { ...streaming("R1", "running", entries), problem }, null, shown);
  const start = [said("It returns")];
  const drawn = key(start);
  assert.notEqual(key([...start, said(" early.")]), drawn, "the answer it shows whole");
  assert.notEqual(
    key(start, "the agent changed files, though it may only read: its changes were undone"),
    drawn,
    "the notice above the answer",
  );
  // A change note's dialog shows only the agent's line about it, not the reply.
  const change = sent("N1", "R1");
  const changeKey = (entries) =>
    noteDialogKey(change, streaming("R1", "running", entries), null, shown);
  assert.equal(changeKey([...start, said(" early.")]), changeKey(start));
});

test("the note dialog keeps its key while its run streams what the dialog does not show", () => {
  const note = sent("N1", "R1");
  const shown = [true, "Claude", ["Codex"], true];
  const key = (entries, status = "running", review = null) =>
    noteDialogKey(note, streaming("R1", status, entries), review, shown);
  const start = [said("Looking at it")];
  const drawn = key(start);
  assert.equal(key([...start, said(" and more")]), drawn, "a token of the reply");
  assert.equal(key([...start, { kind: "thought", text: "hm" }]), drawn, "a thought");
  assert.equal(key([...start, tool("t1", "read", "b.rs")]), drawn, "a tool call on another file");
  assert.notEqual(key([...start, tool("t1", "edit", "a.rs")]), drawn, "its file's history grows");
  assert.notEqual(key([...start, said("\nN1: fixed")]), drawn, "the agent's line about it");
  assert.notEqual(key(start, "waiting"), drawn, "the run's status");

  const stage = (entries) =>
    streaming("R2", "running", entries, { review: { of: "R1", reviewer: 1, reviewers: 1 } });
  const reviewed = (entries) => key(start, "running", reviewOf("R1", [stage(entries)]));
  assert.equal(
    reviewed([said("reading")]),
    reviewed([said("reading"), said(" on")]),
    "a reviewer's token",
  );
});

test("a judge writing its line changes the verdict's word only for the run dialog", () => {
  const stage = (id, reviewer, entries) =>
    streaming(id, "running", entries, { review: { of: "R1", reviewer, reviewers: 2 } });
  const finding = { id: "F1", note: "N1", path: "a.rs", line: 1, text: "too short", fix: null };
  const proposer = {
    ...stage("R2", 1, [{ kind: "findings", findings: [finding], problem: null }]),
    status: "ended",
  };
  const review = (line) => reviewOf("R1", [proposer, stage("R3", 2, [said(line)])]);
  const writing = review("F1: accepted - bec");
  const more = review("F1: accepted - because it");
  assert.deepEqual(reviewKey(more), reviewKey(writing), "the run dialog shows only the word");
  assert.notDeepEqual(reviewKey(review("F1: rejected - no")), reviewKey(writing), "a new word");
  const noteKey = (judged) =>
    noteDialogKey(sent("N1", "R1"), streaming("R1", "ended", []), judged, []);
  assert.notEqual(noteKey(more), noteKey(writing), "the note dialog shows the judge's line");
});

test("the run dialog keeps its key while the agent streams its reply and thoughts", () => {
  const thought = (text) => ({ kind: "thought", text });
  const edit = tool("t1", "edit", "a.rs", [], "in_progress");
  const start = [thought("Reading"), edit, said("Done")];
  const key = (entries, status = "running") => threadKey([streaming("R1", status, entries)]);
  const drawn = key(start);
  assert.equal(key([...start, said(" with it")]), drawn, "a token of the reply");
  assert.equal(key([thought("Reading a.rs"), edit, said("Done")]), drawn, "a token of a thought");
  assert.notEqual(
    key([...start, { ...edit, status: "completed" }]),
    drawn,
    "a step's card changes",
  );
  assert.notEqual(key([...start, thought("Then")]), drawn, "a new step");
  assert.notEqual(key(start, "cancelling"), drawn, "the run's status");
  assert.notEqual(
    key([thought("Reading"), edit]),
    key([thought("Reading"), edit, said("Done")]),
    "the reply shows",
  );
  assert.equal(replyText(streaming("R1", "ended", start)), "Done");
});

test("an undone edit redraws the run dialog with its notice", () => {
  // The server sets the run's problem from the `undone` entry (src/run/log.rs, `apply`), live
  // and after a restart; the dialog shows it under the reply.
  const start = [said("It returns early.")];
  const text = "the agent changed files, though it may only read: its changes were undone";
  const drawn = threadKey([streaming("R1", "running", start)]);
  const undone = threadKey([
    streaming("R1", "running", [...start, { kind: "undone", text }], { problem: text }),
  ]);
  assert.notEqual(undone, drawn);
});

test("a run that started without team memory redraws its dialogs with the notice", () => {
  // The server sets `memoryNotice` as the agent starts (src/run.rs, `Runner::start`), with no
  // entry of its own: only the key shows it changed.
  const notice = {
    memoryNotice: "team memory was not given: hippius-mem is not installed on PATH",
  };
  const start = [said("Done")];
  assert.notEqual(
    threadKey([streaming("R1", "running", start, notice)]),
    threadKey([streaming("R1", "running", start)]),
    "the run dialog",
  );
  const note = sent("N1", "R1");
  const shown = [false, "Claude", [], true];
  assert.notEqual(
    noteDialogKey(note, streaming("R1", "running", start, notice), null, shown),
    noteDialogKey(note, streaming("R1", "running", start), null, shown),
    "the note's dialog",
  );
});

test("an analysis run's dialog redraws for what it was asked and for why nothing was stored", () => {
  // The server sets the problem from the `noAnalysis` entry (src/run/log.rs, `apply`).
  const asked = { analysis: { kind: "analyze" } };
  const ended = threadKey([streaming("R1", "ended", [], asked)]);
  assert.notEqual(ended, threadKey([streaming("R1", "ended", [])]), "the run's kind");
  const empty = "the analysis was empty";
  const nothing = threadKey([
    streaming("R1", "ended", [{ kind: "noAnalysis", problem: empty }], {
      ...asked,
      problem: empty,
    }),
  ]);
  assert.notEqual(nothing, ended);
});

const fenced = (json) => "```json\n" + json + "\n```";
const reportEntry = (report, problem = null) =>
  report ? { kind: "report", report } : { kind: "noReport", problem };

// The block as src/report.rs keeps it, closed in whichever way the agent closed it.
const report = fenced('{"steps": [], "checks": []}');

test("the report block the server read is left out of the reply, and only it", () => {
  assert.equal(withoutReport(`N1: done\n\n${report}`, report), "N1: done", "block last");
  // Spike S2 saw Grok go on working after its report in 3 of 10 runs.
  assert.equal(
    withoutReport(`N1: done\n\n${report}\nThen I ran the linter.`, report),
    "N1: done\n\nThen I ran the linter.",
    "text after the block stays",
  );
  // An example the agent showed before its report stays, whatever its shape.
  const config = fenced('{"port": 80}');
  assert.equal(
    withoutReport(`Use this:\n\n${config}\n\n${report}`, report),
    `Use this:\n\n${config}`,
  );
  assert.equal(withoutReport(`${report}\nAgain:\n${report}`, report), `${report}\nAgain:`);
  // The server reads only a fence that starts a line: the same text repeated inside a line is
  // prose, and stays.
  assert.equal(
    withoutReport(`N1: done\n\n${report}\n\nRepeated inline: ${report} end.`, report),
    `N1: done\n\nRepeated inline: ${report} end.`,
  );
  const unclosed = '```json\n{"checks": []}';
  assert.equal(
    withoutReport(`Done.\n${unclosed}\nThen I ran ruff.`, unclosed),
    "Done.\n\nThen I ran ruff.",
    "a block never closed",
  );
  assert.equal(withoutReport("N1: done", report), "N1: done", "a reply that does not hold it");
});

test("the reply keeps its json block when the run has no report", () => {
  const text = `Here is the config:\n\n${fenced('{"port": 80}')}`;
  const ran = (entries) => streaming("R1", "ended", [said(text), ...entries]);
  assert.equal(replyShown(ran([])), text, "a run from before reports");
  assert.equal(replyShown(ran([reportEntry(null, "no steps and no checks")])), text);
  const block = fenced('{"port": 80}');
  const found = reportEntry({ steps: [], checks: [], cut: 0, block });
  assert.equal(replyShown(ran([found])), "Here is the config:");
});

test("the run dialog says when the report is missing or partly left out", () => {
  const ran = (entries) => streaming("R1", "ended", entries);
  assert.equal(reportNotice(ran([])), null, "a run from before reports");
  assert.equal(
    reportNotice(ran([reportEntry(null, "the reply has no ```json block")])),
    "No report from the agent: the reply has no ```json block",
  );
  assert.equal(reportNotice(ran([reportEntry({ steps: [], checks: [], cut: 0 })])), null);
  assert.match(reportNotice(ran([reportEntry({ steps: [], checks: [], cut: 2 })])), /^2 parts/);
});

test("a report arriving redraws the run dialog, and note lines before it still count", () => {
  const start = [said("N1: renamed it")];
  const key = (entries) => threadKey([streaming("R1", "ended", entries)]);
  const report = reportEntry({ steps: [], checks: [], cut: 0 });
  assert.notEqual(key([...start, report]), key(start), "the report arrives");
  const withBlock = [said(`N1: renamed it\n\n${fenced('{"steps": [{"why": "N1: not this"}]}')}`)];
  assert.equal(agentSaid(streaming("R1", "ended", withBlock), "N1"), "renamed it");
});

test("a command that arrives after its call started redraws the run's Checks", () => {
  // Claude's adapter names the command on an update, after the call (src/run/log.rs).
  const report = reportEntry({
    steps: [],
    checks: [{ command: "cargo test", result: "passed" }],
    cut: 0,
    block: "",
  });
  const call = tool("c1", "execute", "Terminal");
  const key = (entries) => threadKey([streaming("R1", "ended", [call, ...entries, report])]);
  const named = { kind: "toolUpdate", id: "c1", command: "cargo test" };
  assert.notEqual(key([named]), key([]), "the claim now matches the call");
});

test("a follow-up went to a fresh agent only when its prompt told the thread first", () => {
  const turn = (prompt) => ({ prompt, followUp: { of: "R1", text: "and more" } });
  assert.equal(freshAgent(turn("and more")), false, "a log from before the reminder");
  assert.equal(freshAgent(turn("and more\n\nWhen you finish, end your reply…")), false);
  assert.equal(
    freshAgent(turn("You are continuing work another coding agent did…\n\nand more")),
    true,
  );
  assert.equal(freshAgent(turn("and more, and then")), true, "words that only start alike");
});
