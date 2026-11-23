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
