import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

// app.css as the page loads it, with its comments removed so prose cannot match.
const css = readFileSync(new URL("app.css", import.meta.url), "utf8").replaceAll(
  /\/\*[\s\S]*?\*\//g,
  "",
);

// The text between `opening` and the first line after it that is `closing`.
function block(opening, closing) {
  const start = css.indexOf(opening);
  assert.notEqual(start, -1, `app.css has no ${opening}`);
  const end = css.indexOf(`\n${closing}\n`, start);
  assert.notEqual(end, -1, `app.css does not close ${opening}`);
  return { start, end: end + closing.length + 2, text: css.slice(start, end) };
}

// The two palettes: Graphite on the bare :root, Paper in the light media query.
const dark = block(":root {", "}");
const light = block("@media (prefers-color-scheme: light) {", "}");

const declared = (text) => new Set([...text.matchAll(/(--[\w-]+):/g)].map((match) => match[1]));
const COLOR = /#[0-9a-f]{3,8}\b|\b(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch)\(|\b(?:white|black)\b/i;

test("every colour in the rules is a palette variable", () => {
  const rules = css.slice(0, dark.start) + css.slice(dark.end, light.start) + css.slice(light.end);
  const literals = rules.split("\n").filter((line) => COLOR.test(line.replace(/white-space/g, "")));
  assert.deepEqual(literals, []);
});

test("Paper sets every colour Graphite sets", () => {
  const paper = declared(light.text);
  // A declaration's value may run over several lines, as a shadow list does.
  const colors = [...dark.text.matchAll(/(--[\w-]+):([^;]*);/g)]
    .filter(([, , value]) => COLOR.test(value))
    .map(([, name]) => name);
  assert.ok(colors.length > 30, "the dark palette was found");
  assert.deepEqual(
    colors.filter((name) => !paper.has(name)),
    [],
  );
});

test("every variable a rule reads without a fallback is declared", () => {
  // A misspelt name makes the declaration that reads it invalid, and the browser drops it
  // without a word. Variables the page sets (pane widths, gutter digits) carry a fallback.
  const names = declared(css);
  const read = [...css.matchAll(/var\((--[\w-]+)\)/g)].map((match) => match[1]);
  assert.deepEqual(
    [...new Set(read)].filter((name) => !names.has(name)),
    [],
  );
});
