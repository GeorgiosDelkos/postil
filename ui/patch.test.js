import assert from "node:assert/strict";
import test from "node:test";

import {
  findingsByLine,
  hunkRisk,
  notesByLine,
  parsePatch,
  unquote,
  walkthrough,
} from "./patch.js";

// As `git diff --no-color --no-renames --unified=3` printed it for two snapshots.
const patch = `diff --git a/a.txt b/a.txt
index 7898192..4d1c4fb 100644
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 a
-prompt 1
+prompt 2
 c
diff --git a/new file.txt b/new file.txt
new file mode 100644
index 0000000..3e75765
--- /dev/null
+++ b/new file.txt
@@ -0,0 +1 @@
+new
\\ No newline at end of file
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 3e75765..0000000
--- a/old.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-gone
-too
diff --git a/logo.png b/logo.png
index 1111111..2222222 100644
Binary files a/logo.png and b/logo.png differ
`;

test("a patch reads as files with their status, counts and numbered lines", () => {
  const files = parsePatch(patch);
  assert.deepEqual(
    files.map((file) => [file.path, file.status, file.binary, file.added, file.removed]),
    [
      ["a.txt", "modified", false, 1, 1],
      ["new file.txt", "added", false, 1, 0],
      ["old.txt", "deleted", false, 0, 2],
      ["logo.png", "modified", true, 0, 0],
    ],
  );
  assert.deepEqual(files[0].hunks[0].lines, [
    { kind: "context", old: 1, new: 1, text: "a" },
    { kind: "del", old: 2, new: null, text: "prompt 1" },
    { kind: "add", old: null, new: 2, text: "prompt 2" },
    { kind: "context", old: 3, new: 3, text: "c" },
  ]);
  assert.deepEqual(
    files[1].hunks[0].lines,
    [{ kind: "add", old: null, new: 1, text: "new" }],
    "the no-newline marker is not a line",
  );
  assert.equal(files[3].hunks.length, 0);
  assert.deepEqual(parsePatch(""), []);
});

test("quoted paths are read back, and a second hunk numbers its lines from its header", () => {
  const quoted = [
    'diff --git "a/tab\\there.txt" "b/tab\\there.txt"',
    "index 1..2 100644",
    '--- "a/tab\\there.txt"',
    '+++ "b/tab\\there.txt"',
    "@@ -1,2 +1,2 @@",
    " one",
    "-two",
    "+zwei",
    "@@ -40,2 +40,3 @@ fn later() {",
    " forty",
    "+inserted",
    " forty-one",
    'diff --git "a/caf\\303\\251 \\"x\\".png" "b/caf\\303\\251 \\"x\\".png"',
    "Binary files differ",
    "",
  ].join("\n");
  const files = parsePatch(quoted);
  assert.deepEqual(
    files.map((file) => file.path),
    ["tab\there.txt", 'café "x".png'],
  );
  assert.deepEqual(
    files[0].hunks[1].lines.map((line) => [line.kind, line.old, line.new]),
    [
      ["context", 40, 40],
      ["add", null, 41],
      ["context", 41, 42],
    ],
  );
  assert.equal(unquote("plain/path.rs"), "plain/path.rs");
  assert.equal(unquote('"a\\\\b\\nc"'), "a\\b\nc");
});

test("a character outside the BMP in a quoted path is read whole, not as two halves", () => {
  // git leaves it unescaped with core.quotePath off, and quotes the path for its tab.
  assert.equal(unquote('"notes/\u{1F600}\\tdraft.md"'), "notes/\u{1F600}\tdraft.md");
});

test("a note shows at the changed line its lines end on, and only there", () => {
  const [file] = parsePatch(patch);
  const note = (id, path, end) => ({ id, path, target: "lines", lines: { start: end, end } });
  const placed = notesByLine(file, [
    note("N1", "a.txt", 2),
    note("N2", "a.txt", 2),
    note("N3", "a.txt", 9),
    note("N4", "b.txt", 2),
    // A note on the whole file has no line to show beside.
    { id: "N5", path: "a.txt", target: "file" },
  ]);
  assert.deepEqual(
    [...placed].map(([line, notes]) => [line, notes.map((n) => n.id)]),
    [[2, ["N1", "N2"]]],
  );
});

