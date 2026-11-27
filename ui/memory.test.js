import assert from "node:assert/strict";
import { mock, test } from "node:test";

import {
  MEMORY_BODY_LIMIT,
  MEMORY_POLL_MS,
  checkMemoryDraft,
  cutBody,
  isMemoryId,
  isNearDuplicateError,
  memoryAge,
  partitionPointers,
  replyProblem,
} from "./memory.js";
import { memoryDialog, rememberDialog } from "./memory-dialog.js";
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

test("checkMemoryDraft counts UTF-8 bytes, refuses blank text, and a scope with no name", () => {
  const draft = { summary: "s", body: "b", scope: "global", memoryRepo: null };
  assert.equal(checkMemoryDraft(draft), null);
  // 16 KiB of two-byte characters is at the limit; one byte more is over it.
  const atLimit = "é".repeat(8 * 1024);
  assert.equal(checkMemoryDraft({ ...draft, body: atLimit }), null);
  assert.equal(
    checkMemoryDraft({ ...draft, body: `${atLimit}a` }),
    "The memory is 16385 bytes; at most 16384 (16 KiB) are sent.",
  );
  assert.equal(
    checkMemoryDraft({ ...draft, summary: "é".repeat(513) }),
    "The summary is 1026 bytes; at most 1024 are sent.",
  );
  assert.equal(checkMemoryDraft({ ...draft, summary: " \n" }), "Write a summary.");
  assert.equal(checkMemoryDraft({ ...draft, body: "\t" }), "Write the memory.");
  assert.match(checkMemoryDraft({ ...draft, scope: "repo" }), /no origin remote/);
  assert.equal(checkMemoryDraft({ ...draft, scope: "repo", memoryRepo: "postil" }), null);
  assert.equal(isNearDuplicateError("near-duplicate of note mem_01A: retry with force"), true);
  assert.equal(isNearDuplicateError("hippius-mem exited"), false);
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
    this.value = "";
    this.attributes = new Map();
    const classes = () => new Set(this.className.split(" ").filter(Boolean));
    this.classList = {
      contains: (name) => classes().has(name),
      toggle: (name, on) => {
        const set = classes();
        if (on ?? !set.has(name)) set.add(name);
        else set.delete(name);
        this.className = [...set].join(" ");
      },
    };
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

// Runs `run` with the fake DOM and `fetch` answering each request from `answer(url, method,
// body)`, recording the requests and their bodies; setTimeout is mocked, so `mock.timers.tick`
// moves the polls on.
async function withPage(answer, run) {
  const previous = { document: globalThis.document, fetch: globalThis.fetch };
  const asked = [];
  // Beside the requests, by index, and left out of comparisons of `asked`.
  Object.defineProperty(asked, "bodies", { value: [] });
  globalThis.document = { createElement: (tag) => new FakeElement(tag) };
  globalThis.fetch = async (url, { method = "GET", body } = {}) => {
    asked.push(`${method} ${url}`);
    asked.bodies.push(body);
    const reply = answer(url, method, body);
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

// ---- The Remember dialog ---------------------------------------------------------------------

// The Remember dialog on the fake DOM, and what it told the page.
function rememberOn(dialogEl, memoryRepo = "postil") {
  const told = { opened: [], started: 0, saved: [] };
  const dialog = rememberDialog(dialogEl, {
    memoryRepo: () => memoryRepo,
    openMemory: (id) => told.opened.push(id),
    onDraftStarted: () => (told.started += 1),
    onSaved: (reply) => told.saved.push(reply),
  });
  return { dialog, told };
}

const one = (root, className) => root.find(className)[0];

function type(input, text) {
  input.value = text;
  input.dispatchEvent(new Event("input"));
}

test("Remember drafts in the thread, checks for a duplicate, and saves the text as shown", () => {
  let polls = 0;
  let saves = 0;
  return withPage(
    (url, method) => {
      if (url === "api/memory/draft") return { kind: "run", run: "R2", fallback: "keep it short" };
      if (url.startsWith("api/run?")) {
        polls += 1;
        return polls === 1
          ? { total: 3, status: "running" }
          : { total: 5, draft: { state: "ready", summary: "a.txt is one line", body: "Why." } };
      }
      if (url.startsWith("api/memory/recall")) return { state: "ready", pointers: [strong] };
      assert.equal(`${method} ${url}`, "POST api/memory/remember");
      saves += 1;
      return saves === 1
        ? { state: "pending" }
        : { state: "ready", id: "mem_01NEW", repo: "postil" };
    },
    async (asked) => {
      const dialogEl = new FakeElement("dialog");
      const { dialog, told } = rememberOn(dialogEl);
      dialog.open({ note: "N1", where: "N1 · a.txt:1", body: "keep it short" });
      assert.equal(dialogEl.open, true);
      assert.ok(dialogEl.text.includes("Drafting from the thread…"));
      await settle();
      assert.equal(told.started, 1, "the runs are told a draft started");
      mock.timers.tick(MEMORY_POLL_MS);
      await settle();
      assert.deepEqual(asked.slice(0, 3), [
        "POST api/memory/draft",
        "GET api/run?id=R2&after=0",
        "GET api/run?id=R2&after=3",
      ]);
      const summary = one(dialogEl, "remember-summary");
      const body = one(dialogEl, "remember-body");
      assert.equal(summary.value, "a.txt is one line");
      assert.equal(body.value, "Why.");
      assert.ok(asked.includes("GET api/memory/recall?text=a.txt%20is%20one%20line"));
      // A duplicate's card opens it over the draft, which stays.
      one(dialogEl, "memory-card").click();
      assert.deepEqual(told.opened, ["mem_01A"]);
      assert.equal(dialogEl.open, true);

      // The check runs again once the summary rests.
      type(summary, "  a.txt is one line, on purpose ");
      assert.equal(asked.filter((line) => line.includes("recall")).length, 1);
      mock.timers.tick(300);
      await settle();
      assert.equal(asked.filter((line) => line.includes("recall")).length, 2);

      // Two clicks write once; the text goes untrimmed, with nothing added.
      one(dialogEl, "remember-save").click();
      one(dialogEl, "remember-save").click();
      await settle();
      const posts = () => asked.filter((line) => line === "POST api/memory/remember");
      assert.equal(posts().length, 1);
      assert.equal(one(dialogEl, "remember-save").disabled, true);
      const sent = JSON.parse(asked.bodies[asked.indexOf("POST api/memory/remember")]);
      assert.deepEqual(sent, {
        summary: "  a.txt is one line, on purpose ",
        body: "Why.",
        scope: "repo",
        noteType: "gotcha",
        note: "N1",
      });
      mock.timers.tick(MEMORY_POLL_MS);
      await settle();
      assert.equal(posts().length, 2, "asked again while pending");
      assert.deepEqual(told.saved, [{ state: "ready", id: "mem_01NEW", repo: "postil" }]);
      assert.equal(dialogEl.open, false);
    },
  );
});

test("a check that cannot run leaves Save, and a near-duplicate offers Save anyway", () => {
  let saves = 0;
  return withPage(
    (url) => {
      if (url === "api/memory/draft") return { kind: "fallback", body: "never sent" };
      if (url.startsWith("api/memory/recall")) {
        return {
          state: "unavailable",
          reason: "missing",
          message: "hippius-mem is not installed on PATH",
        };
      }
      saves += 1;
      return saves === 1
        ? { state: "failed", error: "near-duplicate of note mem_01OLD: retry with force" }
        : { state: "ready", id: "mem_01NEW", repo: "global" };
    },
    async (asked) => {
      const dialogEl = new FakeElement("dialog");
      // No origin remote: the whole team's memory is the only scope.
      const { dialog, told } = rememberOn(dialogEl, null);
      dialog.open({ note: "N1", where: "N1 · a.txt:1", body: "never sent" });
      await settle();
      assert.equal(one(dialogEl, "remember-body").value, "never sent");
      assert.ok(dialogEl.text.includes("A summary is checked once written."));
      const summary = one(dialogEl, "remember-summary");
      type(summary, "a.txt is one line");
      mock.timers.tick(300);
      await settle();
      assert.ok(
        dialogEl.text.includes("Check did not run: hippius-mem is not installed on PATH."),
        dialogEl.text,
      );

      const save = one(dialogEl, "remember-save");
      const anyway = one(dialogEl, "remember-anyway");
      assert.equal(anyway.hidden, true);
      save.click();
      await settle();
      assert.ok(dialogEl.text.includes("Not saved: near-duplicate of note mem_01OLD"));
      assert.equal(dialogEl.open, true, "the text stays");
      assert.equal(summary.value, "a.txt is one line");
      assert.equal(save.disabled, false);
      assert.equal(anyway.hidden, false);

      anyway.click();
      await settle();
      const bodies = asked
        .map((line, index) => [line, asked.bodies[index]])
        .filter(([line]) => line === "POST api/memory/remember")
        .map(([, body]) => JSON.parse(body));
      assert.deepEqual(
        bodies.map((body) => [body.scope, body.force]),
        [
          ["global", undefined],
          ["global", true],
        ],
      );
      assert.equal(told.saved.length, 1);
    },
  );
});

test("a draft that fails falls back to the note's text, and closing early stops the draft", () => {
  let draft = { state: "failed", problem: "the agent's reply holds no draft: no block" };
  return withPage(
    (url) => {
      if (url === "api/memory/draft") return { kind: "run", run: "R2", fallback: "keep it short" };
      if (url.startsWith("api/run?")) return draft ? { total: 1, draft } : { total: 1 };
      if (url.startsWith("api/memory/recall")) return { state: "ready", pointers: [] };
      return {};
    },
    async (asked) => {
      const dialogEl = new FakeElement("dialog");
      const { dialog } = rememberOn(dialogEl);
      dialog.open({ note: "N1", where: "N1 · a.txt:1", body: "keep it short" });
      await settle();
      assert.equal(one(dialogEl, "remember-body").value, "keep it short");
      assert.equal(
        one(dialogEl, "run-notice").textContent,
        "No draft: the agent's reply holds no draft: no block",
      );
      // Text in the dialog holds a quit until it is saved or cancelled.
      assert.equal(dialog.holdDraft("quitting"), true);
      assert.ok(dialogEl.text.includes("Save or cancel this memory before quitting."));
      dialogEl.close();
      assert.equal(dialog.holdDraft("quitting"), false);

      // Closed while the agent drafts: its run is stopped, and no poll follows.
      draft = null;
      dialog.open({ note: "N1", where: "N1 · a.txt:1", body: "keep it short" });
      await settle();
      dialogEl.close();
      await settle();
      const cancel = asked.indexOf("POST api/run/cancel");
      assert.notEqual(cancel, -1, asked.join("\n"));
      assert.deepEqual(JSON.parse(asked.bodies[cancel]), { run: "R2" });
      const before = asked.length;
      mock.timers.tick(MEMORY_POLL_MS * 4);
      await settle();
      assert.equal(asked.length, before);
    },
  );
});

test("closing before the draft's start is answered stops the run it started", () =>
  withPage(
    (url) =>
      url === "api/memory/draft" ? { kind: "run", run: "R2", fallback: "keep it short" } : {},
    async (asked) => {
      const dialogEl = new FakeElement("dialog");
      const { dialog, told } = rememberOn(dialogEl);
      dialog.open({ note: "N1", where: "N1 · a.txt:1", body: "keep it short" });
      dialogEl.close();
      await settle();
      const cancel = asked.indexOf("POST api/run/cancel");
      assert.notEqual(cancel, -1, asked.join("\n"));
      assert.deepEqual(JSON.parse(asked.bodies[cancel]), { run: "R2" });
      assert.equal(told.started, 0);
      assert.ok(!asked.some((line) => line.startsWith("GET api/run?")), "nothing follows it");
    },
  ));
