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
  const ended = [];
  const store = runStore({
    render() {},
    onChange: (_run, _previous, { liveEnded }) => ended.push(liveEnded),
    onLoaded() {},
    onError: (text) => assert.fail(text),
  });

  await store.track();
  assert.equal(store.byId("R1").status, "running");
  // The nudge of its next entry: the list is not stale, and still the run is read again.
  await store.sync();
  assert.equal(store.byId("R1").status, "ended");
  assert.deepEqual(store.byId("R1").entries, ["a", "b"]);
  // The page is told when it ended, and only then: it refreshes, and checks HEAD again.
  assert.deepEqual(ended, [false, true]);
});

test("a live run that ended after forget() is read final and reported", async (t) => {
  const analysis = { id: "R1", status: "running", total: 1, entries: ["a"], analysis: {} };
  let live = [{ id: "R1", thread: "R1" }];
  const replies = new Map([
    ["api/run?after=0", null],
    ["api/run?id=R1&after=0", analysis],
    ["api/run?id=R1&after=1", { ...analysis, status: "ended", total: 2, entries: ["b"] }],
  ]);
  t.mock.method(globalThis, "fetch", async (url) => {
    const reply = url === "api/runs" ? { runs: ["R1"], live } : replies.get(url);
    return new Response(JSON.stringify(reply));
  });
  const ended = [];
  const store = runStore({
    render() {},
    onChange: (_run, _previous, { liveEnded }) => ended.push(liveEnded),
    onLoaded() {},
    onError: (text) => assert.fail(text),
  });

  await store.track();
  assert.equal(store.byId("R1").status, "running");
  // The window's focus forgets what may have changed; R1 ends, and the list no longer names it.
  store.forget();
  live = [];
  await store.sync();
  assert.equal(store.byId("R1").status, "ended");
  assert.deepEqual(ended, [false, true]);
});

test("knownViews is read-only and does not trigger fetches for uncached run ids", async (t) => {
  const ids = Array.from({ length: 50 }, (_, i) => `R${50 - i}`);
  const fetched = [];
  t.mock.method(globalThis, "fetch", async (url) => {
    fetched.push(url);
    if (url === "api/runs") return new Response(JSON.stringify({ runs: ids, live: [] }));
    return new Response(
      JSON.stringify({ id: "R50", notes: [], prompt: "", status: "ended", total: 0, entries: [] }),
    );
  });
  const store = runStore({
    render() {},
    onChange() {},
    onLoaded() {},
    onError: (text) => assert.fail(text),
  });
  await store.sync();
  const countAfterSync = fetched.length;
  const views = store.knownViews();
  assert.equal(views.length, 1);
  assert.equal(views[0].id, "R50");
  assert.equal(fetched.length, countAfterSync, "knownViews must not trigger byId fetches");
});

test("sync coalesces rapid intra-run streaming notifications into one animation frame when rAF is present", async (t) => {
  const frames = new Map();
  let nextId = 1;
  const prevRaf = globalThis.requestAnimationFrame;
  const prevCancel = globalThis.cancelAnimationFrame;
  globalThis.requestAnimationFrame = (fn) => {
    const id = nextId++;
    frames.set(id, fn);
    return id;
  };
  globalThis.cancelAnimationFrame = (id) => {
    frames.delete(id);
  };
  t.after(() => {
    globalThis.requestAnimationFrame = prevRaf;
    globalThis.cancelAnimationFrame = prevCancel;
  });

  const replies = {
    "api/run?after=0": {
      id: "R1",
      notes: ["N1"],
      prompt: "p",
      status: "running",
      total: 1,
      entries: ["a"],
    },
    "api/run?after=1": { id: "R1", status: "running", total: 2, entries: ["b"] },
    "api/run?after=2": { id: "R1", status: "running", total: 3, entries: ["c"] },
    "api/runs": { runs: ["R1"] },
  };
  t.mock.method(globalThis, "fetch", async (url) => new Response(JSON.stringify(replies[url])));
  let renders = 0;
  const changes = [];
  const store = runStore({
    render: () => {
      renders += 1;
    },
    onChange: (run, previous, meta) => {
      changes.push({
        id: run.id,
        total: run.total,
        status: run.status,
        prevTotal: previous?.total ?? null,
        meta,
      });
    },
    onLoaded() {},
    onError: (text) => assert.fail(text),
  });

  // Initial load transitions null -> R1 and flushes synchronously.
  await store.sync();
  assert.equal(renders, 1);
  assert.equal(frames.size, 0);

  // Two rapid intra-run streaming updates coalesce into one scheduled animation frame.
  await store.sync();
  await store.sync();
  assert.equal(renders, 1, "intra-run streaming updates wait for the scheduled animation frame");
  assert.equal(frames.size, 1, "two rapid streaming syncs schedule only one frame");
  const [id, fn] = frames.entries().next().value;
  frames.delete(id);
  fn();
  assert.equal(renders, 2);
  assert.deepEqual(changes, [
    { id: "R1", total: 1, status: "running", prevTotal: null, meta: { liveEnded: false } },
    { id: "R1", total: 3, status: "running", prevTotal: 1, meta: { liveEnded: false } },
  ]);
});

