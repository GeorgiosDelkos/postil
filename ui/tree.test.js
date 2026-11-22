import assert from "node:assert/strict";
import test from "node:test";

import { ancestors, buildTree, folderCounts, visibleRows } from "./tree.js";

const show = (rows) => rows.map((row) => `${"  ".repeat(row.depth)}${row.name}`);

test("folders come before files, names ignore case and sort digits by value", () => {
  const tree = buildTree(["b.rs", "src/z.rs", "A.rs", "file10.rs", "file2.rs", "Docs/x.md"]);
  assert.deepEqual(show(visibleRows(tree, new Set())), [
    "Docs",
    "src",
    "A.rs",
    "b.rs",
    "file2.rs",
    "file10.rs",
  ]);
});

test("only open folders show their contents, at one more level of depth", () => {
  const tree = buildTree(["src/acp/wire.rs", "src/main.rs", "README.md"]);

  assert.deepEqual(show(visibleRows(tree, new Set(["src"]))), [
    "src",
    "  acp",
    "  main.rs",
    "README.md",
  ]);
  assert.deepEqual(show(visibleRows(tree, new Set(["src", "src/acp"]))), [
    "src",
    "  acp",
    "    wire.rs",
    "  main.rs",
    "README.md",
  ]);
});

test("an open folder inside a closed one stays hidden and reopens as it was", () => {
  const tree = buildTree(["src/acp/wire.rs"]);
  assert.deepEqual(show(visibleRows(tree, new Set(["src/acp"]))), ["src"]);

  const rows = visibleRows(tree, new Set(["src", "src/acp"]));
  assert.deepEqual(
    rows.map(({ kind, path, open }) => [kind, path, open]),
    [
      ["folder", "src", true],
      ["folder", "src/acp", true],
      ["file", "src/acp/wire.rs", false],
    ],
  );
});

test("folders that share a name are told apart by their path", () => {
  const tree = buildTree(["a/src/x.rs", "b/src/y.rs"]);
  const rows = visibleRows(tree, new Set(["a", "b", "a/src"]));
  assert.deepEqual(show(rows), ["a", "  src", "    x.rs", "b", "  src"]);
});

test("names equal but for case keep one order whatever the input order", () => {
  const forward = show(visibleRows(buildTree(["readme", "README"]), new Set()));
  const backward = show(visibleRows(buildTree(["README", "readme"]), new Set()));
  assert.deepEqual(forward, backward);
});

test("ancestors are the folders to open, outermost first", () => {
  assert.deepEqual(ancestors("src/acp/wire.rs"), ["src", "src/acp"]);
  assert.deepEqual(ancestors("README.md"), []);
});

test("a folder's note count is the sum over every file below it", () => {
  const counts = new Map([
    ["src/acp/wire.rs", 2],
    ["src/main.rs", 1],
    ["README.md", 4],
  ]);
  assert.deepEqual(
    folderCounts(counts),
    new Map([
      ["src", 3],
      ["src/acp", 2],
    ]),
  );
});
