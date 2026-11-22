import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { fileIcon, folderIcon } from "./icons.js";

const sprite = readFileSync(new URL("./icons.svg", import.meta.url), "utf8");
const symbols = new Set([...sprite.matchAll(/<symbol id="([^"]+)"/g)].map((match) => match[1]));

test("files get the icon of their language, whatever the case of the name", () => {
  assert.equal(fileIcon("main.rs"), "icon-rust");
  assert.equal(fileIcon("App.TSX"), "icon-react_ts");
  assert.equal(fileIcon("fuzzy.test.js"), "icon-javascript");
  assert.equal(fileIcon("Cargo.toml"), "icon-toml");
});

test("whole names win over extensions, and readme and license prefixes over both", () => {
  assert.equal(fileIcon("Cargo.lock"), "icon-lock");
  assert.equal(fileIcon("package.json"), "icon-nodejs");
  assert.equal(fileIcon("README.md"), "icon-readme");
  assert.equal(fileIcon("LICENSE-MIT"), "icon-license");
  assert.equal(fileIcon(".gitignore"), "icon-git");
});

test("unknown files, and hidden files without an extension, get the plain document", () => {
  assert.equal(fileIcon("notes.xyz"), "icon-document");
  assert.equal(fileIcon("Procfile"), "icon-document");
  assert.equal(fileIcon(".bashrc"), "icon-document");
});

test("folders have a closed and an open icon, special names their own", () => {
  assert.equal(folderIcon("src", false), "icon-folder-src");
  assert.equal(folderIcon("Tests", true), "icon-folder-test-open");
  assert.equal(folderIcon(".github", false), "icon-folder-github");
  assert.equal(folderIcon("acp", true), "icon-folder-base-open");
});

test("every icon the mapping can return is in the sprite", () => {
  // Read the mapping's tables back through its functions: every folder kind, open and
  // closed, and every file icon over a sample of names that reaches each entry.
  const source = readFileSync(new URL("./icons.js", import.meta.url), "utf8");
  const pairs = [...source.matchAll(/\["([^"]+)", "([^"]+)"\]/g)].map((match) => match[1]);
  const ids = new Set(["icon-document", "icon-readme", "icon-license"]);
  for (const key of pairs) {
    ids.add(folderIcon(key, false));
    ids.add(folderIcon(key, true));
    ids.add(fileIcon(key));
    ids.add(fileIcon(`x.${key}`));
  }
  for (const id of ids) assert.ok(symbols.has(id), `${id} is missing from icons.svg`);
});
