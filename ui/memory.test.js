import assert from "node:assert/strict";
import { mock, test } from "node:test";

import {
  MEMORY_BODY_LIMIT,
  MEMORY_POLL_MS,
  cutBody,
  isMemoryId,
  memoryAge,
  partitionPointers,
  replyProblem,
} from "./memory.js";
import { memoryDialog } from "./memory-dialog.js";
import { memoryTab, runMemory } from "./memory-tab.js";

const strong = {
  id: "mem_01A",
  summary: "a.rs parses lazily",
  weak: false,
  author: "5CrAuthorAddressXYZ",
  updated: 0,
};
const weak = {
  id: "mem_01B",
  summary: "maybe related",
  weak: true,
  author: "5CrOther",
  updated: 0,
};

test("partitionPointers: nothing strong is nothing recorded, and the guesses stay", () => {
  assert.deepEqual(partitionPointers([strong, weak]), {
    strong: [strong],
    weak: [weak],
    empty: false,
  });
  assert.deepEqual(partitionPointers([weak]), { strong: [], weak: [weak], empty: true });
  assert.deepEqual(partitionPointers([]), { strong: [], weak: [], empty: true });
});

test("cutBody cuts at 64 KiB of UTF-8, on a character boundary", () => {
  const exact = "a".repeat(MEMORY_BODY_LIMIT);
  assert.deepEqual(cutBody(exact), { text: exact, cut: false });
  assert.deepEqual(cutBody(`${exact}b`), { text: exact, cut: true });

  // A four-byte character (two UTF-16 units) across the limit goes whole.
  const emoji = `${"a".repeat(MEMORY_BODY_LIMIT - 2)}😀`;
  assert.deepEqual(cutBody(emoji), { text: "a".repeat(MEMORY_BODY_LIMIT - 2), cut: true });
  // Counted in bytes, not UTF-16 units: 40 000 two-byte characters are 80 000 bytes.
  const { text, cut } = cutBody("é".repeat(40_000));
  assert.equal(cut, true);
  assert.equal(text, "é".repeat(MEMORY_BODY_LIMIT / 2));
  assert.ok(!text.includes("�"));
});

test("memoryAge reads hippius-mem's milliseconds", () => {
  const updated = 1_795_733_249_727;
  assert.equal(memoryAge(updated, updated + 30_000), "now");
  assert.equal(memoryAge(updated, updated + 90_000), "1m");
  assert.equal(memoryAge(updated, updated + 3 * 3_600_000), "3h");
  assert.equal(memoryAge(updated, updated + 4 * 86_400_000 + 5), "4d");
  // A clock behind the teammate's.
  assert.equal(memoryAge(updated, updated - 60_000), "now");
});

test("isMemoryId takes mem_ and digits or capital letters only", () => {
  assert.equal(isMemoryId("mem_01M8D0NQNZTAF6C8A814QMSFFD"), true);
  for (const id of ["mem_", "mem_01a", "mem_01A\n", "../mem_01A", "N1", "", null]) {
    assert.equal(isMemoryId(id), false, String(id));
  }
});

test("replyProblem names both versions and the missing tools", () => {
  const reply = {
    state: "incompatible",
    serverVersion: "0.6.0",
    postilVersion: "0.1.0",
    missing: ["history"],
  };
  assert.equal(
    replyProblem(reply),
    "This hippius-mem (0.6.0) does not match Postil 0.1.0: it lacks history.",
  );
  assert.equal(replyProblem({ state: "pending" }), null);
});

// ---- The views, on a few fake DOM nodes ------------------------------------------------------