test("sync flushes status and run-ID transitions synchronously and cancels pending rAF", async (t) => {
  const frames = new Map();
  let nextId = 1;
  const prevRaf = globalThis.requestAnimationFrame;
  const prevCancel = globalThis.cancelAnimationFrame;
  globalThis.requestAnimationFrame = (fn) => {
    const id = nextId++;
    frames.set(id, fn);
    return id;
  };
  globalThis.cancelAnimationFrame = (id) => {
    frames.delete(id);
  };
  t.after(() => {
    globalThis.requestAnimationFrame = prevRaf;
    globalThis.cancelAnimationFrame = prevCancel;
  });

  let runReply = {
    id: "R1",
    notes: ["N1"],
    prompt: "p",
    status: "running",
    total: 1,
    entries: ["a"],
  };
  let runsList = ["R1"];
  t.mock.method(globalThis, "fetch", async (url) => {
    if (url === "api/runs") return new Response(JSON.stringify({ runs: runsList }));
    return new Response(JSON.stringify(runReply));
  });
  let renders = 0;
  const changes = [];
  const store = runStore({
    render: () => {
      renders += 1;
    },
    onChange: (run, previous, meta) => {
      changes.push({
        id: run.id,
        status: run.status,
        prevId: previous?.id ?? null,
        prevStatus: previous?.status ?? null,
        prevTotal: previous?.total ?? null,
        meta,
      });
    },
    onLoaded() {},
    onError: (text) => assert.fail(text),
  });

  // 1. Initial sync (null -> R1 running): synchronous.
  await store.sync();
  assert.equal(renders, 1);

  // 2. Intra-run streaming update (R1 running -> R1 running): deferred to rAF.
  runReply = { id: "R1", status: "running", total: 2, entries: ["b"] };
  await store.sync();
  assert.equal(renders, 1);
  assert.equal(frames.size, 1);

  // 3. Status transition (R1 running -> R1 ended): cancels pending rAF and flushes synchronously
  //    with the pending previous snapshot preserved.
  runReply = { id: "R1", status: "ended", total: 3, entries: ["c"] };
  await store.sync();
  assert.equal(frames.size, 0, "pending rAF is cancelled on status transition");
  assert.equal(renders, 2);

  // 4. Run-ID transition (R1 ended -> R2 running): flushes synchronously.
  runsList = ["R2", "R1"];
  runReply = {
    id: "R2",
    notes: ["N2"],
    prompt: "p2",
    status: "running",
    total: 1,
    entries: ["x"],
  };
  await store.sync();
  assert.equal(frames.size, 0);
  assert.equal(renders, 3);
  assert.deepEqual(changes, [
    {
      id: "R1",
      status: "running",
      prevId: null,
      prevStatus: null,
      prevTotal: null,
      meta: { liveEnded: false },
    },
    {
      id: "R1",
      status: "ended",
      prevId: "R1",
      prevStatus: "running",
      prevTotal: 1,
      meta: { liveEnded: false },
    },
    {
      id: "R2",
      status: "running",
      prevId: "R1",
      prevStatus: "ended",
      prevTotal: 3,
      meta: { liveEnded: false },
    },
  ]);
});
