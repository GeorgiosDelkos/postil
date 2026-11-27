import assert from "node:assert/strict";
import test from "node:test";

import { runStore } from "./run-store.js";

// Lets the failed fetches settle; setImmediate is not one of the mocked timers.
const settle = () => new Promise((resolve) => setImmediate(resolve));

test("failed fetches keep one retry waiting, not one per failure", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let fetches = 0;
  t.mock.method(globalThis, "fetch", async () => {
    fetches += 1;
    throw new Error("offline");
  });
  const errors = [];
  // The store draws only after a fetch succeeds, and these all fail.
  const store = runStore({
    render: () => assert.fail("nothing to draw"),
    onChange: () => assert.fail("no run changed"),
    onLoaded: () => assert.fail("no run was read"),
    onError: (text) => errors.push(text),
  });

  // The first failure waits 1 s to retry; a second, before then, waits 2 s and replaces it.
  await store.sync();
  await store.sync();
  assert.equal(fetches, 2);
  t.mock.timers.tick(1000);
  await settle();
  assert.equal(fetches, 2, "the first retry was replaced by the second");
  t.mock.timers.tick(1000);
  await settle();
  assert.equal(fetches, 3);
  assert.deepEqual(errors, ["offline", "offline", "offline"]);
});

test("a later reply updates the run and keeps its notes and prompt, which it leaves out", async (t) => {
  const replies = {
    "api/run?after=0": {
      id: "R1",
      notes: ["N1"],
      prompt: "p",
      status: "running",
      total: 1,
      entries: ["a"],
    },
    "api/run?after=1": { id: "R1", status: "ended", total: 2, entries: ["b"] },
    "api/runs": { runs: ["R1"] },
  };
  t.mock.method(globalThis, "fetch", async (url) => new Response(JSON.stringify(replies[url])));
  const store = runStore({
    render() {},
    onChange() {},
    onLoaded() {},
    onError: (text) => assert.fail(text),
  });

  await store.sync();
  await store.sync();
  assert.deepEqual(store.run(), {
    id: "R1",
    notes: ["N1"],
    prompt: "p",
    status: "ended",
    total: 2,
    entries: ["a", "b"],
  });
});

test("a memory draft's thread is busy while the draft runs, and free once it ended", async (t) => {
  const r1 = { id: "R1", notes: ["N1"], prompt: "p", status: "ended", total: 0, entries: [] };
  let draft = { id: "R2", status: "running", total: 0, entries: [], remember: { of: "R1" } };
  let live = [];
  t.mock.method(globalThis, "fetch", async (url) => {
    const reply = url.startsWith("api/runs")
      ? { runs: live.length ? ["R2", "R1"] : ["R1"], live }
      : url.startsWith("api/run?id=R2")
        ? draft
        : r1;
    return new Response(JSON.stringify(reply));
  });
  const store = runStore({
    render() {},
    onChange() {},
    onLoaded() {},
    onError: (text) => assert.fail(text),
  });
  await store.sync();
  assert.equal(store.threadLive("R1"), false);

  // The draft never becomes the latest run: `started` reads the list itself.
  live = [{ id: "R2", thread: "R1" }];
  await store.started();
  assert.equal(store.threadLive("R1"), true);
  assert.equal(store.run().id, "R1");

  // Its end's nudge: the list read last still names it.
  draft = { ...draft, status: "ended" };
  await store.sync();
  assert.equal(store.threadLive("R1"), false);
});
