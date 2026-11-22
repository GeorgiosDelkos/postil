// Run with `node --test ui/` (Node's built-in runner: the page has no npm packages).
// `cargo test` runs it too, through tests/page_logic.rs.

import assert from "node:assert/strict";
import { test } from "node:test";

import { rank } from "./fuzzy.js";

const paths = (results) => results.map((result) => result.path);

test("only paths that contain the query's characters in order match", () => {
  const files = ["src/shell.rs", "src/repo.rs", "docs/design.md"];

  assert.deepEqual(paths(rank("shl", files, 10)), ["src/shell.rs"]);
  assert.deepEqual(paths(rank("lhs", files, 10)), []);
  assert.deepEqual(paths(rank("zzz", files, 10)), []);
});

test("an empty or blank query matches nothing", () => {
  assert.deepEqual(rank("", ["a.rs"], 10), []);
  assert.deepEqual(rank("   ", ["a.rs"], 10), []);
});

test("matching ignores case and spaces in the query", () => {
  assert.deepEqual(paths(rank("READ me", ["README.md"], 10)), ["README.md"]);
  assert.deepEqual(paths(rank("readme", ["README.md"], 10)), ["README.md"]);
});

test("a match in the file name outranks a match in the directories", () => {
  const files = ["shell/notes/a.txt", "src/shell.rs"];

  assert.deepEqual(paths(rank("shell", files, 10)), ["src/shell.rs", "shell/notes/a.txt"]);
});

test("adjacent characters outrank the same characters scattered", () => {
  const files = ["src/a_b_c.rs", "src/abc.rs"];

  assert.deepEqual(paths(rank("abc", files, 10)), ["src/abc.rs", "src/a_b_c.rs"]);
});

test("the start of a word outranks its middle", () => {
  const files = ["src/preview.rs", "src/view.rs"];

  assert.deepEqual(paths(rank("view", files, 10)), ["src/view.rs", "src/preview.rs"]);
});

test("the shorter of two equal matches comes first", () => {
  const files = ["src/deeply/nested/main.rs", "src/main.rs"];

  assert.deepEqual(paths(rank("main", files, 10)), ["src/main.rs", "src/deeply/nested/main.rs"]);
});

test("the limit caps the results after ranking", () => {
  const files = ["a/x1.rs", "a/x2.rs", "x.rs"];

  assert.deepEqual(paths(rank("x", files, 1)), ["x.rs"]);
});

test("positions index the matched characters, in order", () => {
  const files = ["src/acp/connection.rs", "tests/fakeAgent.rs", "Cargo.toml", "ui/app.css"];

  for (const query of ["conn", "fa", "car", "ac", "s", "rs"]) {
    for (const { path, positions } of rank(query, files, 10)) {
      const marked = positions.map((index) => path[index].toLowerCase()).join("");
      assert.equal(marked, query, `${query} in ${path}`);
      const increasing = positions.every((index, i) => i === 0 || index > positions[i - 1]);
      assert.ok(increasing, `${query} in ${path}: ${positions}`);
    }
  }
});
