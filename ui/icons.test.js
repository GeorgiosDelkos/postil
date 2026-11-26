import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { agentLogo, fileIcon, folderIcon } from "./icons.js";

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

test("no icon paints its bounding box", () => {
  // Some upstream icons start with a full-size box path that only their root's
  // fill="none" keeps invisible; a symbol that lost it draws a solid black tile.
  for (const [, id, attributes, body] of sprite.matchAll(
    /<symbol id="([^"]+)"([^>]*)>([\s\S]*?)<\/symbol>/g,
  )) {
    const unfilledBox = /<path d="M0 0h\d+v\d+H0z"\/>/.test(body);
    assert.ok(!unfilledBox || attributes.includes('fill="none"'), `${id} paints a black box`);
  }
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

const logos = readFileSync(new URL("./agent-logos.svg", import.meta.url), "utf8");

test("each built-in agent's logo is a symbol of agent-logos.svg, other names get none", () => {
  const ids = new Set([...logos.matchAll(/<symbol id="([^"]+)"/g)].map((match) => match[1]));
  const named = ["Claude", "Codex", "Gemini", "Grok", "OpenCode"].map(agentLogo);
  assert.deepEqual(named, ["agent-claude", "agent-codex", "agent-gemini", null, "agent-opencode"]);
  for (const id of named.filter(Boolean)) {
    assert.ok(ids.has(id), `${id} is missing from agent-logos.svg`);
  }
  // A custom agent cannot take a built-in name in any case (saveCustomAgent in settings.js),
  // so a name that differs only in case is not a built-in agent.
  assert.equal(agentLogo("opencode"), null);
  assert.equal(agentLogo("Local"), null);
});

test("agent-logos.svg holds only symbols of plain paths: no script, link, style or image", () => {
  // The logos are third-party files: what else they carried was stripped, and stays out.
  const body = logos.replaceAll(/<!--[\s\S]*?-->/g, "");
  const tags = new Set([...body.matchAll(/<([a-zA-Z][\w:-]*)/g)].map((match) => match[1]));
  assert.deepEqual([...tags].sort(), ["path", "svg", "symbol"]);
  const names = new Set([...body.matchAll(/\s([a-zA-Z][\w:-]*)=/g)].map((match) => match[1]));
  const allowed = new Set(["class", "d", "fill", "id", "viewBox", "xmlns"]);
  assert.deepEqual(
    [...names].filter((name) => !allowed.has(name)),
    [],
  );
});

test("the sprites' comments hold no double hyphen, which XML forbids in a comment", () => {
  // The page parses each sprite as XML (loadIcons in dom.js): one "--" in a comment, such as
  // a CSS variable's name, fails the whole file and every icon in it is an empty box.
  for (const file of [sprite, logos]) {
    for (const [, text] of file.matchAll(/<!--([\s\S]*?)-->/g)) {
      assert.ok(!text.includes("--"), `a comment holds "--": ${text.slice(0, 80)}`);
    }
  }
});
