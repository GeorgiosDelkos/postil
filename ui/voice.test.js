import assert from "node:assert/strict";
import { test } from "node:test";

import { command, normalize } from "./voice.js";

const FULL_CONTEXT = {
  projectOpen: true,
  changesOpen: true,
  hasChanges: true,
  hasTarget: true,
  composerOpen: true,
  noteDialogOpen: true,
  sendOpen: false,
  canSend: true,
  liveRun: true,
};

test("command matches every supported voice command with full context", () => {
  assert.deepEqual(command("next", FULL_CONTEXT), { type: "next", label: "Next" });
  assert.deepEqual(command("previous", FULL_CONTEXT), { type: "previous", label: "Previous" });
  assert.deepEqual(command("note check the lock", FULL_CONTEXT), {
    type: "note",
    text: "check the lock",
    label: "Note",
  });
  assert.deepEqual(command("ask why is this unsafe", FULL_CONTEXT), {
    type: "ask",
    text: "why is this unsafe",
    label: "Ask",
  });
  assert.deepEqual(command("save", FULL_CONTEXT), { type: "save", label: "Save" });
  assert.deepEqual(command("open dictation", FULL_CONTEXT), {
    type: "open",
    query: "dictation",
    label: "Open",
  });
  assert.deepEqual(command("go to Rollover", FULL_CONTEXT), {
    type: "goto",
    name: "Rollover",
    label: "Go to",
  });
  assert.deepEqual(command("show map", FULL_CONTEXT), { type: "show-map", label: "Show map" });
  assert.deepEqual(command("show code", FULL_CONTEXT), { type: "show-code", label: "Show code" });
  assert.deepEqual(command("show changes", FULL_CONTEXT), {
    type: "show-changes",
    label: "Show changes",
  });
  assert.deepEqual(command("resolve", FULL_CONTEXT), { type: "resolve", label: "Resolve" });
  assert.deepEqual(command("send", FULL_CONTEXT), {
    type: "send",
    confirm: false,
    label: "Open send",
  });
  assert.deepEqual(command("send", { ...FULL_CONTEXT, sendOpen: true }), {
    type: "send",
    confirm: true,
    label: "Send notes",
  });
  assert.deepEqual(command("stop", FULL_CONTEXT), { type: "stop", label: "Stop run" });
});

test("normalization handles case, punctuation, and number words", () => {
  assert.equal(normalize("  Next!  "), "next");
  assert.equal(normalize("Open, stage two."), "open stage 2");
  assert.equal(normalize("Go to: item ten?"), "go to item 10");

  assert.deepEqual(command("NEXT!", FULL_CONTEXT), { type: "next", label: "Next" });
  assert.deepEqual(command("Show, Map.", FULL_CONTEXT), {
    type: "show-map",
    label: "Show map",
  });
  assert.deepEqual(command("Open step two", FULL_CONTEXT), {
    type: "open",
    query: "step 2",
    label: "Open",
  });
  assert.deepEqual(command("Note: fix the race.", FULL_CONTEXT), {
    type: "note",
    text: "fix the race.",
    label: "Note",
  });
  assert.deepEqual(command("Note:check the lock", FULL_CONTEXT), {
    type: "note",
    text: "check the lock",
    label: "Note",
  });
  assert.deepEqual(command("Go to Rollover.", FULL_CONTEXT), {
    type: "goto",
    name: "Rollover",
    label: "Go to",
  });
});

test("modal dialogs block background commands while still allowing resolve", () => {
  const modal = { ...FULL_CONTEXT, dialogOpen: true };
  assert.deepEqual(command("resolve", modal), { type: "resolve", label: "Resolve" });
  assert.deepEqual(command("send", modal), { type: "noop", reason: "Close the dialog first." });
  assert.deepEqual(command("stop", modal), { type: "noop", reason: "Close the dialog first." });
  assert.deepEqual(command("next", modal), { type: "noop", reason: "Close the dialog first." });
});

test("utterances outside the grammar or destructive actions do not match", () => {
  assert.equal(command("what is next", FULL_CONTEXT), null);
  assert.equal(command("please send the notes now", FULL_CONTEXT), null);
  assert.equal(command("apply", FULL_CONTEXT), null);
  assert.equal(command("discard", FULL_CONTEXT), null);
  assert.equal(command("delete note", FULL_CONTEXT), null);
  assert.equal(command("remember this", FULL_CONTEXT), null);
  assert.equal(command("", FULL_CONTEXT), null);
});

test("every command returns a noop with a reason when context is empty", () => {
  const empty = {};
  const cases = [
    ["next", "Nothing to step through."],
    ["previous", "Nothing to step through."],
    ["note check this", "Nothing selected for a note."],
    ["ask why", "Nothing selected to ask about."],
    ["save", "Nothing to save."],
    ["open foo", "No project open."],
    ["go to foo", "No project open."],
    ["show map", "No project open."],
    ["show code", "No project open."],
    ["show changes", "No changes to show."],
    ["resolve", "Nothing to resolve."],
    ["send", "Nothing to send."],
    ["stop", "No run to stop."],
  ];
  for (const [utterance, reason] of cases) {
    assert.deepEqual(command(utterance, empty), { type: "noop", reason }, utterance);
  }
});
