// The page's end-to-end tests: the page in Playwright's WebKit, the engine closest to the app's
// WKWebView, while a run streams. `cargo test page_end_to_end -- --ignored` runs them through
// the bridge in src/site/tests/page_e2e.rs, which serves real sites with the fake agent's
// `flood` scenario (tests/fake-agent.mjs): a reply part every 20 ms until the run is stopped.
//
// What they catch, and the page's unit tests cannot: a click lost because the page replaced
// the node under the pointer while it drew the run again (docs/review-2026-11-23.md, D4).

import assert from "node:assert/strict";
import { unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
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

// A new project on the bridge, `src/a.rs`, `b.rs` and `c.rs` in a fresh repository (or `files`,
// a map of path to text, instead), open in a new page, its agent in the fake agent's
// `scenario`. `api` calls its routes from the test, as the page does. `route` is the bridge's
// route that makes the site (src/site/tests/page_e2e.rs).
async function openProject(t, scenario = "flood", route = "new", { files } = {}) {
  const body = JSON.stringify({ scenario, files });
  const created = await fetch(new URL(route, bridge), { method: "POST", body });
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

test("a click on a line number in Changes opens the composer on that line (JS19)", async (t) => {
  const { page, api } = await openProject(t);
  await startRun(page, api, [await addNote(api, "Rename two.")]);
  await page.locator("#run .run-entry").click();
  await page.locator("#run-detail").getByRole("button", { name: "Changes" }).click();

  // The flood's edit, `two` to `2` on line 2 of src/a.rs.
  await page.locator("#changes-view").getByRole("button", { name: "2", exact: true }).click();
  await page.locator("#changes-view").waitFor({ state: "hidden" });
  await page.locator("#code-rows label").filter({ hasText: "Note on line 2" }).waitFor();
});

// The fake agent's `report` run, ended, with its changes open in Walkthrough order: it edits
// a.txt (step 1, routine), deletes b.txt (step 2, mechanical, so folded), and names a.txt's
// line 2 again (step 3, shown in step 1).
async function openWalkthrough(t) {
  const files = { "a.txt": "a\n", "b.txt": "1\n2\n3\n4\n" };
  const { page, api } = await openProject(t, "report", "new", { files });
  const request = { path: "a.txt", lines: { start: 1, end: 1 }, snippet: "a", body: "Change a." };
  const review = await api("api/notes", { method: "POST", body: JSON.stringify(request) });
  const notes = review.notes.map((note) => note.id);
  await api("api/run", { method: "POST", body: JSON.stringify({ notes }) });
  await page.locator("#tab-run").click();
  await page.locator("#run .run-entry").click();
  const detail = page.locator("#run-detail");
  // The report's check is listed once the page holds the run's report.
  await detail.locator(".run-check").filter({ hasText: "cargo test" }).waitFor();
  await detail.getByRole("button", { name: "Changes" }).click();
  const changes = page.locator("#changes-view");
  await changes.locator(".changes-step").first().waitFor();
  return { page, changes };
}

test("n moves to the walkthrough's next step, and still does after a click on Walkthrough", async (t) => {
  const { page, changes } = await openWalkthrough(t);
  const current = changes.locator('.changes-step[aria-current="step"] .changes-step-number');
  await current.filter({ hasText: "Step 1 of 3" }).waitFor();
  await page.keyboard.press("n");
  await current.filter({ hasText: "Step 2 of 3" }).waitFor();
  await page.keyboard.press("p");
  await current.filter({ hasText: "Step 1 of 3" }).waitFor();
  // p at the first step, and n with a modifier held, do nothing.
  await page.keyboard.press("p");
  await page.keyboard.press("Control+n");
  assert.equal(await current.textContent(), "Step 1 of 3");

  // The control is in the pane's header, outside the view: it gives the keys back to it.
  await page.locator("#file-meta").getByRole("radio", { name: "Walkthrough" }).click();
  await current.filter({ hasText: "Step 1 of 3" }).waitFor();
  await page.keyboard.press("n");
  await current.filter({ hasText: "Step 2 of 3" }).waitFor();
});

test("a mechanical hunk starts folded and opens on a click", async (t) => {
  const { changes } = await openWalkthrough(t);
  const deleted = changes.locator(".changes-step").filter({ hasText: "Step 2 of 3" });
  assert.equal(await deleted.locator(".changes-line").count(), 0, "folded");
  await deleted.locator(".changes-fold").click();
  await deleted.locator(".changes-line.del").nth(3).waitFor();
  assert.equal(await deleted.locator(".changes-fold").count(), 0);
});

test("a click on a line number in a walkthrough's hunk opens the composer on that line", async (t) => {
  const { page, changes } = await openWalkthrough(t);
  const first = changes.locator(".changes-step").filter({ hasText: "Step 1 of 3" });
  await first.getByRole("button", { name: "1", exact: true }).click();
  await changes.waitFor({ state: "hidden" });
  await page.locator("#code-rows label").filter({ hasText: "Note on line 1" }).waitFor();
});

test("a review's finding shows beside its line, unfolds its hunk as eyes, and one on no file tops the view", async (t) => {
  const { page, api } = await openProject(t, "report-reviewed", "new", {
    files: { "a.txt": "a\n" },
  });
  // The same fake agent proposes as the one reviewer: the scenario tells the prompts apart.
  const current = await api("api/settings");
  const settings = { ...current.settings, reviewers: [{ name: "Fake" }] };
  const body = JSON.stringify({ revision: current.revision, settings });
  await api("api/settings", { method: "PUT", body });
  const request = { path: "a.txt", lines: { start: 1, end: 1 }, snippet: "a", body: "Change a." };
  const review = await api("api/notes", { method: "POST", body: JSON.stringify(request) });
  const notes = review.notes.map((note) => note.id);
  await api("api/run", { method: "POST", body: JSON.stringify({ notes }) });
  const deadline = Date.now() + WAIT;
  for (;;) {
    const { status } = await api("api/run?id=R1");
    if (status === "ended") break;
    assert.ok(Date.now() < deadline, `R1 is ${status}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  await api("api/run/review", { method: "POST", body: JSON.stringify({ of: "R1" }) });

  await page.locator("#tab-run").click();
  await page.locator("#run .run-entry").click();
  const detail = page.locator("#run-detail");
  // The run's dialog lists the findings once the page holds the review.
  await detail.locator(".run-card.finding").filter({ hasText: "no test" }).waitFor();
  await detail.getByRole("button", { name: "Changes" }).click();

  const changes = page.locator("#changes-view");
  await changes
    .locator(":scope > .changes-loose .changes-finding", { hasText: "no test" })
    .waitFor();
  const step = changes.locator(".changes-step").filter({ hasText: "Step 1 of 1" });
  // Rated mechanical by its author, the hunk a finding is on is drawn whole, as eyes.
  const hunk = step.locator('.changes-hunk-block[data-risk="eyes"]');
  await hunk.locator(".changes-risk.eyes").waitFor();
  assert.equal(await step.locator(".changes-fold").count(), 0, "not folded");
  const beside = hunk.locator(
    '.changes-line:has(button.changes-number.new:text-is("2")) + .changes-notes .changes-finding',
  );
  await beside.filter({ hasText: "reported is not mechanical" }).waitFor();
});

test("a note's remark on the code follows an edit and a delete of the note (JS18)", async (t) => {
  const { page, api } = await openProject(t);
  await addNote(api, "First.");
  // The page reads the notes again on focus (ui/app.js, `refresh`).
  await page.evaluate(() => dispatchEvent(new Event("focus")));
  // Opening a file while the notes are read would make that read give way.
  await page.locator("#notes .note").waitFor();
  await page.locator("#filter").fill("a.rs");
  await page.locator("#filter").press("Enter");
  const remark = page.locator("#code-rows .remark");
  await remark.filter({ hasText: "N1 · First." }).waitFor();

  // An edit and a delete in the page decorate the rows drawn, which keep their remarks.
  await page.locator("#notes .note").first().click();
  const dialog = page.locator("#note-dialog");
  await dialog.getByRole("button", { name: "Edit" }).click();
  await dialog.locator("textarea").fill("Second.");
  await dialog.getByRole("button", { name: /Save/ }).click();
  await remark.filter({ hasText: "N1 · Second." }).waitFor();
  assert.equal(await remark.count(), 1);

  await dialog.getByRole("button", { name: "Delete note" }).click();
  await dialog.getByRole("button", { name: "Click again to delete" }).click();
  await remark.waitFor({ state: "detached" });
});

test("Up and Down in the file filter move the highlight, and Enter opens that file (JS17)", async (t) => {
  const { page } = await openProject(t);
  const filter = page.locator("#filter");
  await filter.fill("rs");
  const highlighted = () =>
    page.evaluate(() =>
      [...document.querySelectorAll("#files .file.active")].map((row) => row.dataset.path),
    );
  await page.locator("#files .file").nth(2).waitFor();
  assert.deepEqual(await highlighted(), ["src/a.rs"], "the best match first");

  await filter.press("ArrowUp");
  assert.deepEqual(await highlighted(), ["src/a.rs"], "the first stays highlighted");
  for (let press = 0; press < 4; press += 1) await filter.press("ArrowDown");
  assert.deepEqual(await highlighted(), ["src/c.rs"], "the last stays highlighted");
  await filter.press("ArrowUp");
  assert.deepEqual(await highlighted(), ["src/b.rs"]);

  await filter.press("Enter");
  await page.locator("#code-rows").filter({ hasText: "bee" }).waitFor();
});

test("a send whose request fails shows the error and leaves Send usable (JS7)", async (t) => {
  const { page, api } = await openProject(t);
  await addNote(api, "Rename two.");
  // The page reads the notes as it loads; this one was written after.
  await page.reload();
  await page.locator(".note-item").first().waitFor();

  // The send's request fails before any answer, as a dropped connection would.
  await page.route("**/api/run", (route) =>
    route.request().method() === "POST" ? route.abort() : route.continue(),
  );
  const send = page.locator("#send-button");
  await send.click();
  await send.click();

  // WebKit's message for a request that got no answer.
  await page.locator("#toast").filter({ hasText: "Load failed" }).waitFor();
  await page.waitForFunction(() => !document.getElementById("send-button").disabled);
  const { runs } = await api("api/runs");
  assert.deepEqual(runs, [], "no run started");
});

test("a send that fails after its step was closed leaves the next send alone (JS7)", async (t) => {
  const { page, api } = await openProject(t);
  await addNote(api, "Rename two.");
  await page.reload();
  await page.locator(".note-item").first().waitFor();

  // Every send's request is held here until the test fails it.
  const held = [];
  let arrived = () => {};
  await page.route("**/api/run", (route) => {
    if (route.request().method() !== "POST") return route.continue();
    held.push(route);
    arrived();
  });
  const sent = (count) =>
    new Promise((resolve) => {
      arrived = () => held.length >= count && resolve();
      arrived();
    });
  const send = page.locator("#send-button");
  const disabled = () => page.evaluate(() => document.getElementById("send-button").disabled);

  await send.click();
  await send.click();
  await sent(1);
  // The step closes while its request runs; a new step sends again.
  await page.keyboard.press("Escape");
  await send.click();
  await send.click();
  await sent(2);
  assert.equal(await disabled(), true, "the second send is under way");

  await held[0].abort();
  await page.locator("#toast").filter({ hasText: "Load failed" }).waitFor();
  assert.equal(await disabled(), true, "the first send's failure leaves the second alone");

  await held[1].abort();
  await page.waitForFunction(() => !document.getElementById("send-button").disabled);
});

test("a right-click on a folder saves a note on it first, to send when chosen", async (t) => {
  const { page, api } = await openProject(t);
  await page.locator('#files .dir[data-path="src"]').click({ button: "right" });
  await page
    .locator("#context-menu")
    .getByRole("menuitem", { name: /Add Note/ })
    .click();

  const dialog = page.locator("#path-note-dialog");
  await dialog.locator(".note-dialog-where").filter({ hasText: "src/" }).waitFor();
  assert.equal(
    await dialog.getByRole("button", { name: /Send/ }).count(),
    0,
    "creating a path note offers Save and Discard, not Send",
  );
  await dialog.locator("textarea").fill("Split this module.");
  await dialog.locator("textarea").press("Meta+Enter");
  await dialog.waitFor({ state: "hidden" });

  // Cmd-Enter saves to the tray without starting a run, as in the line-note composer.
  await page.locator("#notes .note-place").filter({ hasText: "src/" }).waitFor();
  const { notes } = await api("api/notes");
  assert.deepEqual(
    notes.map(({ id, path, target, body }) => [id, path, target, body]),
    [["N1", "src", "folder", "Split this module."]],
  );
  assert.equal(await api("api/run"), null, "saving a folder note sends nothing");

  // The folder's row counts the note on it, closed and open: no file below it carries it.
  const folder = page.locator('#files .dir[data-path="src"]');
  await folder.locator(".count").filter({ hasText: "1" }).waitFor();
  await folder.click();
  await page.locator('#files .file[data-path="src/a.rs"]').waitFor();
  await folder.locator(".count").filter({ hasText: "1" }).waitFor();

  // From the tray, the engineer then sends it when ready.
  await page.locator("#notes .note-send").click();
  await page.waitForFunction(() => !document.getElementById("tab-run").hidden);
  const run = await api("api/run");
  assert.deepEqual(run.notes, ["N1"]);
  assert.match(run.prompt, /\[N1\] src\/ \(folder\)\nSplit this module\.\n$/);
});

test("c on a file's row writes a note on the file, with Discard and Save note", async (t) => {
  const { page, api } = await openProject(t);
  await page.locator('#files .dir[data-path="src"]').click();
  await page.locator('#files .file[data-path="src/b.rs"]').focus();
  await page.keyboard.press("c");

  const dialog = page.locator("#path-note-dialog");
  await dialog.locator(".note-dialog-where").filter({ hasText: "src/b.rs" }).waitFor();
  // An empty note is refused in the dialog, which stays open.
  await dialog.getByRole("button", { name: /Save note/ }).click();
  await dialog.locator(".note-dialog-why").filter({ hasText: "Write the note first." }).waitFor();
  await dialog.locator("textarea").fill("Draft to discard.");
  await dialog.getByRole("button", { name: "Discard", exact: true }).click();
  await dialog.waitFor({ state: "hidden" });
  assert.deepEqual((await api("api/notes")).notes, [], "Discard stores nothing");

  await page.locator('#files .file[data-path="src/b.rs"]').focus();
  await page.keyboard.press("c");
  await dialog.locator("textarea").fill("Fold into a.rs.");
  await dialog.getByRole("button", { name: /Save note/ }).click();
  await dialog.waitFor({ state: "hidden" });

  await page.locator("#notes .note-place").filter({ hasText: "b.rs" }).waitFor();
  const { notes } = await api("api/notes");
  assert.deepEqual(
    notes.map(({ path, target }) => [path, target]),
    [["src/b.rs", "file"]],
  );
  assert.equal(await api("api/run"), null, "Save note sends nothing");
  // The file's row counts its note.
  const count = page.locator('#files .file[data-path="src/b.rs"] .count');
  await count.filter({ hasText: "1" }).waitFor();
});

test("q on a line asks about it, read-only, and its answer becomes a note on request", async (t) => {
  const { page, api } = await openProject(t, "plain");
  // A change the thread does not see: the composer says the agent reads the last commit.
  const { root } = await api("api/repo");
  writeFileSync(join(root, "scratch.txt"), "not committed\n");
  await page.locator("#filter").fill("a.rs");
  await page.locator("#filter").press("Enter");
  await page.locator("#code-rows .n").filter({ hasText: /^1$/ }).click();
  await page.keyboard.press("q");

  const composer = page.locator("#code-rows .composer");
  await composer.locator("label").filter({ hasText: "Ask on line 1" }).waitFor();
  const warning = composer.locator(".send-warning");
  await warning.filter({ hasText: "1 change since the last commit" }).waitFor();
  await composer.locator("textarea").fill("Why one?");
  await composer.locator("textarea").press("Meta+Enter");
  await composer.waitFor({ state: "detached" });

  // The ask's card by its id: Make it a note below adds N2 with the same text.
  const card = page
    .locator("#notes .note")
    .filter({ has: page.locator(".note-id", { hasText: /^N1$/ }) });
  await card.filter({ hasText: "Answered in R1" }).waitFor();
  const asked = (await api("api/notes")).notes;
  assert.deepEqual(
    asked.map(({ id, intent }) => [id, intent]),
    [["N1", "ask"]],
  );
  assert.equal(await card.locator(".note-send").count(), 0, "an ask is never sent");

  await card.click();
  const dialog = page.locator("#note-dialog");
  await dialog.locator(".run-message").filter({ hasText: "Why one?" }).waitFor();
  await dialog.getByRole("button", { name: "Make it a note" }).click();
  await dialog.locator(".note-dialog-why").filter({ hasText: "Made a note" }).waitFor();
  const made = (await api("api/notes")).notes.find((note) => note.id === "N2");
  assert.deepEqual(
    [made.intent, made.target, made.lines, made.body],
    ["change", "lines", { start: 1, end: 1 }, "Why one?"],
  );

  await dialog.getByRole("button", { name: "Open R1" }).click();
  const detail = page.locator("#run-detail");
  await detail.locator(".run-thread-state").filter({ hasText: "Read-only" }).waitFor();
  for (const name of ["Apply", "Open pull request", "Review", "Changes"]) {
    assert.equal(await detail.getByRole("button", { name, exact: true }).count(), 0, name);
  }

  // Resolving the ask ends its thread, and the run's dialog says so, with no Discard left.
  await page.keyboard.press("Escape");
  await detail.waitFor({ state: "hidden" });
  await card.click();
  await dialog.getByRole("button", { name: "Resolve" }).click();
  // Resolve keeps the note dialog open, as for a change note: its button turns to Reopen.
  await dialog.getByRole("button", { name: "Reopen" }).waitFor();
  await card.filter({ hasText: "Resolved" }).waitFor();
  assert.equal((await api("api/thread?id=R1")).state, "discarded");
  await dialog.getByRole("button", { name: "Open R1" }).click();
  await detail.locator(".run-thread-state").filter({ hasText: "Ended" }).waitFor();
  assert.equal(await detail.getByRole("button", { name: "Discard" }).count(), 0);
});

test("the Run tab lists the run of notes and leaves an analysis run out", async (t) => {
  const { page, api } = await openProject(t);
  await startRun(page, api, [await addNote(api, "Rename two.")]);
  const body = JSON.stringify({ kind: "analyze" });
  const { run } = await api("api/analysis", { method: "POST", body });
  assert.equal(run, "R2");
  // The page reads R2, as it reads every run it lists, and draws again once it has.
  await page.waitForFunction(async () => {
    const response = await fetch("api/run?id=R2");
    return response.ok && (await response.json()).status === "running";
  });
  await draws(page, 5);

  const cards = page.locator("#run .run-entry");
  assert.equal(await cards.count(), 1);
  await cards.filter({ hasText: "R1" }).waitFor();
  assert.equal(await page.locator("#run .run-label").textContent(), "Run");
});

test("the run's dialog shows its branch, and Apply ends the thread once the run stops", async (t) => {
  const { page, api } = await openProject(t);
  await startRun(page, api, [await addNote(api, "Rename two.")]);
  await page.locator("#run .run-entry").click();
  const detail = page.locator("#run-detail");
  await detail.locator(".run-thread-branch").filter({ hasText: "postil/r1-rename-two" }).waitFor();
  // A thread whose run goes on cannot end.
  const apply = detail.getByRole("button", { name: "Apply" });
  assert.ok(await apply.isDisabled(), "Apply waits for the run");

  await api("api/run/cancel", { method: "POST", body: JSON.stringify({ run: "R1" }) });
  await page.locator("#run-header .stop").waitFor({ state: "detached" });
  await detail.getByRole("button", { name: "Apply", disabled: false }).click();
  await detail.locator(".run-thread-state").filter({ hasText: "Applied" }).waitFor();
  const thread = await api("api/thread?id=R1");
  assert.equal(thread.state, "applied");
});

test("a run's card marks its worst check, and a Checks row opens the step that ran it", async (t) => {
  const { page, api } = await openProject(t, "evidence");
  const note = await addNote(api, "Fix it.");
  await api("api/run", { method: "POST", body: JSON.stringify({ notes: [note.id] }) });
  await page.locator("#tab-run").click();
  // A check claimed and not in the log ranks below one that failed.
  const card = page.locator("#run .run-entry");
  await card.locator(".run-evidence.failed").waitFor();

  await card.click();
  const detail = page.locator("#run-detail");
  const row = detail.locator(".run-check").filter({ hasText: "cargo clippy" });
  await row.filter({ hasText: "failed · the author says passed" }).waitFor();
  const unseen = detail.locator(".run-check").filter({ hasText: "cargo doc" });
  assert.match(await unseen.textContent(), /not seen in the log/);
  await row.click();
  await detail.getByText("Step 2 of 2").waitFor();
});

test("two threads run side by side, and each stops from its own dialog", async (t) => {
  const { page, api } = await openProject(t);
  await startRun(page, api, [await addNote(api, "Rename two.")]);
  await startRun(page, api, [await addNote(api, "Rename three.")]);
  assert.deepEqual(
    (await api("api/runs")).live.map((run) => run.id),
    ["R1", "R2"],
  );

  // The older run, no longer the latest, is still followed: its card opens its dialog, whose Stop
  // stops it alone.
  await page.locator("#run .run-entry").filter({ hasText: "R1" }).click();
  const detail = page.locator("#run-detail");
  await detail.getByRole("button", { name: "Stop", exact: true }).click();
  // Its Stop goes once the run ended: the dialog is drawn again without it.
  await page.waitForFunction(() => !document.querySelector("#run-detail button.stop"));
  assert.equal((await api("api/run?id=R1")).status, "ended");
  assert.deepEqual(
    (await api("api/runs")).live.map((run) => run.id),
    ["R2"],
  );
  assert.equal((await api("api/run")).status, "running");

  await api("api/run/cancel", { method: "POST", body: JSON.stringify({ run: "R2" }) });
  await page.locator("#run-header .stop").waitFor({ state: "detached" });
});

test("Usage button opens the usage report, switches filters, and Refresh settles", async (t) => {
  const { page } = await openProject(t);
  await page.locator("#open-usage").click();
  const dialog = page.locator("#usage");
  await dialog.waitFor();

  await dialog.locator(".usage-summary").waitFor();
  await dialog.getByRole("radio", { name: "All projects" }).click();
  await dialog.getByRole("radio", { name: "24h" }).click();
  const refresh = dialog.getByRole("button", { name: "Refresh" });
  await refresh.click();
  // Even when the reply matches the previous view, Refresh must settle back to enabled.
  await dialog.getByRole("button", { name: "Refresh", disabled: false }).waitFor();

  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
});

test("OpenCode's logo shows wherever the page names it, and a custom agent has none", async (t) => {
  const { page, api } = await openProject(t, null, "new-opencode");
  const logo = 'svg.agent-logo use[href="#agent-opencode"]';
  const note = await addNote(api, "Rename two.");
  // The page reads the notes as it loads; this one was written after.
  await page.reload();
  await page.locator(".note-item").first().waitFor();

  // The send step's agent picker: OpenCode with its logo, Fake with an empty slot instead.
  await page.locator("#send-button").click();
  await page.locator("#agent-picker").click();
  const menu = page.locator("#context-menu");
  const item = (name) => menu.locator(".menu-item").filter({ hasText: name });
  await item("OpenCode").locator(logo).waitFor({ state: "attached" });
  assert.equal(await item("Fake").locator("svg.agent-logo").count(), 0);
  assert.equal(await item("Fake").locator(".menu-icon-slot").count(), 1);
  await page.keyboard.press("Escape");

  // The run's card, and once it ended, its reviewer's card in the run's dialog.
  await api("api/run", { method: "POST", body: JSON.stringify({ notes: [note.id] }) });
  await page.locator("#tab-run").click();
  const card = page.locator("#run .run-entry");
  await card.locator(logo).waitFor({ state: "attached" });
  const deadline = Date.now() + WAIT;
  for (;;) {
    const { status } = await api("api/run?id=R1");
    if (status === "ended") break;
    assert.ok(Date.now() < deadline, `R1 is ${status}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  await api("api/run/review", { method: "POST", body: JSON.stringify({ of: "R1" }) });
  await card.click();
  const stage = page.locator("#run-detail .run-card").filter({ hasText: "R2" });
  await stage
    .filter({ hasText: "OpenCode · proposes" })
    .locator(logo)
    .waitFor({ state: "attached" });
  await page.keyboard.press("Escape");

  // The Usage dialog's cards.
  await page.locator("#open-usage").click();
  const usage = page.locator("#usage .usage-card");
  await usage.filter({ hasText: "OpenCode" }).locator(logo).waitFor({ state: "attached" });
  const fake = usage.filter({ hasText: "Fake" });
  await fake.waitFor();
  assert.equal(await fake.locator("svg.agent-logo").count(), 0);
});

test("a question from OpenCode shows its logo on the card that asks for an answer", async (t) => {
  const { page, api } = await openProject(t, "ask", "new-opencode");
  const note = await addNote(api, "Rename two.");
  await api("api/run", { method: "POST", body: JSON.stringify({ notes: [note.id] }) });
  await page.locator("#tab-run").click();
  const logo = 'svg.agent-logo use[href="#agent-opencode"]';
  await page.locator("#run .run-card.needs").locator(logo).waitFor({ state: "attached" });
});

// Adds `name` as a reviewer in `dialog` as a user does: "+ Add reviewer", then the agent in the
// menu it opens.
async function addReviewer(dialog, name) {
  await dialog.locator('[data-key="reviewer-add"]').click();
  await dialog.locator(".context-menu").getByRole("menuitem", { name, exact: true }).click();
}

test("Settings lists the models the agent reports and saves one per slot", async (t) => {
  const { page, api } = await openProject(t, "plain");
  await page.locator("#open-settings").click();
  const dialog = page.locator("#settings");

  // The fake agent reports m, pro, fast and, in a group, deep (tests/fake-agent.mjs).
  const model = (key) => dialog.locator(`[data-key="${key}"]`);
  await model("agent-model").locator('option[value="deep"]').waitFor({ state: "attached" });
  await model("agent-model").selectOption("pro");
  // The same agent reviews twice, each time on a model of its own.
  await addReviewer(dialog, "Fake");
  await model("reviewer-model-0").locator('option[value="fast"]').waitFor({ state: "attached" });
  await addReviewer(dialog, "Fake");
  await model("reviewer-model-1").waitFor();
  await model("reviewer-model-0").selectOption("fast");
  await model("reviewer-model-1").selectOption("deep");

  const wanted = {
    agent: { name: "Fake", model: "pro" },
    reviewers: [
      { name: "Fake", model: "fast" },
      { name: "Fake", model: "deep" },
    ],
  };
  // Each change saves on its own, one after another: the last one's reply ends the wait.
  const deadline = Date.now() + WAIT;
  for (;;) {
    const { settings } = await api("api/settings");
    const saved = { agent: settings.agent, reviewers: settings.reviewers };
    if (JSON.stringify(saved) === JSON.stringify(wanted)) break;
    assert.ok(Date.now() < deadline, `saved ${JSON.stringify(saved)}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  assert.equal(await model("reviewer-model-1").inputValue(), "deep");
});

test("Set up a trial vault shows what quickstart printed and turns team memory on", async (t) => {
  const { page, api } = await openProject(t, "plain", "new-memory");
  await page.locator("#open-settings").click();
  const dialog = page.locator("#settings");

  await dialog.getByText("Installed, not set up").waitFor();
  await dialog.locator('[data-key="memory-setup"]').click();
  // The set up takes half a second; the dialog asks again every 250 ms until it ends.
  await dialog.locator(".memory-output").filter({ hasText: "Trial vault ready at" }).waitFor();
  assert.equal(
    await dialog.locator(".memory-output").textContent(),
    "Trial vault ready at /tmp/postil-e2e-vault.",
  );
  await dialog.getByText("Ready", { exact: true }).waitFor();
  const toggle = dialog.locator('[data-key="agent-memory"]');
  await page.waitForFunction(
    () => document.querySelector('[data-key="agent-memory"]')?.ariaChecked === "true",
  );
  assert.equal(await toggle.isDisabled(), false);
  assert.equal((await api("api/settings")).settings.agentMemory, true);
});

test("the Memory tab recalls only while it shows, keeps the send bar hidden, and opens a memory", async (t) => {
  const { page, api } = await openProject(t, "plain", "new-memory-server");
  const recalls = [];
  page.on("request", (request) => {
    if (request.url().includes("api/memory/recall")) recalls.push(request.url());
  });
  // An open note puts the send bar on the Review tab.
  await addNote(api, "check this");
  await page.reload();
  await page.locator("#send-bar").waitFor();

  await openFile(page, "src/a.rs");
  await openFile(page, "src/b.rs");
  assert.deepEqual(recalls, [], "no recall while the Memory tab is closed");

  await page.locator("#tab-memory").click();
  assert.equal(await page.locator("#send-bar").isVisible(), false);
  const cards = page.locator("#memory .memory-card");
  await cards.first().waitFor();
  assert.equal(recalls.length >= 1 && recalls[0].endsWith("path=src%2Fb.rs"), true, recalls.join());
  assert.equal(await page.locator("#memory .memory-weak .memory-card").count(), 1);
  assert.match(await page.locator("#memory .memory-weak").textContent(), /^Possibly related/);

  // A file opened while the tab shows is recalled for.
  const recalled = page.waitForRequest((request) => request.url().endsWith("path=src%2Fc.rs"));
  await openFile(page, "src/c.rs");
  await recalled;

  await cards.filter({ hasText: "a.rs parses lazily" }).click();
  const dialog = page.locator("#memory-dialog");
  await dialog.locator(".run-message").filter({ hasText: "It is lazy." }).waitFor();
  assert.equal(await dialog.locator(".run-message em").textContent(), "is");
  await dialog.getByText("Edit · 5CrAuthor · anchored in the vault").waitFor();
  await page.keyboard.press("Escape");

  await page.locator("#tab-notes").click();
  await page.locator("#send-bar").waitFor();
});

test("a run's dialog shows what the team recorded about its notes and changes", async (t) => {
  const { page, api } = await openProject(t, "plain", "new-memory-server");
  const note = await addNote(api, "Rename two.");
  await api("api/run", { method: "POST", body: JSON.stringify({ notes: [note.id] }) });
  await page.locator("#tab-run").click();
  await page.locator("#run .run-entry").click();
  const detail = page.locator("#run-detail");
  // Pending first, then the cards, the guess under "Possibly related".
  const section = detail.locator(".note-dialog-section").filter({ hasText: /^Memory/ });
  await section.locator(".memory-card").first().waitFor();
  assert.equal(await section.locator(".memory-weak .memory-card").count(), 1);
  await section.locator(".memory-card").filter({ hasText: "a.rs parses lazily" }).click();
  await page.locator("#memory-dialog .run-message").filter({ hasText: "It is lazy." }).waitFor();
});

test("a sent note's thread drafts the memory, and is free again once the draft ended", async (t) => {
  const { page, api } = await openProject(t, "memory-draft", "new-memory-server");
  const note = await addNote(api, "Keep src/a.rs to three lines.");
  await api("api/run", { method: "POST", body: JSON.stringify({ notes: [note.id] }) });
  for (let tries = 0; (await api("api/run"))?.status !== "ended"; tries += 1) {
    assert.ok(tries < 200, "the run did not end");
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  await page.locator("#tab-notes").click();
  const card = page
    .locator("#notes .note")
    .filter({ has: page.locator(".note-id", { hasText: new RegExp(`^${note.id}$`) }) });
  const noteDialog = page.locator("#note-dialog");
  await card.click();
  await noteDialog.locator(".note-dialog-remember:not(:disabled)").click();

  // The agent's draft fills the form once its run ended.
  const dialog = page.locator("#remember-dialog");
  const summary = dialog.locator(".remember-summary");
  await summary.waitFor();
  assert.equal(await summary.inputValue(), "a.txt holds one line on purpose");
  assert.equal(
    await dialog.locator(".remember-body").inputValue(),
    "The parser reads a.txt whole: keep it to one line.",
  );
  await dialog.locator(".remember-save").click();
  await dialog.waitFor({ state: "hidden" });

  // The draft ended: its thread is free, so Remember is offered again, beside the chip.
  await card.click();
  await noteDialog.locator(".remembered-chip").waitFor();
  const remember = noteDialog.locator(".note-dialog-remember");
  assert.equal(await remember.isDisabled(), false);
  assert.notEqual(await remember.getAttribute("title"), "Wait for the run");
});

test("Remember waits for the thread's run, saves a note's text, and the note opens its memory", async (t) => {
  // `hang`: the run never ends, so its thread stays busy for the whole test.
  const { page, api } = await openProject(t, "hang", "new-memory-server");
  const running = await addNote(api, "In the run.");
  const unsent = await addNote(api, "Keep src/a.rs to three lines.");
  await startRun(page, api, [running]);
  await page.locator("#tab-notes").click();
  const card = (id) =>
    page
      .locator("#notes .note")
      .filter({ has: page.locator(".note-id", { hasText: new RegExp(`^${id}$`) }) });
  const noteDialog = page.locator("#note-dialog");

  // The thread's run is under way: its agent would get the draft.
  await card(running.id).click();
  const disabled = noteDialog.locator(".note-dialog-remember");
  await disabled.waitFor();
  assert.equal(await disabled.isDisabled(), true);
  assert.equal(await disabled.getAttribute("title"), "Wait for the run");
  await page.keyboard.press("Escape");

  // Never sent: no draft run, the dialog opens with the note's text.
  await card(unsent.id).click();
  await noteDialog.locator(".note-dialog-remember").click();
  const dialog = page.locator("#remember-dialog");
  const body = dialog.locator(".remember-body");
  await body.waitFor();
  assert.equal(await body.inputValue(), "Keep src/a.rs to three lines.");
  await dialog.locator(".remember-summary").fill("src/a.rs holds three lines");

  // A duplicate opens over the draft, and Escape closes it back to the draft as it was.
  const duplicate = dialog
    .locator(".remember-check .memory-card")
    .filter({ has: page.locator(".note-body", { hasText: /^a\.rs parses lazily$/ }) });
  await duplicate.click();
  const memory = page.locator("#memory-dialog");
  await memory
    .locator(".run-message")
    .filter({ hasText: /^It is lazy\.$/ })
    .waitFor();
  await page.keyboard.press("Escape");
  await memory.waitFor({ state: "hidden" });
  assert.equal(await dialog.isVisible(), true);
  assert.equal(
    await dialog.locator(".remember-summary").inputValue(),
    "src/a.rs holds three lines",
  );

  await dialog.locator(".remember-save").click();
  await dialog.waitFor({ state: "hidden" });
  const saved = (await api("api/notes")).notes.find((note) => note.id === unsent.id);
  assert.equal(saved.memory, "mem_01SAVED");

  // The note shows it was remembered, and opens the memory.
  await card(unsent.id).click();
  const chip = noteDialog.locator(".remembered-chip");
  await chip.waitFor();
  assert.equal(await chip.textContent(), "Remembered");
  await chip.click();
  await memory
    .locator(".run-message")
    .filter({ hasText: /^It is lazy\.$/ })
    .waitFor();
});

// The longest name a custom agent may have (MAX_NAME in src/settings.rs).
const LONG_NAME = "An agent whose name runs to forty chars!";

// A project whose agent and analyzer are OpenCode, reviewed by OpenCode and then by a custom
// agent named LONG_NAME, both played by the fake agent.
async function openLongNameProject(t) {
  const opened = await openProject(t, null, "new-opencode");
  const { revision, settings } = await opened.api("api/settings");
  const fake = settings.customAgents.find((agent) => agent.name === "Fake");
  const next = {
    ...settings,
    customAgents: [...settings.customAgents, { name: LONG_NAME, command: fake.command }],
    enabledAgents: [...settings.enabledAgents, LONG_NAME],
    analyzer: { name: "OpenCode", model: null },
    reviewers: [
      { name: "OpenCode", model: null },
      { name: LONG_NAME, model: null },
    ],
  };
  const body = JSON.stringify({ revision, settings: next });
  await opened.api("api/settings", { method: "PUT", body });
  // The page reads the settings as it loads; these were written after.
  await opened.page.reload();
  await opened.page.locator("#repo-name").filter({ hasText: /\S/ }).waitFor();
  return opened;
}

// Waits until the computed opacity of `locator`'s element is `value`, past its transition.
// The element is looked up anew each frame: a save in Settings redraws the dialog, and a node
// it replaced has no computed style.
async function settlesAt(dialog, selector, value) {
  await dialog.evaluate(
    (node, [query, wanted]) =>
      new Promise((resolve) => {
        const check = () => {
          const target = node.querySelector(query);
          if (target && getComputedStyle(target).opacity === wanted) resolve();
          else requestAnimationFrame(check);
        };
        check();
      }),
    [selector, value],
  );
}

// Checks that `dialog`'s slot rows share their columns: the agents and the model pickers of
// every slot (`keys`, the model pickers' keys, one per slot) start at one x, the reviewers'
// model cells have one width and their buttons one x, every card fits in the dialog, the long
// name is cut to its column with a tooltip, and a long model name shows whole.
async function assertSlotsAligned(dialog, keys) {
  // The dialog comes in with a short animation, which moves every box while it runs.
  await dialog.evaluate((node) =>
    Promise.all(node.getAnimations({ subtree: true }).map((animation) => animation.finished)),
  );
  const box = (key) => dialog.locator(`[data-key="${key}"]`).boundingBox();
  const models = await Promise.all(keys.map(box));
  for (const [index, model] of models.entries()) {
    assert.equal(model.x, models[0].x, `${keys[index]} starts where ${keys[0]} does`);
  }
  const agents = await Promise.all(
    (await dialog.locator(".slot-agent").all()).map((agent) => agent.boundingBox()),
  );
  assert.equal(agents.length, keys.length, "one agent per slot");
  for (const agent of agents) assert.equal(agent.x, agents[0].x, "the agents start at one x");
  const cells = await Promise.all(
    (await dialog.locator("li .model-select").all()).map((cell) => cell.boundingBox()),
  );
  assert.equal(cells[1].width, cells[0].width, "the reviewers' model cells are one width");
  const removes = await Promise.all(["unreview-0", "unreview-1"].map(box));
  assert.equal(removes[1].x, removes[0].x, "the reviewers' buttons line up");

  const outer = await dialog.boundingBox();
  for (const card of await dialog.locator(".slot-card").all()) {
    const { x, width } = await card.boundingBox();
    assert.ok(x >= outer.x && x + width <= outer.x + outer.width, "a slot card fits its dialog");
  }
  const name = dialog.locator("li .slot-agent").filter({ hasText: LONG_NAME });
  assert.equal(await name.getAttribute("title"), LONG_NAME);
  const cut = await name
    .locator(".slot-agent-name")
    .evaluate((node) => node.scrollWidth > node.clientWidth);
  assert.ok(cut, "the long name is cut to its column");

  // A model name as long as real ones shows whole, not cut with an ellipsis. The reviewers'
  // model cells are one width, so one stands for both; the send-to row's is wider.
  const select = dialog.locator('[data-key="reviewer-model-1"]');
  await select.locator('option[value="long"]').waitFor({ state: "attached" });
  await select.selectOption("long");
  const text = select.locator("xpath=..").locator(".picker-text");
  await text.filter({ hasText: "OpenCode/Nemotron 3.5 Super VL Free" }).waitFor();
  const room = await text.evaluate((node) => ({ text: node.scrollWidth, room: node.clientWidth }));
  assert.ok(room.text <= room.room, `a long model name shows whole: ${JSON.stringify(room)}`);

  // A move a row cannot make is hidden, not drawn disabled; the others are shown.
  // Looked up and read in one evaluation, for the reason `settlesAt` gives.
  const visibility = (key) =>
    dialog.evaluate(
      (node, query) => getComputedStyle(node.querySelector(query)).visibility,
      `[data-key="${key}"]`,
    );
  assert.equal(await visibility("up-0"), "hidden");
  assert.equal(await visibility("down-1"), "hidden");
  assert.equal(await visibility("down-0"), "visible");
  assert.equal(await visibility("up-1"), "visible");

  // A row's buttons show with the focus in the row, and not in another, with the pointer away.
  await dialog.page().mouse.move(1, 1);
  await dialog.locator('[data-key="reviewer-model-0"]').focus();
  const actions = (index) => `li:nth-child(${index + 1}) .slot-actions`;
  await settlesAt(dialog, actions(0), "1");
  await settlesAt(dialog, actions(1), "0");
}

test("the workflow slots line up whatever the agents' names, in Settings and the wizard", async (t) => {
  const { page } = await openLongNameProject(t);
  await page.locator("#open-settings").click();
  const settings = page.locator("#settings");
  await settings.locator('[data-key="reviewer-model-1"]').waitFor();
  // Settings has the Analyzer's row, which the wizard does not.
  await assertSlotsAligned(settings, [
    "agent-model",
    "analyzer-model",
    "reviewer-model-0",
    "reviewer-model-1",
  ]);

  await settings.locator('[data-key="run-wizard"]').click();
  const wizard = page.locator("#wizard");
  await wizard.locator('[data-key="step-workflow"]').click();
  await wizard.locator('[data-key="reviewer-model-1"]').waitFor();
  await assertSlotsAligned(wizard, ["wizard-agent-model", "reviewer-model-0", "reviewer-model-1"]);
});

test("the workflow slots ring a picker only after a key, and Tab reaches every button", async (t) => {
  const { page } = await openLongNameProject(t);
  await page.locator("#open-settings").click();
  const wizard = page.locator("#wizard");
  await page.locator('#settings [data-key="run-wizard"]').click();
  await wizard.locator('[data-key="step-workflow"]').click();
  const model = (key) => wizard.locator(`[data-key="${key}"]`);
  await model("reviewer-model-1").waitFor();
  const outline = (key) =>
    wizard.evaluate(
      (node, query) => getComputedStyle(node.querySelector(query).parentElement).outlineStyle,
      `[data-key="${key}"]`,
    );
  const focused = () => page.evaluate(() => document.activeElement?.dataset.key);

  // A click focuses the picker without its ring; Shift-Tab and Tab ring the one they reach.
  const box = await model("reviewer-model-0").boundingBox();
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  assert.equal(await focused(), "reviewer-model-0");
  assert.equal(await outline("reviewer-model-0"), "none");
  await page.keyboard.press("Shift+Tab");
  assert.equal(await focused(), "wizard-agent-model");
  assert.equal(await outline("wizard-agent-model"), "solid");
  await page.keyboard.press("Tab");
  assert.equal(await outline("reviewer-model-0"), "solid");

  // Tab goes on through the row's buttons, which it shows, past the move the first row cannot
  // make, then through the last row to "+ Add reviewer".
  await page.keyboard.press("Tab");
  assert.equal(await focused(), "down-0");
  await settlesAt(wizard, "li:nth-child(1) .slot-actions", "1");
  const keys = [];
  for (let step = 0; step < 5; step += 1) {
    await page.keyboard.press("Tab");
    keys.push(await focused());
  }
  assert.deepEqual(keys, ["unreview-0", "reviewer-model-1", "up-1", "unreview-1", "reviewer-add"]);
});

test("the startup wizard opens on first launch, saves settings, and stays closed on reload", async (t) => {
  const created = await fetch(new URL("new-fresh", bridge), { method: "POST" });
  const site = new URL((await created.json()).base, bridge);
  const page = await browser.newPage();
  t.after(() => page.close());
  page.setDefaultTimeout(WAIT);
  await page.addInitScript(init);
  await page.goto(site.href);

  const wizard = page.locator("#wizard");
  await wizard.waitFor();
  await wizard.getByRole("heading", { name: "Connect your coding agents" }).waitFor();

  // Add a custom agent on Step 1, walk through Steps 2–4, change appearance, and Finish.
  await wizard.getByRole("button", { name: "Add agent…" }).click();
  await wizard.locator('[data-key="form-name"]').fill("Local");
  await wizard.locator('[data-key="form-command"]').fill("local-agent --acp");
  await wizard.getByRole("button", { name: "Add", exact: true }).click();
  await wizard.getByRole("button", { name: "Next" }).click();

  await wizard.getByRole("heading", { name: "Default agent and reviewers" }).waitFor();
  assert.equal(await wizard.locator('[data-key="wizard-agent"]').inputValue(), "Local");
  await wizard.getByRole("button", { name: "Next" }).click();

  await wizard.getByRole("heading", { name: "Appearance and startup" }).waitFor();
  await wizard.getByRole("radio", { name: "Dark" }).click();
  await wizard.getByRole("button", { name: "Next" }).click();

  await wizard.getByRole("heading", { name: "Ready to review" }).waitFor();
  await wizard.getByRole("button", { name: "Finish", exact: true }).click();
  await wizard.waitFor({ state: "hidden" });

  const reply = await fetch(new URL("api/settings", site));
  const saved = await reply.json();
  assert.equal(saved.firstRun, false);
  assert.equal(saved.settings.theme, "dark");
  assert.deepEqual(saved.settings.agent, { name: "Local", model: null });

  // Reloading after setup shows the welcome screen without reopening the wizard.
  await page.reload();
  await page.locator("#welcome").waitFor();
  assert.equal(await wizard.isVisible(), false);
});

test("Skip on the first run saves the defaults with the installed agents turned on", async (t) => {
  const created = await fetch(new URL("new-fresh-gemini", bridge), { method: "POST" });
  const site = new URL((await created.json()).base, bridge);
  const page = await browser.newPage();
  t.after(() => page.close());
  page.setDefaultTimeout(WAIT);
  await page.addInitScript(init);
  await page.goto(site.href);

  const wizard = page.locator("#wizard");
  await wizard.getByRole("switch", { name: /Gemini/ }).waitFor();

  // Each row's mark says where the agent stands; a missing agent's row is dimmed.
  const row = (name) =>
    wizard.locator(".agent-row").filter({ has: page.getByRole("switch", { name }) });
  const mark = (name) => row(name).locator(".agent-mark");
  assert.equal(await mark("Gemini").getAttribute("aria-label"), "Installed");
  assert.equal(await mark("OpenCode").getAttribute("aria-label"), "Not installed");
  const opacity = (name) =>
    row(name)
      .locator(".agent-text")
      .evaluate((node) => Number(getComputedStyle(node).opacity));
  assert.equal(await opacity("Gemini"), 1);
  assert.ok((await opacity("OpenCode")) < 1, "a missing agent's row is dimmed");
  // OpenCode's logo refers to a symbol the page loaded from agent-logos.svg.
  const logo = row("OpenCode").locator("svg.agent-logo");
  const symbol = await logo.locator("use").getAttribute("href");
  await page.locator(`${symbol} path`).first().waitFor({ state: "attached" });

  await wizard.getByRole("button", { name: "Skip" }).click();
  await wizard.waitFor({ state: "hidden" });

  const saved = await (await fetch(new URL("api/settings", site))).json();
  assert.equal(saved.firstRun, false);
  assert.deepEqual(saved.settings.enabledAgents, ["Gemini"]);
  assert.deepEqual(saved.settings.agent, { name: "Gemini", model: null });
  assert.equal(saved.settings.theme, "system");
});

test("Skip on the first run turns team memory on only when hippius-mem is ready", async (t) => {
  for (const memory of ["ready", "unconfigured"]) {
    const body = JSON.stringify({ memory });
    const created = await fetch(new URL("new-fresh-memory", bridge), { method: "POST", body });
    const site = new URL((await created.json()).base, bridge);
    const page = await browser.newPage();
    t.after(() => page.close());
    page.setDefaultTimeout(WAIT);
    await page.addInitScript(init);
    await page.goto(site.href);

    const wizard = page.locator("#wizard");
    await wizard.getByRole("button", { name: "Skip" }).click();
    await wizard.waitFor({ state: "hidden" });
    const saved = await (await fetch(new URL("api/settings", site))).json();
    assert.equal(saved.firstRun, false, memory);
    assert.equal(saved.settings.agentMemory, memory === "ready", memory);
  }
});

test("a wizard whose save is refused starts over from the file another window wrote", async (t) => {
  const created = await fetch(new URL("new-fresh", bridge), { method: "POST" });
  const site = new URL((await created.json()).base, bridge);
  const page = await browser.newPage();
  t.after(() => page.close());
  page.setDefaultTimeout(WAIT);
  await page.addInitScript(init);
  await page.goto(site.href);

  const wizard = page.locator("#wizard");
  await wizard.getByRole("heading", { name: "Connect your coding agents" }).waitFor();
  await wizard.getByRole("button", { name: "Next" }).click();
  await wizard.getByRole("button", { name: "Next" }).click();
  await wizard.getByRole("radio", { name: "Dark" }).click();
  await wizard.getByRole("button", { name: "Next" }).click();
  await wizard.getByRole("heading", { name: "Ready to review" }).waitFor();

  // Another window creates settings.json while this wizard is open.
  const before = await (await fetch(new URL("api/settings", site))).json();
  const other = { ...before.settings, theme: "light", tabWidth: 8 };
  const body = JSON.stringify({ revision: before.revision, settings: other });
  const written = await fetch(new URL("api/settings", site), { method: "PUT", body });
  assert.equal(written.status, 200, await written.text());

  await wizard.getByRole("button", { name: "Finish", exact: true }).click();
  await wizard.getByRole("alert").filter({ hasText: "the file changed" }).waitFor();
  await wizard.getByRole("button", { name: "Finish", exact: true }).click();
  await wizard.waitFor({ state: "hidden" });

  const saved = await (await fetch(new URL("api/settings", site))).json();
  assert.equal(saved.settings.theme, "light");
  assert.equal(saved.settings.tabWidth, 8);
});

// Jump to definition (docs/review-assist-plan.md, 7.7): `src/a.rs` calls a function of
// `src/b.rs`, one defined in two files, one of its own and one defined nowhere;
// `src/long.rs` calls `helper` far enough down to scroll. No run starts, so the scenario
// does not matter.
const LONG_FILLER = 150;
const DEFINITIONS = {
  "src/a.rs": [
    "fn main() {",
    "    helper();",
    "    ambiguous();",
    "    local();",
    "    // helper is in b",
    '    missing("helper");',
    "}",
    "fn local() {}",
    "",
  ].join("\n"),
  "src/b.rs": "pub fn helper() {}\n\npub fn ambiguous() {}\n",
  "other/c.rs": "pub fn ambiguous() {}\n",
  "src/long.rs": `${"// filler\n".repeat(LONG_FILLER)}fn far() {\n    helper();\n}\n`,
  "config.json": '{"helper": 1}\n',
};

// A project of `DEFINITIONS`, with `asked`: every ask for definitions the page sends, to tell
// a click that asked nothing.
async function openDefinitions(t) {
  const opened = await openProject(t, "flood", "new", { files: DEFINITIONS });
  const asked = [];
  opened.page.on("request", (request) => {
    if (request.url().includes("api/definitions?")) asked.push(request.url());
  });
  return { ...opened, asked };
}

async function openFile(page, path) {
  await page.locator("#filter").fill(path);
  await page.locator("#filter").press("Enter");
  await page
    .locator("#crumbs b")
    .filter({ hasText: path.split("/").at(-1) })
    .waitFor();
}

// The token `text` on 0-based line `line` of the open file.
const token = (page, line, text) =>
  page.locator(`#code-rows .ln[data-line="${line}"] .c`).getByText(text, { exact: true });

test("Cmd-click jumps to a definition and leaves the selection as it was; Cmd-[ goes back", async (t) => {
  const { page } = await openDefinitions(t);
  await openFile(page, "src/a.rs");
  await page.locator('#code-rows .ln[data-line="0"] .n').click();
  const selected = page.locator('#code-rows .ln.selected[data-line="0"]');
  await selected.waitFor();

  // In the same file: the selection stays, and the line jumped to is marked.
  await token(page, 3, "local").click({ modifiers: ["Meta"] });
  await page.locator('#code-rows .ln.jumped[data-line="7"]').waitFor();
  assert.equal(await selected.count(), 1);

  // To another file, and back with Cmd-[, the selection with it; forward again with Cmd-].
  await token(page, 1, "helper").click({ modifiers: ["Meta"] });
  await page.locator("#crumbs b").filter({ hasText: "b.rs" }).waitFor();
  await page.locator('#code-rows .ln.jumped[data-line="0"]').waitFor();
  await page.keyboard.press("Meta+BracketLeft");
  await page.locator("#crumbs b").filter({ hasText: "a.rs" }).waitFor();
  await selected.waitFor();
  await page.keyboard.press("Meta+BracketRight");
  await page.locator("#crumbs b").filter({ hasText: "b.rs" }).waitFor();
});

test("a name defined twice opens a ranked list, and a pick jumps there", async (t) => {
  const { page } = await openDefinitions(t);
  await openFile(page, "src/a.rs");
  await token(page, 2, "ambiguous").click({ modifiers: ["Meta"] });

  const dialog = page.locator("#definitions-dialog");
  await dialog.filter({ hasText: "2 definitions · by name" }).waitFor();
  const places = await dialog.locator(".definition-place").allTextContents();
  assert.deepEqual(places, ["src/b.rs:3", "other/c.rs:1"], "the same folder first");
  await dialog.locator(".definition-card").nth(1).click();
  await page.locator("#crumbs b").filter({ hasText: "c.rs" }).waitFor();
});

test("Go to Definition in the right-click menu jumps, and clicks that cannot jump say why", async (t) => {
  const { page, asked } = await openDefinitions(t);
  await openFile(page, "src/a.rs");

  // A Cmd-click on `helper` in a comment or a string asks nothing: the jump after them is
  // the only ask.
  const comment = token(page, 4, "// helper is in b");
  await comment.click({ modifiers: ["Meta"], position: { x: 30, y: 5 } });
  await token(page, 5, '"helper"').click({ modifiers: ["Meta"] });
  await token(page, 0, "main").click({ modifiers: ["Meta"] });
  await page.locator("#toast").filter({ hasText: "This is the definition." }).waitFor();
  assert.equal(asked.length, 1, asked.join("\n"));

  await token(page, 5, "missing").click({ modifiers: ["Meta"] });
  await page.locator("#toast").filter({ hasText: "No definition found for missing." }).waitFor();

  await token(page, 1, "helper").click({ button: "right" });
  await page
    .locator("#context-menu")
    .getByRole("menuitem", { name: /Go to Definition/ })
    .click();
  await page.locator("#crumbs b").filter({ hasText: "b.rs" }).waitFor();

  await openFile(page, "config.json");
  await page.locator('#code-rows .ln[data-line="0"] .c').click({ modifiers: ["Meta"] });
  await page.locator("#toast").filter({ hasText: "No definitions for this file type." }).waitFor();
});

// Waits until the project's index has finished its first build.
const indexBuilt = (page) =>
  page.waitForFunction(
    async () => (await (await fetch("api/definitions?name=x")).json()).state === "ready",
    null,
    { polling: 100 },
  );

test("while the index is building, the page asks again and then jumps", async (t) => {
  const { page, asked } = await openDefinitions(t);
  await openFile(page, "src/a.rs");
  let answers = 0;
  await page.route(/api\/definitions\?/, (route) => {
    answers += 1;
    return answers === 1 ? route.fulfill({ json: { state: "building" } }) : route.continue();
  });

  await token(page, 1, "helper").click({ modifiers: ["Meta"] });

  await page.locator("#crumbs b").filter({ hasText: "b.rs" }).waitFor();
  assert.equal(asked.length, 2, asked.join("\n"));
});

test("Escape stops the page asking again while the index is building", async (t) => {
  const { page, asked } = await openDefinitions(t);
  await openFile(page, "src/a.rs");
  await page.route(/api\/definitions\?/, (route) => route.fulfill({ json: { state: "building" } }));
  // The page's timers run only when the test moves this clock on.
  await page.clock.install();
  await page.clock.pauseAt(Date.now() + 1000);

  const answered = page.waitForResponse(/api\/definitions\?/);
  await token(page, 1, "helper").click({ modifiers: ["Meta"] });
  await answered;
  await page.keyboard.press("Escape");
  // Far past the 250 ms the page waits before asking again (ui/code-view.js).
  await page.clock.runFor(5000);
  // A round trip to the page, so a request it sent during the run above has been seen.
  await page.evaluate(() => 0);

  assert.equal(asked.length, 1, asked.join("\n"));
  await page.locator("#crumbs b").filter({ hasText: "a.rs" }).waitFor();
});

test("a definition written on disk is found after the window regains focus", async (t) => {
  const { page, api } = await openDefinitions(t);
  // Built before the files are written, so only the focus refresh can see them.
  await indexBuilt(page);
  const { root } = await api("api/repo");
  writeFileSync(join(root, "src/d.rs"), "fn caller() {\n    fresh();\n}\n");
  writeFileSync(join(root, "src/e.rs"), "pub fn fresh() {}\n");

  const refreshed = page.waitForResponse((response) =>
    response.url().includes("api/definitions/refresh"),
  );
  await page.evaluate(() => dispatchEvent(new Event("focus")));
  await refreshed;
  // The refresh runs on the index's own thread: this waits until it has seen the new file.
  await page.waitForFunction(
    async () => (await (await fetch("api/definitions?name=fresh")).json()).total === 1,
    null,
    { polling: 100 },
  );

  await openFile(page, "src/d.rs");
  await token(page, 1, "fresh").click({ modifiers: ["Meta"] });
  await page.locator("#crumbs b").filter({ hasText: "e.rs" }).waitFor();
});

test("a jump to a file deleted since the index looked says so and stays put", async (t) => {
  const { page, api } = await openDefinitions(t);
  await indexBuilt(page);
  const { root } = await api("api/repo");
  unlinkSync(join(root, "src/b.rs"));

  await openFile(page, "src/a.rs");
  await token(page, 1, "helper").click({ modifiers: ["Meta"] });

  await page.locator("#toast").filter({ hasText: "src/b.rs is no longer there." }).waitFor();
  await page.locator("#crumbs b").filter({ hasText: "a.rs" }).waitFor();
});

test("Cmd-[ comes back to where the file was scrolled", async (t) => {
  const { page } = await openDefinitions(t);
  await openFile(page, "src/long.rs");
  const code = page.locator("#code");
  await code.evaluate((element) => {
    element.scrollTop = element.scrollHeight;
  });
  const call = token(page, LONG_FILLER + 1, "helper");
  await call.waitFor();
  const scrolled = await code.evaluate((element) => element.scrollTop);
  assert.ok(scrolled > 0, "the file scrolls");

  await call.click({ modifiers: ["Meta"] });
  await page.locator("#crumbs b").filter({ hasText: "b.rs" }).waitFor();
  await page.keyboard.press("Meta+BracketLeft");
  await page.locator("#crumbs b").filter({ hasText: "long.rs" }).waitFor();
  await page.waitForFunction((top) => document.getElementById("code").scrollTop === top, scrolled);
});