class FakeElement extends EventTarget {
  constructor(tag) {
    super();
    this.tagName = tag.toUpperCase();
    this.className = "";
    this.textContent = "";
    this.children = [];
    this.hidden = false;
    this.disabled = false;
    this.open = false;
    this.attributes = new Map();
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  append(...items) {
    for (const item of items) {
      if (typeof item === "string") {
        const text = new FakeElement("#text");
        text.textContent = item;
        this.children.push(text);
      } else {
        this.children.push(item);
      }
    }
  }

  replaceChildren(...items) {
    this.children = [];
    this.append(...items);
  }

  showModal() {
    this.open = true;
  }

  close() {
    this.open = false;
    this.dispatchEvent(new Event("close"));
  }

  focus() {}

  click() {
    this.dispatchEvent(new Event("click"));
  }

  // Every node below this one, in order.
  *nodes() {
    for (const child of this.children) {
      yield child;
      yield* child.nodes();
    }
  }

  find(className) {
    return [...this.nodes()].filter((node) => node.className.split(" ").includes(className));
  }

  // The text of this node and every node below it.
  get text() {
    return [this.textContent, ...[...this.nodes()].map((node) => node.textContent)].join("");
  }
}

// Runs `run` with the fake DOM and `fetch` answering each request from `answer(url, method)`,
// recording the requests; setTimeout is mocked, so `mock.timers.tick` moves the polls on.
async function withPage(answer, run) {
  const previous = { document: globalThis.document, fetch: globalThis.fetch };
  const asked = [];
  globalThis.document = { createElement: (tag) => new FakeElement(tag) };
  globalThis.fetch = async (url, { method = "GET" } = {}) => {
    asked.push(`${method} ${url}`);
    const reply = answer(url, method);
    return { ok: true, json: async () => reply };
  };
  mock.timers.enable({ apis: ["setTimeout"] });
  try {
    await run(asked);
  } finally {
    mock.timers.reset();
    globalThis.document = previous.document;
    globalThis.fetch = previous.fetch;
  }
}

// Lets the replies of the requests made so far arrive.
async function settle() {
  for (let i = 0; i < 10; i += 1) await new Promise((resolve) => setImmediate(resolve));
}

test("a memory's body is drawn as text, never HTML, and cut at 64 KiB", () =>
  withPage(
    (url) => ({
      state: "ready",
      id: "mem_01A",
      noteType: "gotcha",
      author: "5CrAuthor",
      created: 0,
      updated: 0,
      tags: [],
      summary: "a.rs parses lazily",
      body: url.includes("mem_01BIG")
        ? "x".repeat(MEMORY_BODY_LIMIT + 1)
        : "<img src=x onerror=alert(1)> [click](javascript:alert(1)) <script>x</script>",
      history: [{ kind: "Remember", author: "5CrAuthor", anchor: "local" }],
    }),
    async (asked) => {
      const dialogEl = new FakeElement("dialog");
      const dialog = memoryDialog(dialogEl);

      dialog.open("mem_bad");
      assert.equal(dialogEl.open, false);
      assert.deepEqual(asked, [], "a bad id is never sent");

      dialog.open("mem_01A");
      assert.equal(dialogEl.open, true);
      assert.ok(dialogEl.text.includes("Loading…"));
      await settle();
      assert.deepEqual(asked, ["GET api/memory?id=mem_01A"]);
      const tags = new Set([...dialogEl.nodes()].map((node) => node.tagName));
      for (const tag of ["IMG", "A", "SCRIPT"]) assert.ok(!tags.has(tag), `no ${tag}`);
      assert.ok(dialogEl.text.includes("<img src=x onerror=alert(1)>"), "shown as text");
      assert.ok(dialogEl.text.includes("Remember · 5CrAuthor · anchored in the vault"));
      assert.ok(!dialogEl.text.includes("Cut at"));

      dialog.open("mem_01BIG");
      await settle();
      const shown = dialogEl.find("run-message")[0].text;
      assert.equal(shown.length, MEMORY_BODY_LIMIT);
      assert.ok(dialogEl.text.includes("Cut at 64 KiB."));
    },
  ));

test("a memory's dialog is not drawn again while its answer is pending", () =>
  withPage(
    () => ({ state: "pending" }),
    async (asked) => {
      const dialogEl = new FakeElement("dialog");
      memoryDialog(dialogEl).open("mem_01A");
      const [header] = dialogEl.children;
      await settle();
      mock.timers.tick(MEMORY_POLL_MS);
      await settle();
      assert.equal(asked.length, 2, "asked again");
      assert.equal(dialogEl.children[0], header, "the header with its esc stays the same node");
    },
  ));

test("the Memory tab recalls only while it shows, and keeps each file's answer", () => {
  let polls = 0;
  return withPage(
    () => {
      polls += 1;
      return polls === 1 ? { state: "pending" } : { state: "ready", pointers: [strong, weak] };
    },
    async (asked) => {
      let file = { path: "src/café notes.rs", node: null };
      const panelEl = new FakeElement("div");
      const tab = memoryTab({
        panelEl,
        headerEl: new FakeElement("div"),
        currentFile: () => file,
        openMemory: () => {},
        openSettings: () => {},
      });

      tab.fileOpened();
      await settle();
      assert.deepEqual(asked, [], "no recall while the tab is closed");

      tab.show();
      await settle();
      assert.deepEqual(asked, ["GET api/memory/recall?path=src%2Fcaf%C3%A9%20notes.rs"]);
      assert.equal(panelEl.text, "Checking team memory…");
      mock.timers.tick(MEMORY_POLL_MS);
      await settle();
      assert.equal(asked.length, 2, "asked again while pending");
      const [card] = panelEl.children.filter((node) => node.className.includes("memory-card"));
      assert.ok(card.text.includes("a.rs parses lazily"));
      const [group] = panelEl.find("memory-weak");
      assert.ok(group.text.startsWith("Possibly related"));
      assert.ok(group.text.includes("maybe related"));

      // Kept: showing the tab again asks nothing.
      tab.hide();
      tab.show();
      await settle();
      assert.equal(asked.length, 2);

      // Hidden while a recall is pending: it stops asking.
      file = { path: "b.rs", node: null };
      polls = 0;
      tab.fileOpened();
      await settle();
      assert.equal(asked.length, 3);
      tab.hide();
      mock.timers.tick(MEMORY_POLL_MS * 4);
      await settle();
      assert.equal(asked.length, 3, "no poll once hidden");
    },
  );
});

test("Refresh pulls, asks again while pending, then drops the kept answer and recalls", () => {
  let refreshes = 0;
  return withPage(
    (url) => {
      if (!url.startsWith("api/memory/refresh")) return { state: "ready", pointers: [strong] };
      refreshes += 1;
      return refreshes === 1 ? { state: "pending" } : { state: "ready", indexed: 3 };
    },
    async (asked) => {
      const panelEl = new FakeElement("div");
      const headerEl = new FakeElement("div");
      const tab = memoryTab({
        panelEl,
        headerEl,
        currentFile: () => ({ path: "a.rs", node: null }),
        openMemory: () => {},
        openSettings: () => {},
      });
      tab.show();
      await settle();
      assert.deepEqual(asked, ["GET api/memory/recall?path=a.rs"]);

      const [button] = headerEl.find("memory-refresh");
      const [status] = headerEl.find("memory-status");
      button.click();
      assert.equal(button.disabled, true);
      assert.equal(status.textContent, "Refreshing…");
      // A second click while it runs asks nothing more.
      button.click();
      await settle();
      assert.deepEqual(asked.slice(1), ["POST api/memory/refresh"]);

      mock.timers.tick(MEMORY_POLL_MS);
      await settle();
      assert.deepEqual(asked.slice(1), [
        "POST api/memory/refresh",
        "POST api/memory/refresh",
        "GET api/memory/recall?path=a.rs",
      ]);
      assert.equal(button.disabled, false);
      assert.equal(status.textContent, "");
    },
  );
});

test("a recall of guesses only says nothing is recorded, with the guesses dimmed below", () =>
  withPage(
    () => ({ state: "ready", pointers: [weak] }),
    async () => {
      const panelEl = new FakeElement("div");
      const tab = memoryTab({
        panelEl,
        headerEl: new FakeElement("div"),
        currentFile: () => ({ path: "a.rs", node: null }),
        openMemory: () => {},
        openSettings: () => {},
      });
      tab.show();
      await settle();
      assert.equal(panelEl.children[0].textContent, "Nothing recorded for this file.");
      assert.equal(panelEl.find("memory-weak").length, 1);
    },
  ));

test("the run dialog's Memory section is left out when hippius-mem cannot run", () =>
  withPage(
    (url) =>
      url.endsWith("R1")
        ? {
            state: "unavailable",
            reason: "missing",
            message: "hippius-mem is not installed on PATH",
          }
        : { state: "ready", pointers: [] },
    async (asked) => {
      let settled = 0;
      const memory = runMemory({ openMemory: () => {}, onSettled: () => (settled += 1) });
      assert.equal(memory.key("R1"), "pending");
      assert.equal(memory.key("R2"), "pending");
      await settle();
      assert.equal(settled, 2);
      assert.deepEqual(asked, ["GET api/memory/recall?run=R1", "GET api/memory/recall?run=R2"]);
      assert.equal(memory.view("R1"), null);
      assert.equal(memory.key("R2"), "ready");
      assert.equal(memory.view("R2")[0].textContent, "Nothing recorded for this run.");
      assert.equal(asked.length, 2, "answers are kept while the dialog is open");
    },
  ));
