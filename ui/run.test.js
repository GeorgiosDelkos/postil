import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_EDITS,
  diffLines,
  diffStats,
  elapsed,
  foldRun,
  isLive,
  inline,
  keyFor,
  permissionKey,
  markdownBlocks,
  openPermission,
  permissionKeys,
  promptLines,
  sendable,
  sentIn,
  statusLine,
  toolRow,
} from "./run.js";

const note = (id, state, anchor = "found") => ({ id, state, anchor });
const OPEN = { kind: "open" };

test("the send step ticks the open notes whose lines are still there", () => {
  const notes = [
    note("N1", OPEN),
    note("N2", { kind: "sent", run: "R1" }),
    note("N3", OPEN, "lost"),
    note("N4", OPEN),
    note("N5", { kind: "resolved", run: "R2" }),
    note("N6", { kind: "resolved" }),
  ];
  assert.deepEqual(sendable(notes), ["N1", "N4"], "a resolved note is not ticked");
  assert.equal(sentIn(notes[1]), "R1");
  assert.equal(sentIn(notes[0]), null);
  assert.equal(sentIn(notes[4]), "R2", "a resolved note keeps its run");
  assert.equal(sentIn(notes[5]), null, "resolved without being sent");
});

test("message and thought parts join until something else comes between them", () => {
  const { items } = foldRun([
    { kind: "started" },
    { kind: "message", text: "Look" },
    { kind: "message", text: "ing." },
    { kind: "thought", text: "a" },
    { kind: "thought", text: "b" },
    { kind: "message", text: "Done." },
  ]);
  assert.deepEqual(
    items.map(({ type, text }) => ({ type, text })),
    [
      { type: "message", text: "Looking." },
      { type: "thought", text: "ab" },
      { type: "message", text: "Done." },
    ],
  );
});

test("parts of different messages stay apart, by the agent's message id", () => {
  const { items } = foldRun([
    { kind: "message", text: "One", message: "m1" },
    { kind: "message", text: " more.", message: "m1" },
    { kind: "message", text: "Two.", message: "m2" },
  ]);
  assert.deepEqual(
    items.map((item) => item.text),
    ["One more.", "Two."],
  );
});

const tool = (id, fields = {}) => ({
  kind: "tool",
  id,
  title: `Edit ${id}`,
  tool: "edit",
  status: "pending",
  places: [{ path: "src/a.rs", line: 3 }],
  output: [],
  ...fields,
});

test("a tool call's changes apply to it in place, and only the fields they set", () => {
  const diff = { type: "diff", path: "src/a.rs", oldText: "a\n", newText: "b\n" };
  const { items } = foldRun([
    tool("t1"),
    { kind: "message", text: "between" },
    { kind: "toolUpdate", id: "t1", title: null, tool: null, status: "completed", places: null, output: [diff] },
  ]);
  assert.equal(items.length, 2, "the change is not a row of its own");
  assert.equal(items[0].status, "completed");
  assert.equal(items[0].title, "Edit t1", "a field left out stays");
  assert.deepEqual(items[0].output, [diff]);
});

test("a repeated tool call replaces the first, and a change to an unknown call shows under its id", () => {
  const { items } = foldRun([
    tool("t1"),
    tool("t1", { title: "Edit again" }),
    { kind: "toolUpdate", id: "t9", status: "failed" },
  ]);
  assert.deepEqual(
    items.map((item) => [item.id, item.title, item.status]),
    [
      ["t1", "Edit again", "pending"],
      ["t9", "t9", "failed"],
    ],
  );
});

test("the latest plan wins, and errors and unreadable messages are rows", () => {
  const steps = (status) => [{ content: "Fix N1", status }];
  const { plan, items } = foldRun([
    { kind: "plan", steps: steps("pending") },
    { kind: "unreadable", problem: "not JSON" },
    { kind: "plan", steps: steps("completed") },
    { kind: "failed", error: "the agent exited" },
  ]);
  assert.deepEqual(plan, steps("completed"));
  assert.deepEqual(
    items.map((item) => item.type),
    ["notice", "error"],
  );
  assert.match(items[0].text, /not JSON/);
});

const options = [
  { id: "once", name: "Allow", kind: "allow_once" },
  { id: "always", name: "Always", kind: "allow_always" },
  { id: "no", name: "Reject", kind: "reject_once" },
];

test("a permission request takes its tool's title and is open until answered", () => {
  const { items } = foldRun([
    tool("t1", { title: "cargo test" }),
    { kind: "permission", request: 0, toolCall: "t1", title: null, options },
    { kind: "permission", request: 1, toolCall: "t2", title: "rm -rf target", options },
    { kind: "answer", request: 0, option: "once" },
  ]);
  assert.equal(items[1].title, "cargo test");
  assert.equal(items[1].answer, "once");
  assert.equal(openPermission(items), items[2], "the unanswered one");

  const cancelled = foldRun([
    { kind: "permission", request: 0, toolCall: "t1", title: "x", options },
    { kind: "answer", request: 0, option: null },
  ]).items;
  assert.equal(openPermission(cancelled), null, "a cancelled request is closed");

  const answeredFirst = foldRun([
    { kind: "answer", request: 3, option: null },
    { kind: "permission", request: 3, toolCall: "t1", title: "x", options },
  ]).items;
  assert.equal(answeredFirst[0].answer, null, "an answer logged before its request closes it");
  assert.equal(openPermission(answeredFirst), null);
});

