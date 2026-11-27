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

test("a live run of another thread is followed while there is no latest run, as an analysis", async (t) => {
  // An analysis is never the latest run: `api/run` answers null all along.
  const analysis = { id: "R1", status: "running", total: 1, entries: ["a"], analysis: {} };
  const replies = new Map([
    ["api/run?after=0", null],
    ["api/runs", { runs: ["R1"], live: [{ id: "R1", thread: "R1" }] }],
    ["api/run?id=R1&after=0", analysis],
    ["api/run?id=R1&after=1", { ...analysis, status: "ended", total: 2, entries: ["b"] }],
  ]);
  t.mock.method(globalThis, "fetch", async (url) => new Response(JSON.stringify(replies.get(url))));
  const store = runStore({
    render() {},
    onChange() {},
    onLoaded() {},
    onError: (text) => assert.fail(text),
  });

  await store.track();
  assert.equal(store.byId("R1").status, "running");
  // The nudge of its next entry: the list is not stale, and still the run is read again.
  await store.sync();
  assert.equal(store.byId("R1").status, "ended");
  assert.deepEqual(store.byId("R1").entries, ["a", "b"]);
});
