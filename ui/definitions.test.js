import assert from "node:assert/strict";
import test from "node:test";

import { MAX_PLACES, countLine, jumpHistory, nameAt } from "./definitions.js";

test("nameAt takes the whole name around the caret, before or after the clicked character", () => {
  const text = "let total = sum(items) + $el._x2;";
  for (const offset of [12, 13, 15]) assert.equal(nameAt(text, offset)?.name, "sum", `${offset}`);
  assert.deepEqual(nameAt(text, 4), { start: 4, end: 9, name: "total" });
  assert.equal(nameAt(text, 26)?.name, "$el");
  assert.equal(nameAt(text, 30)?.name, "_x2", "a digit inside a name is part of it");
  assert.equal(nameAt("größe = 1", 2)?.name, "größe");
  // A name of characters outside the Basic Multilingual Plane, two code units each.
  assert.equal(nameAt("x𝑓𝑔 = 1", 3)?.name, "x𝑓𝑔");
});

test("nameAt finds nothing on whitespace, punctuation or a number", () => {
  const text = "a + 200 :: (b)";
  for (const offset of [2, 5, 9, 11]) assert.equal(nameAt(text, offset), null, `${offset}`);
  assert.equal(nameAt("", 0), null);
});

// The places as `path:line`, for short assertions.
const at = (path, line, scrollTop = null) => ({ path, line, scrollTop, selection: null });

// Steps once and returns the place shown, or null; `gone` lists the paths that cannot be
// shown, as files deleted since.
async function step(history, direction, current, gone = []) {
  let shown = null;
  await history.step(direction, current, async (place) => {
    if (gone.includes(place.path)) return "missing";
    shown = place;
    return "shown";
  });
  return shown && `${shown.path}:${shown.line}`;
}

test("back and forward return to the places jumped from and to", async () => {
  const history = jumpHistory();
  assert.equal(await step(history, -1, at("a.rs", 1)), null, "nothing to go back to yet");

  history.jump(at("a.rs", 3), at("b.rs", 10));
  history.jump(at("b.rs", 10), at("c.rs", 20));
  assert.equal(await step(history, -1, at("c.rs", 20)), "b.rs:10");
  assert.equal(await step(history, -1, at("b.rs", 10)), "a.rs:3");
  assert.equal(await step(history, -1, at("a.rs", 3)), null, "the oldest place stays");
  assert.equal(await step(history, 1, at("a.rs", 3)), "b.rs:10");

  // A jump from the middle drops the places ahead, as in a browser.
  history.jump(at("b.rs", 10), at("d.rs", 1));
  assert.equal(await step(history, 1, at("d.rs", 1)), null);
  assert.equal(await step(history, -1, at("d.rs", 1)), "b.rs:10");
});

test("going back restores where the engineer was in the file, not where the jump landed", async () => {
  const history = jumpHistory();
  history.jump(at("a.rs", 3, 0), at("b.rs", 10));
  // Scrolled and selected in b.rs since the jump, then jumped on from there.
  const moved = { path: "b.rs", line: 10, scrollTop: 400, selection: { anchor: 40, head: 42 } };
  history.jump(moved, at("c.rs", 1));
  let shown;
  await history.step(-1, at("c.rs", 1), async (place) => {
    shown = place;
    return "shown";
  });
  assert.deepEqual(shown, moved);
});

test("a place whose file is gone is skipped, going back and going forward", async () => {
  const history = jumpHistory();
  history.jump(at("a.rs", 1), at("gone.rs", 2));
  history.jump(at("gone.rs", 2), at("c.rs", 3));
  assert.equal(await step(history, -1, at("c.rs", 3), ["gone.rs"]), "a.rs:1");
  assert.equal(await step(history, 1, at("a.rs", 1)), "c.rs:3", "and dropped");
  assert.equal(await step(history, -1, at("c.rs", 3)), "a.rs:1");

  // Forward over a place whose file went after it was passed going back.
  const forward = jumpHistory();
  forward.jump(at("a.rs", 1), at("gone.rs", 2));
  forward.jump(at("gone.rs", 2), at("c.rs", 3));
  assert.equal(await step(forward, -1, at("c.rs", 3)), "gone.rs:2");
  assert.equal(await step(forward, -1, at("gone.rs", 2)), "a.rs:1");
  assert.equal(await step(forward, 1, at("a.rs", 1), ["gone.rs"]), "c.rs:3");
});

test("a step another open overtook stops there and drops no place", async () => {
  const history = jumpHistory();
  history.jump(at("a.rs", 1), at("b.rs", 2));
  history.jump(at("b.rs", 2), at("c.rs", 3));
  const superseded = await history.step(-1, at("c.rs", 3), async () => "superseded");
  assert.equal(superseded, false);
  assert.equal(await step(history, -1, at("c.rs", 3)), "b.rs:2", "b.rs is still there");
});

test("back from a file opened another way returns to the newest place first", async () => {
  const history = jumpHistory();
  history.jump(at("a.rs", 1), at("b.rs", 2));
  // c.rs was opened from the file list since the jump.
  assert.equal(await step(history, -1, at("c.rs", 9)), "b.rs:2");
  assert.equal(await step(history, -1, at("b.rs", 2)), "a.rs:1");
  // And when that newest place is gone, the one before it.
  const gone = jumpHistory();
  gone.jump(at("a.rs", 1), at("b.rs", 2));
  assert.equal(await step(gone, -1, at("c.rs", 9), ["b.rs"]), "a.rs:1");
});

test("the history keeps the latest places, dropping the oldest", async () => {
  const history = jumpHistory();
  for (let line = 1; line <= MAX_PLACES + 5; line += 1) {
    history.jump(at(`f${line}.rs`, line), at(`f${line + 1}.rs`, line + 1));
  }
  let current = at(`f${MAX_PLACES + 6}.rs`, MAX_PLACES + 6);
  let steps = 0;
  for (;;) {
    const shown = await step(history, -1, current);
    if (!shown) break;
    steps += 1;
    current = at(...shown.split(":").map((part, i) => (i ? Number(part) : part)));
  }
  assert.equal(steps, MAX_PLACES - 1);
  assert.equal(current.path, `f${MAX_PLACES + 6 - (MAX_PLACES - 1)}.rs`);
});

test("countLine says how many are listed and that they match by name", () => {
  assert.equal(countLine(2, 2), "2 definitions · by name");
  assert.equal(countLine(50, 312), "50 of 312 definitions · by name");
});
