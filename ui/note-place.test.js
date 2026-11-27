import assert from "node:assert/strict";
import test from "node:test";

import { covers, fullPlace, noteMarks, shortPlace } from "./note-place.js";

const lines = (path, start, end) => ({ path, target: "lines", lines: { start, end } });
const file = (path) => ({ path, target: "file" });
const folder = (path) => ({ path, target: "folder" });

test("a note's place names its lines, its file, or its folder with a slash", () => {
  assert.equal(shortPlace(lines("src/a.rs", 3, 3)), "a.rs:3");
  assert.equal(shortPlace(lines("src/a.rs", 3, 5)), "a.rs:3-5");
  assert.equal(fullPlace(lines("src/a.rs", 3, 5)), "src/a.rs:3-5");
  assert.equal(shortPlace(file("docs/README.md")), "README.md");
  assert.equal(fullPlace(file("docs/README.md")), "docs/README.md");
  assert.equal(shortPlace(folder("src/acp")), "acp/");
  assert.equal(fullPlace(folder("src/acp")), "src/acp/");
  assert.equal(shortPlace(folder("src")), "src/");
});

test("a folder note covers every file below its folder, and no file beside it", () => {
  const note = folder("src/acp");
  assert.equal(covers(note, "src/acp/wire.rs"), true);
  assert.equal(covers(note, "src/acp/deep/x.rs"), true);
  // A sibling whose name starts with the folder's is not inside it.
  assert.equal(covers(note, "src/acp.rs"), false);
  assert.equal(covers(note, "src/acpx/a.rs"), false);
  assert.equal(covers(note, "src/acp"), false);
  assert.equal(covers(note, "src/a.rs"), false);
});

test("a file or line note covers its own file only", () => {
  assert.equal(covers(file("src/a.rs"), "src/a.rs"), true);
  assert.equal(covers(file("src/a.rs"), "src/a.rs.bak"), false);
  assert.equal(covers(lines("src/a.rs", 1, 2), "src/a.rs"), true);
  assert.equal(covers(lines("src/a.rs", 1, 2), "src/b.rs"), false);
});

const note = (id, start, end, body, extra = {}) => ({
  id,
  path: "src/a.rs",
  target: "lines",
  lines: { start, end },
  body,
  anchor: "found",
  state: "open",
  ...extra,
});

test("noteMarks spans a note's lines with its chip on the first line and ignores unplaced notes", () => {
  const notes = [
    note("N1", 2, 4, "Fix loop."),
    note("N2", 1, 2, "Lost.", { anchor: "lost" }),
    note("N3", 1, 2, "Other file.", { path: "src/b.rs" }),
    { id: "N4", path: "src/a.rs", target: "file", body: "File note.", anchor: "found" },
  ];
  const marks = noteMarks(notes, "src/a.rs", 10);
  assert.deepEqual([...marks.keys()], [1, 2, 3]);
  assert.equal(marks.get(1).noteId, "N1");
  assert.equal(marks.get(1).chip.label, "N1 · Fix loop.");
  assert.deepEqual(marks.get(1).chip.others, []);
  assert.equal(marks.get(2).chip, null);
  assert.equal(marks.get(3).chip, null);
  assert.equal(noteMarks(notes, "src/a.rs", 0).size, 0);
  assert.equal(noteMarks(notes, "src/a.rs", 2).size, 1);
});

test("overlapping notes give the line to the innermost (or newest on ties) and list others in +N", () => {
  const notes = [
    note("N1", 1, 6, "Outer."),
    note("N2", 3, 4, "Inner."),
    note("N3", 3, 4, "Newer inner."),
  ];
  const marks = noteMarks(notes, "src/a.rs", 10);
  assert.equal(marks.get(0).noteId, "N1");
  assert.equal(marks.get(0).chip.noteId, "N1");
  assert.deepEqual(marks.get(0).chip.others, []);

  // Lines 3-4 (0-based 2-3): N3 wins (same narrow span as N2, newer), with N1 and N2 in others.
  assert.equal(marks.get(2).noteId, "N3");
  assert.equal(marks.get(2).chip.noteId, "N3");
  assert.deepEqual(
    marks.get(2).chip.others.map((other) => other.noteId),
    ["N1", "N2"],
  );
  assert.equal(marks.get(3).noteId, "N3");
  assert.equal(marks.get(3).chip, null);

  // Line 5 (0-based 4): N1 resumes without repeating its chip.
  assert.equal(marks.get(4).noteId, "N1");
  assert.equal(marks.get(4).chip, null);
});

test("noteMarks tracks resolved state and changes its key when body or state changes", () => {
  const open = noteMarks([note("N1", 1, 1, "Check this.")], "src/a.rs", 5).get(0);
  const edited = noteMarks([note("N1", 1, 1, "Updated.")], "src/a.rs", 5).get(0);
  const resolved = noteMarks(
    [note("N1", 1, 1, "Updated.", { state: "resolved" })],
    "src/a.rs",
    5,
  ).get(0);
  assert.equal(open.resolved, false);
  assert.equal(resolved.resolved, true);
  assert.notEqual(open.key, edited.key);
  assert.notEqual(edited.key, resolved.key);
});