test("a hunk keeps its header's ranges, a count of 0 for a side with no lines", () => {
  const emptied = [
    "diff --git a/e.txt b/e.txt",
    "index 1..2 100644",
    "--- a/e.txt",
    "+++ b/e.txt",
    "@@ -1 +0,0 @@",
    "-x",
    "diff --git a/two.txt b/two.txt",
    "new file mode 100644",
    "--- /dev/null",
    "+++ b/two.txt",
    "@@ -0,0 +1,2 @@",
    "+one",
    "+two",
    "",
  ].join("\n");
  const ranges = (files) =>
    files.map((file) => [
      file.path,
      file.hunks.map((h) => [h.oldStart, h.oldCount, h.newStart, h.newCount]),
    ]);
  assert.deepEqual(ranges(parsePatch(patch)), [
    ["a.txt", [[1, 3, 1, 3]]],
    // A one-line file: git leaves out the count of 1.
    ["new file.txt", [[0, 0, 1, 1]]],
    ["old.txt", [[1, 2, 0, 0]]],
    ["logo.png", []],
  ]);
  assert.deepEqual(ranges(parsePatch(emptied)), [
    ["e.txt", [[1, 1, 0, 0]]],
    ["two.txt", [[0, 0, 1, 2]]],
  ]);
});

// A file as `walkthrough` reads it: a path and hunks with their new-side ranges.
const file = (path, ...ranges) => ({
  path,
  binary: false,
  hunks: ranges.map(([newStart, newCount]) => ({ newStart, newCount, lines: [] })),
});
const step = (path, lines, risk = "routine") => ({
  path,
  why: "",
  risk,
  ...(lines ? { lines: { start: lines[0], end: lines[1] } } : {}),
});
// Each group as `[number, [[path, newStart] per entry], shownIn]`, and the rest's entries.
const shape = ({ groups, rest }) => ({
  groups: groups.map((group) => [
    group.number,
    group.entries.map(({ file, hunk }) => [file.path, hunk?.newStart ?? null]),
    group.shownIn,
  ]),
  rest: rest.map(({ file, hunk }) => [file.path, hunk?.newStart ?? null]),
});

test("a step takes the hunks its lines overlap, ends included, and the rest are left", () => {
  const files = [file("a.rs", [1, 5], [10, 3]), file("b.rs", [4, 2])];
  assert.deepEqual(shape(walkthrough(files, [step("a.rs", [5, 9]), step("b.rs", [6, 8])])), {
    groups: [
      [1, [["a.rs", 1]], []],
      // b.rs's hunk is lines 4 and 5: line 6 is past it.
      [2, [], []],
    ],
    rest: [
      ["a.rs", 10],
      ["b.rs", 4],
    ],
  });
  // Line 12 is the last of the hunk at 10, three lines long.
  assert.deepEqual(shape(walkthrough(files, [step("a.rs", [12, 30])])).groups, [
    [1, [["a.rs", 10]], []],
  ]);
});

test("a hunk two steps overlap is the first's, and the second says where it is", () => {
  const files = [file("a.rs", [1, 5], [10, 3])];
  const steps = [step("a.rs", [2, 2]), step("a.rs", [3, 11]), step("a.rs", [4, 4])];
  assert.deepEqual(shape(walkthrough(files, steps)).groups, [
    [1, [["a.rs", 1]], []],
    [2, [["a.rs", 10]], [1]],
    [3, [], [1]],
  ]);
});