test("keys answer with the options the agent offered, and only those", () => {
  const keys = permissionKeys(options);
  assert.deepEqual([...keys], [
    ["a", "once"],
    ["A", "always"],
    ["r", "no"],
  ]);
  assert.equal(keyFor(keys, "always"), "⇧a");
  assert.equal(keyFor(keys, "other"), "");

  const without = permissionKeys([
    { id: "ok", name: "Allow", kind: "allow_once" },
    { id: "never", name: "Never", kind: "reject_always" },
  ]);
  assert.deepEqual([...without], [
    ["a", "ok"],
    ["r", "never"],
  ], "no always key without an always option; reject falls back to reject always");
});

test("a tool row names the file for file tools and the title for commands", () => {
  const edit = tool("t1", {
    status: "completed",
    output: [{ type: "diff", path: "src/a.rs", oldText: "a\nb\nc\n", newText: "a\nB\nc\nd\n" }],
  });
  assert.deepEqual(toolRow({ ...edit, type: "tool" }, true), {
    verb: "edit",
    target: "src/a.rs",
    state: "+2 -1",
    running: false,
  });
  const command = { tool: "execute", title: "cargo test", places: [], output: [], status: "in_progress" };
  assert.deepEqual(toolRow(command, true), { verb: "run", target: "cargo test", state: "", running: true });
  assert.deepEqual(
    toolRow(command, false),
    { verb: "run", target: "cargo test", state: "unfinished", running: false },
    "a call left open when the run ended is not running",
  );
  assert.equal(toolRow({ ...command, status: "completed" }, true).state, "done");
  assert.equal(toolRow({ ...command, status: "failed" }, true).state, "failed");
  assert.equal(toolRow({ ...command, tool: "teleport" }, true).verb, "tool");
  assert.equal(
    toolRow({ ...command, tool: "read", title: "Read file" }, true).target,
    "Read file",
    "a file tool with no place names its title",
  );
});

test("diff counts are lines added and removed, for new files too", () => {
  assert.equal(diffStats([{ type: "text", text: "x" }]), null);
  assert.deepEqual(diffStats([{ type: "diff", path: "n", oldText: null, newText: "a\nb\n" }]), {
    added: 2,
    removed: 0,
  });
  assert.deepEqual(
    diffStats([
      { type: "diff", path: "a", oldText: "x\ny", newText: "x\nz" },
      { type: "diff", path: "b", oldText: "gone\n", newText: "" },
    ]),
    { added: 1, removed: 2 },
    "summed over every diff",
  );
});

const run = (status, entries = []) => ({ status, agent: "Claude", entries });

test("the status line says where the run is and whether it needs the engineer", () => {
  assert.deepEqual(statusLine(run("starting")), { text: "Starting Claude…", tone: "busy" });
  assert.equal(statusLine(run("waiting")).tone, "attention");
  assert.deepEqual(statusLine(run("ended", [{ kind: "ended", reason: "end_turn" }])), {
    text: "Done",
    tone: "done",
  });
  assert.deepEqual(statusLine(run("ended", [{ kind: "ended", reason: "cancelled" }])), {
    text: "Stopped",
    tone: "idle",
  });
  assert.equal(statusLine(run("ended", [{ kind: "ended", reason: "other" }])).text, "Ended");
  assert.equal(statusLine(run("failed")).tone, "failed");
  assert.match(statusLine(run("unfinished")).text, /another window/);

  assert.equal(isLive(run("cancelling")), true);
  assert.equal(isLive(run("ended")), false);
  assert.equal(isLive(null), false);
});

test("elapsed time reads as minutes, then hours", () => {
  assert.equal(elapsed(0), "0:00");
  assert.equal(elapsed(42.9), "0:42");
  assert.equal(elapsed(3599), "59:59");
  assert.equal(elapsed(3661), "1:01:01");
  assert.equal(elapsed(-5), "0:00", "a clock that went back shows zero");
});

test("prompt lines are told apart for coloring", () => {
  const lines = promptLines(
    "Intro.\n\n[N1] src/a.rs:3\n(These lines are no longer in the file; they were:)\n> }\nFix it.\n[Nope] not a heading\n",
  );
  assert.deepEqual(
    lines.map((line) => line.kind),
    ["text", "text", "heading", "lost", "quote", "text", "text"],
  );
});

