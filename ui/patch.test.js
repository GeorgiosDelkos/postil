import assert from "node:assert/strict";
import test from "node:test";

import { notesByLine, parsePatch, unquote } from "./patch.js";

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
  assert.deepEqual(files[1].hunks[0].lines, [{ kind: "add", old: null, new: 1, text: "new" }], "the no-newline marker is not a line");
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
  assert.deepEqual(files.map((file) => file.path), ["tab\there.txt", 'café "x".png']);
  assert.deepEqual(
    files[0].hunks[1].lines.map((line) => [line.kind, line.old, line.new]),
    [["context", 40, 40], ["add", null, 41], ["context", 41, 42]],
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
  const note = (id, path, end) => ({ id, path, lines: { start: end, end } });
  const placed = notesByLine(file, [note("N1", "a.txt", 2), note("N2", "a.txt", 2), note("N3", "a.txt", 9), note("N4", "b.txt", 2)]);
  assert.deepEqual([...placed].map(([line, notes]) => [line, notes.map((n) => n.id)]), [[2, ["N1", "N2"]]]);
});
