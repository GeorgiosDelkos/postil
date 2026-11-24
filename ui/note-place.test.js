import assert from "node:assert/strict";
import test from "node:test";

import { covers, fullPlace, shortPlace } from "./note-place.js";

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