test("markdown blocks: paragraphs, lists, headings and fenced code", () => {
  const blocks = markdownBlocks(
    "## Done\n\n- **N3**: `divide` returns *Option*.\n- N4 too\n\n1. first\n2. second\n\nText\nwraps.\n\n```rust\nlet x = a * b; // **not bold**\n```",
  );
  assert.deepEqual(
    blocks.map((block) => block.type),
    ["heading", "list", "list", "paragraph", "code"],
  );
  assert.deepEqual(blocks[1].items[0], [
    { type: "strong", text: "N3" },
    { type: "text", text: ": " },
    { type: "code", text: "divide" },
    { type: "text", text: " returns " },
    { type: "em", text: "Option" },
    { type: "text", text: "." },
  ]);
  assert.equal(blocks[1].ordered, false);
  assert.equal(blocks[2].ordered, true);
  assert.equal(blocks[2].items.length, 2);
  assert.deepEqual(blocks[3].inline, [{ type: "text", text: "Text\nwraps." }]);
  assert.equal(blocks[4].text, "let x = a * b; // **not bold**", "code is never parsed");
});

test("inline marks: code first, and unclosed or in-word marks stay text", () => {
  assert.deepEqual(inline("`a **b**` **c**"), [
    { type: "code", text: "a **b**" },
    { type: "text", text: " " },
    { type: "strong", text: "c" },
  ]);
  assert.deepEqual(inline("2 * 3 and snake_case_name and **open"), [
    { type: "text", text: "2 * 3 and snake_case_name and **open" },
  ]);
  assert.deepEqual(
    inline("a my_var_ b"),
    [{ type: "text", text: "a my_var_ b" }],
    "a mark that starts inside a word is text",
  );
  assert.deepEqual(inline("<b>not html</b>"), [{ type: "text", text: "<b>not html</b>" }]);
});

test("a line diff keeps unchanged lines as context and puts removals before additions", () => {
  const lines = diffLines("a\n}\n/// Divides.\nfn divide() -> i32 {\n", "a\n}\n/// Divides, or None.\nfn divide() -> Option<i32> {\n\n");
  assert.deepEqual(
    lines.map((line) => `${line.kind}:${line.text}`),
    [
      "same:a",
      "same:}",
      "del:/// Divides.",
      "del:fn divide() -> i32 {",
      "add:/// Divides, or None.",
      "add:fn divide() -> Option<i32> {",
      "add:",
    ],
    "a blank line is a line",
  );
  assert.deepEqual(diffLines("", "x"), [{ kind: "add", text: "x" }], "a new file");
});

// The length of the longest common subsequence, by the plain table: the model the line diff
// is checked against.
function lcs(a, b) {
  let previous = new Array(b.length + 1).fill(0);
  for (const line of a) {
    const current = [0];
    for (let j = 0; j < b.length; j += 1) {
      current.push(line === b[j] ? previous[j] + 1 : Math.max(previous[j + 1], current[j]));
    }
    previous = current;
  }
  return previous[b.length];
}

test("the line diff rebuilds both texts with the fewest edits, on random texts", () => {
  // A fixed seed, so a failure repeats.
  let seed = 7;
  const random = (limit) => {
    seed = (seed * 1103515245 + 12345) % 2147483648;
    return seed % limit;
  };
  for (let round = 0; round < 300; round += 1) {
    const text = () => Array.from({ length: random(12) }, () => "abc"[random(3)]);
    const before = text();
    const after = text();
    const lines = diffLines(before.join("\n"), after.join("\n"));
    const old = lines.filter((line) => line.kind !== "add").map((line) => line.text);
    const now = lines.filter((line) => line.kind !== "del").map((line) => line.text);
    assert.deepEqual([old, now], [before, after], `round ${round}`);
    const edits = lines.filter((line) => line.kind !== "same").length;
    assert.equal(edits, before.length + after.length - 2 * lcs(before, after), `round ${round}`);
  }
});

test("a one-line change in a large file is one line, and too many edits count as all changed", () => {
  const big = Array.from({ length: 2001 }, (_, index) => `line ${index}`);
  const changed = big.with(1000, "changed");
  assert.deepEqual(diffStats([{ type: "diff", path: "a", oldText: big.join("\n"), newText: changed.join("\n") }]), {
    added: 1,
    removed: 1,
  });
  const other = big.map((line) => `${line} x`);
  assert.ok(other.length > MAX_EDITS);
  assert.deepEqual(
    diffStats([{ type: "diff", path: "a", oldText: big.join("\n"), newText: other.join("\n") }]),
    { added: 2001, removed: 2001 },
  );
});

test("a permission key is read from the character, with only Shift making it upper case", () => {
  const press = (key, more = {}) => permissionKey({ key, shiftKey: false, repeat: false, ...more });
  assert.equal(press("a"), "a");
  assert.equal(press("A", { shiftKey: true }), "A");
  assert.equal(press("A"), "a", "Caps Lock alone does not allow always");
  assert.equal(press("r"), "r");
  assert.equal(press("R", { shiftKey: true }), null, "Shift-r answers nothing");
  assert.equal(press("a", { repeat: true }), null, "a held key answers once");
  assert.equal(press("a", { metaKey: true }), null, "a shortcut is not an answer");
  // On AZERTY the key labelled A sends code KeyQ and key "a": it answers as its label says.
  assert.equal(press("a", { code: "KeyQ" }), "a");
  assert.equal(press("q", { code: "KeyA" }), null);
});
