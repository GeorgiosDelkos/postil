import assert from "node:assert/strict";
import { test } from "node:test";

import { insertion, sliceDelta } from "./dictation.js";

test("insertion preserves text before and after the caret", () => {
  const first = insertion("before  after", { start: 7, end: 7 }, "spoken");
  assert.equal(first.text, "before spoken after");
  assert.deepEqual(first.range, { start: 7, end: 13 });

  const grown = insertion(first.text, first.range, "spoken words");
  assert.equal(grown.text, "before spoken words after");
  assert.deepEqual(grown.range, { start: 7, end: 19 });
});

test("insertion handles a shrinking partial and an empty partial", () => {
  const initial = insertion("prefix hello world suffix", { start: 7, end: 18 }, "hello");
  assert.equal(initial.text, "prefix hello suffix");
  assert.deepEqual(initial.range, { start: 7, end: 12 });

  const cleared = insertion(initial.text, initial.range, "");
  assert.equal(cleared.text, "prefix  suffix");
  assert.deepEqual(cleared.range, { start: 7, end: 7 });
});

test("insertion commits a final result in place", () => {
  const partial = insertion("fix ", { start: 4, end: 4 }, "the race");
  const final = insertion(partial.text, partial.range, "the race condition.");
  assert.equal(final.text, "fix the race condition.");
  assert.deepEqual(final.range, { start: 4, end: 23 });
});

test("caret moved mid-partial commits the partial and continues at the new caret", () => {
  const first = insertion("and more", { start: 0, end: 0 }, "first part ");
  assert.equal(first.text, "first part and more");
  const delta = sliceDelta("first part at the end", "first part");
  assert.equal(delta, "at the end");
  const second = insertion(first.text + " ", { start: 20, end: 20 }, delta);
  assert.equal(second.text, "first part and more at the end");
  assert.deepEqual(second.range, { start: 20, end: 30 });
});

test("sliceDelta handles case changes, punctuation revisions, and word boundaries", () => {
  assert.equal(sliceDelta("Hello world next", "hello world"), "next");
  assert.equal(sliceDelta("Hello, world. Next sentence", "hello world"), "Next sentence");
  assert.equal(sliceDelta("testing one two", "test"), "testing one two");
  assert.equal(sliceDelta("fresh words", ""), "fresh words");
  assert.equal(sliceDelta("hello", "hello"), "");
});