test("a hunk with no new side, a step with no lines and a binary file match by path", () => {
  const files = [
    file("gone.rs", [0, 0]),
    file("emptied.rs", [0, 0]),
    file("whole.rs", [1, 2], [40, 1]),
    { path: "logo.png", binary: true, hunks: [] },
    { path: "other.png", binary: true, hunks: [] },
  ];
  const steps = [
    step("gone.rs", [3, 4], "mechanical"),
    step("emptied.rs", null),
    step("whole.rs", null),
    step("logo.png", [1, 1]),
    step("logo.png", null),
  ];
  assert.deepEqual(shape(walkthrough(files, steps)), {
    groups: [
      [1, [["gone.rs", 0]], []],
      [2, [["emptied.rs", 0]], []],
      [
        3,
        [
          ["whole.rs", 1],
          ["whole.rs", 40],
        ],
        [],
      ],
      [4, [["logo.png", null]], []],
      [5, [], [4]],
    ],
    rest: [["other.png", null]],
  });
  assert.deepEqual(shape(walkthrough(files, [])).groups, []);
});

// A small seeded generator (mulberry32), so a failing case is the same on every run.
function random(seed) {
  let state = seed;
  return (below) => {
    state = (state + 0x6d2b79f5) | 0;
    let t = Math.imul(state ^ (state >>> 15), 1 | state);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) % below;
  };
}

test("every hunk is in exactly one step or in the rest, for any steps", () => {
  for (let seed = 1; seed <= 300; seed += 1) {
    const next = random(seed);
    const paths = ["a", "b", "c", "d"];
    const files = paths.slice(0, 1 + next(4)).map((path) => {
      if (next(6) === 0) return { path, binary: true, hunks: [] };
      let at = 1;
      const ranges = Array.from({ length: next(4) }, () => {
        const range = [at + next(5), next(5)];
        at = range[0] + range[1] + 1;
        return range;
      });
      return file(path, ...ranges);
    });
    const steps = Array.from({ length: next(6) }, () => {
      const start = 1 + next(30);
      return step(paths[next(paths.length)], next(4) === 0 ? null : [start, start + next(8)]);
    });
    const { groups, rest } = walkthrough(files, steps);
    const placed = [...groups.flatMap((group) => group.entries), ...rest];
    const expected = files.flatMap((f) => (f.hunks.length ? f.hunks : [null]));
    assert.equal(placed.length, expected.length, `seed ${seed}`);
    assert.deepEqual(
      new Set(placed.map((entry) => entry.hunk ?? entry.file)),
      new Set(files.flatMap((f) => (f.hunks.length ? f.hunks : [f]))),
      `seed ${seed}`,
    );
    for (const group of groups)
      for (const entry of group.entries) assert.equal(entry.file.path, group.step.path);
  }
});

test("a finding shows at its changed line, else with its file, and another file's not at all", () => {
  const [a] = parsePatch(patch);
  const finding = (id, path, line) => ({ id, path, line, text: id });
  const { placed, unplaced } = findingsByLine(a, [
    finding("F1", "a.txt", 2),
    finding("F2", "a.txt", 3),
    finding("F3", "a.txt", 2),
    // Not in the hunk, and no line at all.
    finding("F4", "a.txt", 9),
    finding("F5", "a.txt", null),
    finding("F6", "b.txt", 2),
    finding("F7", null, null),
  ]);
  assert.deepEqual(
    [...placed].map(([line, found]) => [line, found.map((f) => f.id)]),
    [
      [2, ["F1", "F3"]],
      [3, ["F2"]],
    ],
  );
  assert.deepEqual(
    unplaced.map((f) => f.id),
    ["F4", "F5"],
  );
});

test("a finding on a hunk's lines rates it eyes, whatever the report said", () => {
  const [a, , old] = parsePatch(patch);
  const on = (line) => findingsByLine(a, [{ id: "F1", path: "a.txt", line }]).placed;
  assert.equal(hunkRisk("mechanical", a.hunks[0], on(3)), "eyes");
  assert.equal(hunkRisk("routine", a.hunks[0], on(1)), "eyes");
  assert.equal(hunkRisk("mechanical", a.hunks[0], on(9)), "mechanical");
  // A deleted file's hunk has no new line for a finding to be on.
  assert.equal(hunkRisk("routine", old.hunks[0], on(1)), "routine");
  assert.equal(hunkRisk("routine", null, on(1)), "routine");
});
