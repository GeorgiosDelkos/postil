import assert from "node:assert/strict";
import test from "node:test";

import { pathNoteDialog } from "./path-note-dialog.js";

// Minimal DOM nodes so `pathNoteDialog` runs under `node --test` without a browser.
class FakeElement extends EventTarget {
  constructor(tag) {
    super();
    this.tagName = tag.toUpperCase();
    this.className = "";
    this.textContent = "";
    this.value = "";
    this.attributes = new Map();
    this.children = [];
    this.open = false;
    this.focused = false;
    this.classList = {
      add: (...names) => {
        const set = new Set(this.className.split(/\s+/).filter(Boolean));
        for (const name of names) set.add(name);
        this.className = [...set].join(" ");
      },
      contains: (name) => this.className.split(/\s+/).includes(name),
    };
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  append(...items) {
    for (const item of items) {
      if (typeof item === "string") {
        const span = new FakeElement("#text");
        span.textContent = item;
        this.children.push(span);
      } else {
        this.children.push(item);
      }
    }
  }

  replaceChildren(...items) {
    this.children = [];
    this.append(...items);
  }

  focus() {
    this.focused = true;
  }

  click() {
    this.dispatchEvent(new Event("click"));
  }

  showModal() {
    this.open = true;
  }

  close() {
    if (!this.open) return;
    this.open = false;
    this.dispatchEvent(new Event("close"));
  }

  querySelector(selector) {
    return this.querySelectorAll(selector)[0] ?? null;
  }

  querySelectorAll(selector) {
    const matches = [];
    const wanted = selector.startsWith(".") ? selector.slice(1) : null;
    const visit = (node) => {
      if (wanted && node.classList?.contains(wanted)) matches.push(node);
      for (const child of node.children ?? []) visit(child);
    };
    for (const child of this.children) visit(child);
    return matches;
  }
}

function withFakeDom(run) {
  const previous = globalThis.document;
  globalThis.document = {
    createElement: (tag) => new FakeElement(tag),
    createElementNS: (_ns, tag) => new FakeElement(tag),
  };
  return Promise.resolve()
    .then(run)
    .finally(() => {
      globalThis.document = previous;
    });
}

function metaEnter(dialogEl, { shiftKey = false } = {}) {
  let prevented = false;
  const event = Object.assign(new Event("keydown"), {
    key: "Enter",
    metaKey: true,
    shiftKey,
    preventDefault: () => {
      prevented = true;
    },
  });
  dialogEl.dispatchEvent(event);
  return prevented;
}

test("pathNoteDialog offers Discard and Save note, and Cmd-Enter saves without sending", () =>
  withFakeDom(async () => {
    const saved = [];
    let sendOpened = 0;
    const dialogEl = new FakeElement("dialog");
    const dialog = pathNoteDialog(dialogEl, {
      add: async (path, target, body) => {
        saved.push([path, target, body]);
        return null;
      },
      openSend: () => {
        sendOpened += 1;
      },
    });

    dialog.open("src/acp", "folder");
    assert.equal(dialogEl.open, true);
    assert.equal(dialogEl.querySelector(".path-note-send"), null);
    assert.ok(dialogEl.querySelector(".path-note-discard"));
    assert.ok(dialogEl.querySelector(".path-note-save"));

    const input = dialogEl.querySelector(".path-note-input");
    dialogEl.querySelector(".path-note-save").click();
    assert.equal(dialogEl.querySelector(".note-dialog-why").textContent, "Write the note first.");
    assert.equal(dialogEl.open, true);

    input.value = "  Split by message kind.  ";
    assert.equal(metaEnter(dialogEl), true);
    await new Promise((resolve) => setTimeout(resolve, 0));

    assert.deepEqual(saved, [["src/acp", "folder", "Split by message kind."]]);
    assert.equal(dialogEl.open, false);
    assert.equal(sendOpened, 0);
  }));

test("Discard closes without storing, and Cmd-Shift-Enter holds a draft or opens Send", () =>
  withFakeDom(async () => {
    const saved = [];
    let sendOpened = 0;
    const dialogEl = new FakeElement("dialog");
    const dialog = pathNoteDialog(dialogEl, {
      add: async (...args) => {
        saved.push(args);
        return null;
      },
      openSend: () => {
        sendOpened += 1;
      },
    });

    dialog.open("README.md", "file");
    const input = dialogEl.querySelector(".path-note-input");
    input.value = "Document flags.";

    // With a draft, Cmd-Shift-Enter asks to save or discard first, like the inline composer.
    metaEnter(dialogEl, { shiftKey: true });
    assert.equal(dialogEl.open, true);
    assert.equal(sendOpened, 0);
    assert.match(
      dialogEl.querySelector(".note-dialog-why").textContent,
      /Save or discard this note before sending\./,
    );

    dialogEl.querySelector(".path-note-discard").click();
    assert.equal(dialogEl.open, false);
    assert.deepEqual(saved, []);

    // With an empty draft, Cmd-Shift-Enter closes the dialog and opens the send step.
    dialog.open("README.md", "file");
    metaEnter(dialogEl, { shiftKey: true });
    assert.equal(dialogEl.open, false);
    assert.equal(sendOpened, 1);
  }));

test("a save error keeps the dialog open with the draft and shows the reason", () =>
  withFakeDom(async () => {
    const dialogEl = new FakeElement("dialog");
    const dialog = pathNoteDialog(dialogEl, {
      add: async () => "src/gone.rs is not a file in the repository",
    });

    dialog.open("src/gone.rs", "file");
    const input = dialogEl.querySelector(".path-note-input");
    input.value = "Remove dead code.";
    dialogEl.querySelector(".path-note-save").click();
    await new Promise((resolve) => setTimeout(resolve, 0));

    assert.equal(dialogEl.open, true);
    assert.equal(input.value, "Remove dead code.");
    assert.equal(
      dialogEl.querySelector(".note-dialog-why").textContent,
      "src/gone.rs is not a file in the repository",
    );
  }));
