// The page's end-to-end tests: the page in Playwright's WebKit, the engine closest to the app's
// WKWebView, while a run streams. `cargo test page_end_to_end -- --ignored` runs them through
// the bridge in src/site/tests/page_e2e.rs, which serves real sites with the fake agent's
// `flood` scenario (tests/fake-agent.mjs): a reply part every 20 ms until the run is stopped.
//
// What they catch, and the page's unit tests cannot: a click lost because the page replaced
// the node under the pointer while it drew the run again (docs/review-2026-11-23.md, D4).

import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { webkit } from "playwright-core";

import { isLive } from "../../ui/run.js";

const bridge = process.env.POSTIL_E2E_URL;
if (!bridge)
  throw new Error("POSTIL_E2E_URL is unset: run `cargo test page_end_to_end -- --ignored`");

// How long one wait may take before the test fails. Arbitrary: far above what a step takes on
// a loaded laptop (well under a second), so it bounds a lost click and times nothing.
const WAIT = 10_000;

// Stands in for the app's window (src/shell.rs): its IPC, which these tests do not use, and its
// call of `runChanged` whenever the run changed, here on each event the bridge sends. It also
// counts the page's draws, each one task and so one observer callback: `run`, of the Run tab,
// each of which writes the tray header's status (`renderHeader` in ui/run-tab.js); and
// `noteDialog`, of the note dialog, each of which replaces at least its body.
const init = () => {
  window.ipc = { postMessage() {} };
  new EventSource("events").onmessage = () => window.postil?.runChanged?.();
  window.e2eDraws = { run: 0, noteDialog: 0 };
  addEventListener("DOMContentLoaded", () => {
    // The clock's tick writes `.run-time` once a second; it is no draw.
    const drawn = (record) => !record.target.classList?.contains("run-time");
    const header = new MutationObserver((records) => {
      if (records.some(drawn)) window.e2eDraws.run += 1;
    });
    header.observe(document.getElementById("run-header"), { childList: true, subtree: true });
    const dialog = new MutationObserver(() => (window.e2eDraws.noteDialog += 1));
    dialog.observe(document.getElementById("note-dialog"), { childList: true });
  });
};

let browser;
before(async () => {
  browser = await webkit.launch();
});
after(() => browser?.close());

// A new project on the bridge, `src/a.rs` in a fresh repository, open in a new page. `api` calls
// its routes from the test, as the page does.
async function openProject(t) {
  const created = await fetch(new URL("new", bridge), { method: "POST" });
  const site = new URL((await created.json()).base, bridge);
  const page = await browser.newPage();
  t.after(() => page.close());
  page.setDefaultTimeout(WAIT);
  await page.addInitScript(init);
  await page.goto(site.href);
  await page.locator("#repo-name").filter({ hasText: /\S/ }).waitFor();
  const api = async (path, options) => {
    const response = await fetch(new URL(path, site), options);
    const text = await response.text();
    assert.ok(response.ok, `${path}: ${response.status} ${text}`);
    return JSON.parse(text);
  };
  return { page, api };
}

// Writes a note on the first line of src/a.rs, as the composer does, and returns it.
async function addNote(api, body) {
  const request = { path: "src/a.rs", lines: { start: 1, end: 1 }, snippet: "one", body };
  const review = await api("api/notes", { method: "POST", body: JSON.stringify(request) });
  return review.notes.find((note) => note.body === body);
}

// Sends `notes` to the agent, shows the Run tab, and waits until it shows the run streaming:
// its Stop.
async function startRun(page, api, notes) {
  await api("api/run", { method: "POST", body: JSON.stringify({ notes: notes.map((n) => n.id) }) });
  await page.locator("#tab-run").click();
  await page.locator("#run-header .stop").waitFor();
  await draws(page, 3);
}

// Waits until the page drew `what` (a key of `window.e2eDraws`) `count` more times, as that
// many updates streamed in.
async function draws(page, count, what = "run") {
  const now = await page.evaluate((key) => window.e2eDraws[key], what);
  const target = [what, now + count];
  await page.waitForFunction(([key, at]) => window.e2eDraws[key] >= at, target);
}

test("one click on Stop stops a run that streams (JS1)", async (t) => {
  const { page, api } = await openProject(t);
  await startRun(page, api, [await addNote(api, "Rename two.")]);

  // A person's click: the button is held while the run draws again, then released.
  const stop = await page.locator("#run-header .stop").boundingBox();
  await page.mouse.move(stop.x + stop.width / 2, stop.y + stop.height / 2);
  await page.mouse.down();
  await draws(page, 3);
  await page.mouse.up();

  // The header drops Stop once the run ended.
  await page.locator("#run-header .stop").waitFor({ state: "detached" });
  const run = await api("api/run");
  assert.equal(run.status, "ended");
});

test("the note dialog keeps its scroll and its two-click Delete while its run streams (JS2)", async (t) => {
  const { page, api } = await openProject(t);
  // Long enough that the dialog's body scrolls.
  const text = Array.from({ length: 80 }, (_, line) => `Line ${line + 1} of the note.`).join("\n");
  const note = await addNote(api, text);
  await startRun(page, api, [note]);
  await page.locator("#tab-notes").click();
  await page.locator("#notes .note").first().click();
  const dialog = page.locator("#note-dialog");
  await dialog.waitFor();

  // The body is found and scrolled in one task: a locator's element can be replaced by a draw
  // between Playwright finding it and acting on it.
  const scroll = (to) =>
    page.evaluate((top) => {
      const body = document.querySelector("#note-dialog .note-dialog-body");
      if (top !== null) body.scrollTop = top;
      return body.scrollTop;
    }, to);
  assert.equal(await scroll(200), 200, "the dialog's body scrolls");
  // The reply is a line about the note, so the dialog draws again on every part.
  await draws(page, 5, "noteDialog");
  assert.equal(await scroll(null), 200);

  await dialog.getByRole("button", { name: "Delete note" }).click();
  await draws(page, 5, "noteDialog");
  await dialog.getByRole("button", { name: "Click again to delete" }).click();
  await dialog.waitFor({ state: "hidden" });
  // The page sends the delete after it closed the dialog; the note's card goes once it is done.
  await page.locator("#notes .note").waitFor({ state: "detached" });
  const { notes } = await api("api/notes");
  assert.deepEqual(
    notes.map((left) => left.id),
    [],
    "the note is deleted",
  );
});

test("Esc with Changes open and the focus elsewhere closes Changes, not the run (JS5)", async (t) => {
  const { page, api } = await openProject(t);
  await startRun(page, api, [await addNote(api, "Rename two.")]);
  await page.locator("#run .run-entry").click();
  await page.locator("#run-detail").getByRole("button", { name: "Changes" }).click();
  const changes = page.locator("#changes-view");
  await changes.waitFor();

  await page.locator("#tab-notes").focus();
  await page.keyboard.press("Escape");
  await changes.waitFor({ state: "hidden" });

  // A stop would show within a few draws: the Rust process records the request at once.
  await draws(page, 5);
  const run = await api("api/run");
  assert.ok(isLive(run), `the run goes on: ${run.status}`);
  assert.ok(!run.entries.some((entry) => entry.kind === "cancelRequested"), "no stop was asked");
});
