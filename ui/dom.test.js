import assert from "node:assert/strict";
import test from "node:test";

import { actOnSecondClick } from "./dom.js";

// A button and its area as plain event targets, and a log of what the helper called.
function setUp({ withArea }) {
  const button = new EventTarget();
  const area = withArea ? new EventTarget() : null;
  const calls = [];
  actOnSecondClick(button, {
    area,
    arm: () => calls.push("arm"),
    disarm: () => calls.push("disarm"),
    act: () => calls.push("act"),
  });
  const fire = (target, type) => target.dispatchEvent(new Event(type));
  return { button, area, calls, fire };
}

test("the first click arms and the second acts", () => {
  const { button, calls, fire } = setUp({ withArea: false });
  fire(button, "click");
  assert.deepEqual(calls, ["arm"]);
  fire(button, "click");
  assert.deepEqual(calls, ["arm", "act"]);
});

test("losing the focus between the clicks starts over", () => {
  const { button, calls, fire } = setUp({ withArea: false });
  fire(button, "click");
  fire(button, "blur");
  fire(button, "click");
  assert.deepEqual(calls, ["arm", "disarm", "arm"]);
});

test("leaving the area between the clicks starts over", () => {
  const { button, area, calls, fire } = setUp({ withArea: true });
  fire(button, "click");
  fire(area, "mouseleave");
  fire(button, "click");
  assert.deepEqual(calls, ["arm", "disarm", "arm"]);
});

test("without an area, the pointer leaving the button keeps it armed", () => {
  const { button, calls, fire } = setUp({ withArea: false });
  fire(button, "click");
  fire(button, "mouseleave");
  fire(button, "click");
  assert.deepEqual(calls, ["arm", "act"]);
});
