// The page's end-to-end tests: the page in Playwright's WebKit, the engine closest to the app's
// WKWebView, while a run streams. `cargo test page_end_to_end -- --ignored` runs them through
// the bridge in src/site/tests/page_e2e.rs, which serves real sites with the fake agent's
// `flood` scenario (tests/fake-agent.mjs): a reply part every 20 ms until the run is stopped.
//
// What they catch, and the page's unit tests cannot: a click lost because the page replaced
// the node under the pointer while it drew the run again (docs/review-2026-11-23.md, D4).

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, sep } from "node:path";
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

// ---- The map (docs/review-assist-plan.md, 8.3) --------------------------------------------

// An analysis of the default files, written by hand as an agent's run would leave it: a
// component on line 2 of src/a.rs inside a boundary, a store on src/b.rs, and an edge between
// the two cards (src/analysis.rs, "The file").
const ANALYSIS = {
  version: 2,
  diagrams: [
    {
      id: "D1",
      kind: "architecture",
      title: "System",
      run: "R1",
      nodes: [
        {
          id: "D1.1",
          kind: "component",
          label: "Parser",
          summary: "reads the input",
          detail: "Turns **text** into tokens.",
          group: "D1.3",
          anchors: [
            { path: "src/a.rs", target: "lines", lines: { start: 2, end: 2 }, snippet: "two" },
          ],
        },
        {
          id: "D1.2",
          kind: "store",
          label: "Cache",
          summary: "keeps tokens",
          detail: "",
          anchors: [{ path: "src/b.rs", target: "file", state: "found" }],
        },
        {
          id: "D1.3",
          kind: "boundary",
          label: "Process",
          summary: "",
          detail: "",
          anchors: [{ path: "src", target: "folder" }],
        },
      ],
      edges: [{ from: "D1.1", to: "D1.2", label: "fills" }],
    },
  ],
};

// Writes `analysis` into the open project's analysis file, as an analysis run would.
async function writeAnalysis(api, analysis = ANALYSIS) {
  const { root } = await api("api/repo");
  mkdirSync(join(root, ".git/postil"), { recursive: true });
  writeFileSync(join(root, ".git/postil/analysis.json"), JSON.stringify(analysis));
}

// Opens a project with `ANALYSIS` written, and counts the page's requests for the layout
// library.
async function openMapped(t, scenario = "flood", options = {}) {
  const opened = await openProject(t, scenario, "new", options);
  await writeAnalysis(opened.api);
  const dagre = [];
  opened.page.on("request", (request) => {
    if (request.url().endsWith("vendor/dagre.min.js")) dagre.push(request.url());
  });
  return { ...opened, dagre };
}

// The card whose label is `label`, whole: `hasText` alone matches any card whose text holds it
// anywhere, case-insensitively ("Far" in "runs on far past").
const node = (page, label) => {
  const exact = new RegExp(`^${label.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`);
  return page
    .locator(".map-node")
    .filter({ has: page.locator(".map-node-label", { hasText: exact }) });
};

test("the map loads its layout only when opened, and a node's click opens its lines", async (t) => {
  const { page, dagre } = await openMapped(t);
  assert.deepEqual(dagre, [], "a window that never opens the map never loads dagre");

  await page.locator("#toggle-map").click();
  await node(page, "Parser").waitFor();
  assert.equal(dagre.length, 1);
  assert.equal(await page.locator("#toggle-map").getAttribute("aria-pressed"), "true");

  await node(page, "Parser").click();
  await page.locator("#crumbs b").filter({ hasText: "a.rs" }).waitFor();
  await page.locator('#code-rows .ln.mapped[data-line="1"]').waitFor();
  assert.equal(await page.locator("#code-rows .ln.mapped").count(), 1, "line 2 alone");
  assert.match(await node(page, "Parser").getAttribute("class"), /\bselected\b/);

  // Closing and opening again loads nothing more.
  await page.evaluate(() => window.postil.menu("map"));
  await page.locator("#map").waitFor({ state: "hidden" });
  await page.evaluate(() => window.postil.menu("map"));
  await node(page, "Parser").waitFor();
  assert.equal(dagre.length, 1);
});

test("the changes view opens beside the map, and Escape closes it, not the map", async (t) => {
  const { page, api } = await openMapped(t);
  await page.locator("#toggle-map").click();
  await node(page, "Parser").waitFor();
  await startRun(page, api, [await addNote(api, "Rename two.")]);
  await page.locator("#run .run-entry").click();
  await page.locator("#run-detail").getByRole("button", { name: "Changes" }).click();
  const changes = page.locator("#changes-view");
  await changes.waitFor();

  const map = await page.locator("#map").boundingBox();
  const covered = await changes.boundingBox();
  assert.ok(covered.x >= map.x + map.width - 1, "the changes view covers the code, not the map");
  await node(page, "Parser").waitFor();

  await page.locator("#tab-notes").focus();
  await page.keyboard.press("Escape");
  await changes.waitFor({ state: "hidden" });
  assert.ok(await page.locator("#map").isVisible(), "the map stays");
});

test("the map's divider resizes the map and keeps its width; focus mode hides the map", async (t) => {
  const { page, api } = await openMapped(t);
  // Room for the map to grow beside the code (ui/panes.js, `fit`).
  await page.setViewportSize({ width: 1600, height: 900 });
  await page.locator("#toggle-map").click();
  await node(page, "Parser").waitFor();
  const divider = page.locator("#divider-map");
  const before = await page.locator("#map").boundingBox();
  const box = await divider.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + 200);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 60, box.y + 200, { steps: 4 });
  await page.mouse.up();
  const after = await page.locator("#map").boundingBox();
  assert.ok(Math.abs(after.width - before.width - 60) <= 2, `${before.width} to ${after.width}`);
  // The width is saved after 300 ms of quiet (dividers.js): read until it lands.
  const deadline = Date.now() + WAIT;
  let saved = (await api("api/layout")).map;
  while (Math.abs(saved - after.width) > 2 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    saved = (await api("api/layout")).map;
  }
  assert.ok(Math.abs(saved - after.width) <= 2, `saved ${saved}, drawn ${after.width}`);

  await page.evaluate(() => window.postil.menu("focus-mode"));
  await page.locator("#map").waitFor({ state: "hidden" });
  assert.ok(!(await divider.isVisible()), "nor its divider");
  // Cmd-Shift-M leaves focus mode, the map open as it was.
  await page.evaluate(() => window.postil.menu("map"));
  await node(page, "Parser").waitFor();
});

test("a node's dialog shows its detail, code, connections and notes, and Add note writes there", async (t) => {
  const { page, api } = await openMapped(t);
  const request = {
    path: "src/a.rs",
    lines: { start: 2, end: 2 },
    snippet: "two",
    body: "On two.",
  };
  await api("api/notes", { method: "POST", body: JSON.stringify(request) });
  // The page read the notes as it opened, before this one: the window's focus reads them again,
  // as it does on returning from another app.
  await page.evaluate(() => dispatchEvent(new Event("focus")));
  await page.locator("#notes").getByText("On two.").waitFor();
  await page.locator("#toggle-map").click();
  await node(page, "Parser").dblclick();

  const dialog = page.locator("#node-dialog");
  await dialog.waitFor();
  assert.equal(await dialog.locator(".note-dialog-title").textContent(), "Parser");
  // The detail is the Markdown subset, drawn as elements, never as HTML.
  assert.equal(await dialog.locator(".node-detail strong").textContent(), "text");
  await dialog.locator(".node-anchor").filter({ hasText: "src/a.rs:2" }).waitFor();
  await dialog.locator(".node-edge").filter({ hasText: "Cache" }).waitFor();
  await dialog.getByText("On two.").waitFor();

  await dialog.getByRole("button", { name: "Add note" }).click();
  await dialog.waitFor({ state: "hidden" });
  await page.locator("#note-body").waitFor();
  assert.equal(await page.locator("#code-rows .ln.selected").getAttribute("data-line"), "1");
});

test("the keyboard moves between nodes, and Enter opens the dialog", async (t) => {
  const { page } = await openMapped(t);
  await page.locator("#toggle-map").click();
  await node(page, "Parser").focus();
  // The arrow that points from Parser to Cache as dagre placed them: the edge between them
  // crosses a boundary, so it is not laid out and does not put Cache to the right
  // (ui/diagram.js, `layoutDiagram`).
  const from = await node(page, "Parser").boundingBox();
  const to = await node(page, "Cache").boundingBox();
  const [dx, dy] = [to.x - from.x, to.y - from.y];
  const vertical = Math.abs(dy) > Math.abs(dx);
  const arrow = vertical ? (dy > 0 ? "ArrowDown" : "ArrowUp") : dx > 0 ? "ArrowRight" : "ArrowLeft";
  await page.keyboard.press(arrow);
  await page.waitForFunction(() => document.activeElement?.dataset?.nodeId === "D1.2");
  await page.keyboard.press("Enter");
  await page.locator("#node-dialog .note-dialog-title").filter({ hasText: "Cache" }).waitFor();
  await page.keyboard.press("Escape");
  await page.locator("#node-dialog").waitFor({ state: "hidden" });
  await page.locator("#map .map-body").focus();
  await page.keyboard.press("/");
  await page.waitForFunction(() => document.activeElement?.classList.contains("map-search"));
});

test("a failed load of the layout library offers Retry, and the code still opens", async (t) => {
  const { page } = await openMapped(t);
  await page.route("**/vendor/dagre.min.js", (route) => route.abort());
  await page.locator("#toggle-map").click();
  const retry = page.locator(".map-retry");
  await retry.waitFor();

  await openFile(page, "src/b.rs");
  await page.locator("#code-rows .ln").first().waitFor();

  await page.unroute("**/vendor/dagre.min.js");
  await retry.click();
  await node(page, "Parser").waitFor();
});

test("with no analysis the map names the Analyzer, and Analyze draws one that keeps the selection", async (t) => {
  // The repository the fake agent's analysis cites (tests/fake-agent.mjs, `analysisReply`).
  const files = { "src/a.rs": "fn a() {\n    one();\n}\n", "src/b.rs": "x\ny\nfn two() {\n}\n" };
  const { page } = await openProject(t, "analysis", "new", { files });
  await page.locator("#toggle-map").click();
  const empty = page.locator(".map-empty");
  await empty.waitFor();
  assert.match(await empty.textContent(), /Fake reads the last commit \(HEAD\), read-only/);

  await empty.getByRole("button", { name: "Analyze" }).click();
  await node(page, "Page").waitFor();
  await node(page, "Page").click();
  await page.locator('#code-rows .ln.mapped[data-line="1"]').waitFor();

  // The window regains focus: the analysis is read again, and the node stays selected.
  await page.evaluate(() => dispatchEvent(new Event("focus")));
  await page.waitForFunction(() =>
    document.querySelector(".map-node.selected")?.textContent.includes("Page"),
  );
});

test("a window too narrow for the map and the code shows one at a time", async (t) => {
  const { page } = await openMapped(t);
  await page.setViewportSize({ width: 1100, height: 720 });
  await page.evaluate(() => window.postil.menu("map"));
  await node(page, "Parser").waitFor();
  assert.ok(!(await page.locator("#code").isVisible()), "the map takes the whole pane");
  assert.ok(!(await page.locator("#divider-map").isVisible()));

  await page.evaluate(() => window.postil.menu("map"));
  await page.locator("#map").waitFor({ state: "hidden" });
  assert.ok(await page.locator("#code").isVisible(), "Cmd-Shift-M switches to the code");
  await page.evaluate(() => window.postil.menu("map"));
  // A click opens the node's dialog, so a double-click is not cut short by the switch; its
  // Show in code gives the pane to the code.
  await node(page, "Parser").click();
  const dialog = page.locator("#node-dialog");
  await dialog.waitFor();
  assert.ok(await page.locator("#map").isVisible(), "the map stays until asked");
  await dialog.getByRole("button", { name: "Show in code" }).click();
  await page.locator('#code-rows .ln.mapped[data-line="1"]').waitFor();
  assert.ok(!(await page.locator("#map").isVisible()));
});

test("focus mode in a narrow window shows the code, and Cmd-Shift-M comes back to the map", async (t) => {
  const { page } = await openMapped(t);
  await page.setViewportSize({ width: 1100, height: 720 });
  await page.evaluate(() => window.postil.menu("map"));
  await node(page, "Parser").waitFor();

  await page.evaluate(() => window.postil.menu("focus-mode"));
  await page.locator("#map").waitFor({ state: "hidden" });
  assert.ok(await page.locator("#code").isVisible(), "focus mode shows the code, not nothing");
  await page.evaluate(() => window.postil.menu("map"));
  await node(page, "Parser").waitFor();
  assert.ok(!(await page.locator("#code").isVisible()), "out of focus mode onto the map");

  // Switched to the code first, then focus mode and Cmd-Shift-M: the map, all the same.
  await page.evaluate(() => window.postil.menu("map"));
  await page.locator("#map").waitFor({ state: "hidden" });
  await page.evaluate(() => window.postil.menu("focus-mode"));
  await page.evaluate(() => window.postil.menu("map"));
  await node(page, "Parser").waitFor();
  assert.ok(!(await page.locator("#code").isVisible()));
});

test("an analysis file this build cannot read offers Start over, which clears the map", async (t) => {
  const { page, api } = await openProject(t);
  const { root } = await api("api/repo");
  mkdirSync(join(root, ".git/postil"), { recursive: true });
  writeFileSync(join(root, ".git/postil/analysis.json"), "{ not json");
  await page.locator("#toggle-map").click();
  const error = page.locator(".map-error");
  await error.waitFor();
  assert.match(await error.textContent(), /analysis\.json/);

  const response = page.waitForResponse((each) => each.url().endsWith("api/analysis/start-over"));
  await error.getByRole("button", { name: "Start over" }).click();
  const { trashed } = await (await response).json();
  // Out of the engineer's Trash again, but only the file this test wrote, found in a Trash.
  const inTrash =
    trashed.startsWith(join(homedir(), ".Trash") + sep) || trashed.split(sep).includes(".Trashes");
  assert.ok(inTrash, `${trashed} is in no Trash`);
  assert.equal(readFileSync(trashed, "utf8"), "{ not json");
  unlinkSync(trashed);
  await page.locator(".map-empty").waitFor();
});

// ---- The link shown in the code (docs/review-assist-plan.md, 8.4) --------------------------

// `src/m.rs`, 200 lines `l1` to `l200`, tall enough to scroll a node's lines out of view.
const MARKED_LINES = Array.from({ length: 200 }, (_, index) => `l${index + 1}`);
const MARKED_FILES = { "src/m.rs": `${MARKED_LINES.join("\n")}\n` };
const markedCite = (start, end) => ({
  path: "src/m.rs",
  target: "lines",
  lines: { start, end },
  snippet: MARKED_LINES.slice(start - 1, end).join("\n"),
});
const markedNode = (id, label, anchors) => ({
  id,
  kind: "component",
  label,
  summary: "",
  detail: "",
  anchors,
});
const LONG_LABEL = "A parser whose name runs on far past the room a chip in the code may take";

// An Architecture diagram of src/m.rs: Server on lines 2 to 6, the long-named parser inside it
// on 3 and 4, Old on lines 10 and 11, whose snippet is no longer there (outdated), and Far on
// 150 to 152; and a Data diagram whose Token is on line 3 too.
const MARKED = {
  version: 2,
  diagrams: [
    {
      id: "D1",
      kind: "architecture",
      title: "System",
      run: "R1",
      nodes: [
        markedNode("D1.1", "Server", [markedCite(2, 6)]),
        markedNode("D1.2", LONG_LABEL, [markedCite(3, 4)]),
        markedNode("D1.3", "Old", [{ ...markedCite(10, 11), snippet: "gone\nnow" }]),
        markedNode("D1.4", "Far", [markedCite(150, 152)]),
      ],
      edges: [],
    },
    {
      id: "D2",
      kind: "data",
      title: "Types",
      run: "R1",
      nodes: [{ ...markedNode("D2.1", "Token", [markedCite(3, 3)]), kind: "store" }],
      edges: [],
    },
  ],
};

// `MARKED` with a flow of two steps: Read on lines 20 and 21, then Write on line 25.
const MARKED_FLOW = {
  ...MARKED,
  diagrams: [
    ...MARKED.diagrams,
    {
      id: "D3",
      kind: "flow",
      title: "Load",
      run: "R1",
      nodes: [
        markedNode("D3.1", "Read", [markedCite(20, 21)]),
        markedNode("D3.2", "Write", [markedCite(25, 25)]),
      ],
      edges: [],
    },
  ],
};

// Opens a project of `MARKED_FILES` with `analysis` written, src/m.rs open, its stripes drawn,
// and counts the page's requests for the layout library.
async function openMarked(t, analysis = MARKED) {
  const opened = await openProject(t, "flood", "new", { files: MARKED_FILES });
  await writeAnalysis(opened.api, analysis);
  const dagre = [];
  opened.page.on("request", (request) => {
    if (request.url().endsWith("vendor/dagre.min.js")) dagre.push(request.url());
  });
  // The page read no analysis as it opened: the window's focus reads it again.
  await opened.page.evaluate(() => dispatchEvent(new Event("focus")));
  await openFile(opened.page, "src/m.rs");
  await opened.page.locator('#code-rows .ln[data-line="1"] .map-stripe').waitFor();
  return { ...opened, dagre };
}

const row = (page, index) => page.locator(`#code-rows .ln[data-line="${index}"]`);

// The map's card of node `id`, by its id: one label may hold another ("far" in the parser's).
const card = (page, id) => page.locator(`#map .map-node[data-node-id="${id}"]`);

// Waits until node `id` is the only card with class `state` ("selected" or "ringed").
const onlyNode = (page, state, id) =>
  page.waitForFunction(
    ([name, wanted]) => {
      const cards = [...document.querySelectorAll(`.map-node.${name}`)];
      return cards.length === 1 && cards[0].dataset.nodeId === wanted;
    },
    [state, id],
  );

test("stripes and chips show before the map opens, and a chip opens the map at its node", async (t) => {
  const { page, dagre } = await openMarked(t);
  assert.deepEqual(dagre, [], "the stripes need no layout");
  // Server's five lines and Old's two, solid and dashed.
  assert.equal(await page.locator("#code-rows .map-stripe").count(), 7);
  assert.equal(await page.locator("#code-rows .map-stripe.outdated").count(), 2);
  assert.equal(await row(page, 9).locator(".map-stripe.outdated").count(), 1);
  assert.equal(await row(page, 0).locator(".map-stripe").count(), 0);

  // A chip on each node's first line, the long one cut with its whole name in its title, and
  // the Data diagram's Token behind +1.
  assert.equal(await row(page, 1).locator(".map-chip").textContent(), "Server");
  const long = row(page, 2).locator(".map-chip");
  assert.equal(await long.getAttribute("title"), LONG_LABEL);
  assert.ok((await long.boundingBox()).width <= 180, "cut at 180 px");
  assert.equal(await row(page, 2).locator(".map-chip-more").textContent(), "+1");
  assert.equal(await row(page, 3).locator(".map-chip").count(), 0, "one chip per node");

  // View > Map Markers turns them off, and on again.
  await page.evaluate(() => window.postil.menu("map-markers"));
  assert.equal(await page.locator("#code-rows .map-stripe, #code-rows .map-chip").count(), 0);
  await page.evaluate(() => window.postil.menu("map-markers"));
  assert.equal(await page.locator("#code-rows .map-stripe").count(), 7);

  // The chip opens the map, whose layout loads first, at its node.
  await row(page, 1).locator(".map-chip").click();
  await onlyNode(page, "selected", "D1.1");
  assert.equal(dagre.length, 1);
  // The code column beside the map at 1280 px is under 520 px: chips go, stripes stay.
  assert.ok(!(await row(page, 1).locator(".map-chip").isVisible()), "no chip in a narrow column");
  assert.ok(await row(page, 1).locator(".map-stripe").isVisible());

  // With room, +1 lists the other diagram, and its item opens that diagram at Token.
  await page.setViewportSize({ width: 1600, height: 900 });
  await row(page, 2).locator(".map-chip-more").click();
  await page.locator("#context-menu .menu-item").filter({ hasText: "Token" }).click();
  await onlyNode(page, "selected", "D2.1");
  assert.equal(await page.locator(".map-crumbs b").textContent(), "Types");
});

test("a click on a stripe keeps the line selection, and the map follows the code's line", async (t) => {
  const { page } = await openMarked(t);
  await row(page, 2).locator(".n").click();
  await row(page, 3)
    .locator(".n")
    .click({ modifiers: ["Shift"] });
  await row(page, 4).locator(".map-stripe").click();
  await onlyNode(page, "selected", "D1.1");
  assert.deepEqual(
    await page
      .locator("#code-rows .ln.selected")
      .evaluateAll((rows) => rows.map((each) => each.dataset.line)),
    ["2", "3"],
    "the selection stays",
  );
  // The selection's moving end is on line 4, the parser's.
  await onlyNode(page, "ringed", "D1.2");

  // No selection: the first visible line, followed in the scroll's frame even where the scroll
  // draws no new rows (line 10, inside the rendered window).
  await page.locator("#code").focus();
  await page.keyboard.press("Escape");
  await page.locator(".map-node.ringed").waitFor({ state: "detached" });
  const scrollTo = (line) =>
    page.locator("#code").evaluate((code, at) => {
      const height = parseFloat(getComputedStyle(code).getPropertyValue("--line-height"));
      code.scrollTop = 12 + (at - 0.5) * height;
    }, line);
  await scrollTo(10);
  await onlyNode(page, "ringed", "D1.3");

  // Far moved off the map's screen: scrolled to in the code, it is ringed and panned to, in the
  // same diagram.
  const body = await page.locator("#map .map-body").boundingBox();
  const far = await card(page, "D1.4").boundingBox();
  await page.mouse.move(body.x + body.width / 2, body.y + body.height / 2);
  await page.mouse.wheel(0, -(body.y + body.height - far.y + 20));
  await page.waitForFunction(
    (bottom) =>
      document.querySelector('#map .map-node[data-node-id="D1.4"]').getBoundingClientRect().top >
      bottom,
    body.y + body.height,
  );
  await scrollTo(150);
  await onlyNode(page, "ringed", "D1.4");
  await page.waitForFunction(
    ({ top, bottom }) => {
      const box = document
        .querySelector('#map .map-node[data-node-id="D1.4"]')
        .getBoundingClientRect();
      return box.top >= top && box.bottom <= bottom;
    },
    { top: body.y, bottom: body.y + body.height },
  );
  assert.equal(await page.locator(".map-crumbs b").textContent(), "System");

  // The code never switches the diagram: with Types open, line 150, which only System's Far
  // covers, rings nothing, and line 3 rings Types' Token.
  await page.locator(".map-kind").filter({ hasText: "Data" }).click();
  await page.locator(".map-crumbs b").filter({ hasText: "Types" }).waitFor();
  await scrollTo(150);
  await page.evaluate(
    () => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))),
  );
  assert.equal(await page.locator(".map-node.ringed").count(), 0);
  assert.equal(await page.locator(".map-crumbs b").textContent(), "Types");
  await scrollTo(3);
  await onlyNode(page, "ringed", "D2.1");
  assert.equal(await page.locator(".map-crumbs b").textContent(), "Types");
});

test("a flow's step numbers show in the gutter only while the map shows that flow", async (t) => {
  const { page } = await openMarked(t, MARKED_FLOW);
  const steps = () =>
    page
      .locator("#code-rows .map-step")
      .evaluateAll((all) =>
        all.map((step) => [step.closest(".ln").dataset.line, step.textContent]),
      );
  assert.deepEqual(await steps(), [], "none with the map closed");

  await page.locator("#toggle-map").click();
  await node(page, "Server").waitFor();
  assert.deepEqual(await steps(), [], "none while the Architecture shows");

  const kind = (name) => page.locator(".map-kind").filter({ hasText: name }).click();
  await kind("Flows");
  await node(page, "Read").waitFor();
  // Each step on the first line of its reference: lines 20 and 25.
  assert.deepEqual(await steps(), [
    ["19", "1"],
    ["24", "2"],
  ]);
  await kind("Architecture");
  await node(page, "Server").waitFor();
  assert.deepEqual(await steps(), []);
  await kind("Flows");
  await node(page, "Read").waitFor();
  assert.equal((await steps()).length, 2, "back with the flow");

  await page.locator("#toggle-map").click();
  await page.locator("#map").waitFor({ state: "hidden" });
  assert.deepEqual(await steps(), [], "gone with the map closed");
});

test("a pan towards the code's node stops when another diagram opens", async (t) => {
  const { page } = await openMarked(t);
  await page.setViewportSize({ width: 1600, height: 900 });
  await page.locator("#toggle-map").click();
  await card(page, "D1.4").waitFor();
  // Far off the map's screen, as in the test above.
  const body = await page.locator("#map .map-body").boundingBox();
  const far = await card(page, "D1.4").boundingBox();
  await page.mouse.move(body.x + body.width / 2, body.y + body.height / 2);
  await page.mouse.wheel(0, -(body.y + body.height - far.y + 20));
  await page.waitForFunction(
    (bottom) =>
      document.querySelector('#map .map-node[data-node-id="D1.4"]').getBoundingClientRect().top >
      bottom,
    body.y + body.height,
  );
  // Line 150 starts a 160 ms glide to Far; Data opens two frames in, and Types must stay
  // where it is drawn.
  const moved = await page.evaluate(async () => {
    const code = document.getElementById("code");
    const height = parseFloat(getComputedStyle(code).getPropertyValue("--line-height"));
    code.scrollTop = 12 + 149.5 * height;
    const frame = () => new Promise((resolve) => requestAnimationFrame(resolve));
    await frame();
    await frame();
    const ringed = document.querySelector(".map-node.ringed")?.dataset.nodeId;
    [...document.querySelectorAll(".map-kind")].find((each) => each.textContent === "Data").click();
    const pane = () => document.querySelector("#map .map-viewport").getAttribute("transform");
    const drawn = pane();
    await new Promise((resolve) => setTimeout(resolve, 400));
    const crumb = document.querySelector(".map-crumbs b").textContent;
    return { ringed, crumb, drawn, later: pane() };
  });
  assert.equal(moved.ringed, "D1.4", "the glide had started");
  assert.equal(moved.crumb, "Types");
  assert.equal(moved.later, moved.drawn, "Types stays where it was drawn");
});

test("a note shown in the code moves the ring off the node the map showed", async (t) => {
  const { page, api } = await openMarked(t);
  await page.setViewportSize({ width: 1600, height: 900 });
  const request = { path: "src/m.rs", lines: { start: 3, end: 3 }, snippet: "l3", body: "Three." };
  await api("api/notes", { method: "POST", body: JSON.stringify(request) });
  await page.evaluate(() => dispatchEvent(new Event("focus")));
  await page.locator(".note-item").first().waitFor();
  await page.locator("#toggle-map").click();
  // Far's code shown, a third of the way down: the map follows Far, not the first line.
  await card(page, "D1.4").click();
  await onlyNode(page, "ringed", "D1.4");
  await page.waitForFunction(() => document.getElementById("code").scrollTop > 1000);

  await page.locator(".note-item .note-card, .note-item button").first().click();
  await page.locator("#note-dialog button").filter({ hasText: "Show in code" }).click();
  await page.waitForFunction(() => document.getElementById("code").scrollTop < 200);
  await page.waitForFunction(
    () => !document.querySelector('.map-node.ringed[data-node-id="D1.4"]'),
  );
});

test("a stripe's node keeps the focus when the map, opened by the stripe, reads the analysis", async (t) => {
  const { page } = await openMarked(t);
  await page.setViewportSize({ width: 1600, height: 900 });
  await page.locator("#toggle-map").click();
  await card(page, "D1.1").waitFor();
  await page.locator("#toggle-map").click();
  await page.locator("#map").waitFor({ state: "hidden" });

  // The map opens drawn from the analysis it holds, at once, and draws again once it has read
  // it anew. The card focused by the click is taken in the click's own task, before that.
  await page.evaluate(() => {
    document.querySelector('#code-rows .ln[data-line="9"] .map-stripe').click();
    window.focusedCard = document.activeElement;
  });
  assert.equal(await page.evaluate(() => window.focusedCard.dataset.nodeId), "D1.3");
  await page.waitForFunction(() => !window.focusedCard.isConnected);
  assert.equal(await page.evaluate(() => document.activeElement?.dataset?.nodeId), "D1.3");
});

test("the stripe continues past the composer, and a note on a striped line shows both", async (t) => {
  const { page } = await openMarked(t);
  await row(page, 2).locator(".n").click();
  await page.keyboard.press("c");
  await page.locator("#note-body").waitFor();
  // Down to the parser's next line, once the composer is measured (code-view.js).
  await page.waitForFunction(() => {
    const stripe = document.querySelector('.ln[data-line="2"] .map-stripe.continues');
    const next = document.querySelector('.ln[data-line="3"]');
    if (!stripe || !next) return false;
    return Math.abs(stripe.getBoundingClientRect().bottom - next.getBoundingClientRect().top) <= 1;
  });
  assert.equal(await row(page, 3).locator(".map-stripe:not(.continues)").count(), 1);

  await page.locator("#note-body").fill("On three.");
  await page.keyboard.press("Meta+Enter");
  await row(page, 2).locator(".remark").waitFor();
  // The chip comes before the remark, drawn in either order.
  const order = () =>
    row(page, 2).evaluate((line) =>
      [...line.children]
        .map((child) => child.className)
        .filter((name) => ["map-chip", "map-chip-more", "remark"].includes(name)),
    );
  assert.match(await row(page, 2).getAttribute("class"), /\bnoted\b/);
  assert.equal(await row(page, 2).locator(".map-stripe:not(.continues)").count(), 1);
  assert.deepEqual(await order(), ["map-chip", "map-chip-more", "remark"]);
  await page.evaluate(() => window.postil.menu("map-markers"));
  assert.deepEqual(await order(), ["remark"]);
  await page.evaluate(() => window.postil.menu("map-markers"));
  assert.deepEqual(await order(), ["map-chip", "map-chip-more", "remark"]);
});

test("Cmd-C over striped and chipped lines copies their code alone", async (t) => {
  const { page } = await openMarked(t);
  // What the page puts on the clipboard: the whole lines of a gutter selection through the
  // Clipboard API, a text selection through the copy event (code-view.js, `copyLines`).
  await page.evaluate(() => {
    window.written = [];
    window.copyEvents = [];
    navigator.clipboard.writeText = async (text) => window.written.push(text);
    addEventListener("copy", (event) => {
      window.copyEvents.push(event.clipboardData.getData("text/plain"));
    });
  });
  const taken = (name) => page.evaluate((key) => window[key].splice(0), name);

  await row(page, 1).locator(".n").click();
  await row(page, 2)
    .locator(".n")
    .click({ modifiers: ["Shift"] });
  await page.keyboard.press("Meta+c");
  await page.waitForFunction(() => window.written.length > 0);
  assert.deepEqual(await taken("written"), ["l2\nl3"]);

  // A drag across the code of the same two lines, the chips and +1 between them.
  const from = await row(page, 1).locator(".c").boundingBox();
  const to = await row(page, 2).locator(".c").boundingBox();
  await page.mouse.move(from.x + 1, from.y + from.height / 2);
  await page.mouse.down();
  await page.mouse.move(to.x + to.width - 1, to.y + to.height / 2, { steps: 4 });
  await page.mouse.up();
  await taken("copyEvents");
  await page.keyboard.press("Meta+c");
  await page.waitForFunction(() => window.copyEvents.length > 0);
  assert.deepEqual(await taken("copyEvents"), ["l2\nl3"]);
  assert.deepEqual(await taken("written"), [], "the browser copied the text selection");
});

// ---- Deeper, flows, data, outdated and Update (docs/review-assist-plan.md, 8.5) ------------

// The repository the fake agent's analysis cites (tests/fake-agent.mjs, `analysisReply`).
const ANALYZED_FILES = {
  "src/a.rs": "fn a() {\n    one();\n}\n",
  "src/b.rs": "x\ny\nfn two() {\n}\n",
};

// Opens a project on `ANALYZED_FILES` whose Analyzer is the fake agent's `analysis`.
const openAnalyzed = (t) => openProject(t, "analysis", "new", { files: ANALYZED_FILES });

// The switcher's button for `kind`, by its whole name.
const kindButton = (page, name) => page.locator(".map-kind", { hasText: new RegExp(`^${name}$`) });

// The context menu's item whose label starts with `label`.
const menuItem = (page, label) =>
  page.locator("#context-menu").getByRole("menuitem", { name: new RegExp(`^${label}`) });

test("the menu item on a folder starts a run, and Data and a node's Analyze deeper draw theirs", async (t) => {
  const { page, api } = await openAnalyzed(t);
  await page.locator('#files .dir[data-path="src"]').click({ button: "right" });
  await menuItem(page, "Analyze Deeper").click();

  // No node covers src yet: its diagram is at the top, and the map opens on it.
  await node(page, "Inner").waitFor();
  assert.ok(await page.locator("#map").isVisible());
  assert.equal(await page.locator(".map-crumbs b").textContent(), "Inside");

  // Data has none yet: its card offers Analyze, which draws both kinds.
  await kindButton(page, "Data").click();
  await page.locator(".map-empty").getByRole("button", { name: "Analyze" }).click();
  await node(page, "Note").waitFor();
  assert.equal(await page.locator(".map-crumbs b").textContent(), "Types");

  // A node's dialog asks for its inside, which opens under it.
  await kindButton(page, "Architecture").click();
  await node(page, "Page").dblclick();
  await page.locator("#node-dialog").getByRole("button", { name: "Analyze deeper" }).click();
  await page.locator(".map-crumbs", { hasText: /^System › Inside$/ }).waitFor();
  await node(page, "Inner").waitFor();

  // Back up, Enter on Page goes into the diagram it has.
  await page.locator(".map-crumb", { hasText: /^System$/ }).click();
  await node(page, "Page").focus();
  await page.keyboard.press("Enter");
  await page.locator(".map-crumbs", { hasText: /^System › Inside$/ }).waitFor();

  // A right-click on Store analyzes its inside, which opens under it.
  await page.locator(".map-crumb", { hasText: /^System$/ }).click();
  const store = await node(page, "Store").getAttribute("data-node-id");
  await node(page, "Store").click({ button: "right" });
  await menuItem(page, "Analyze Deeper").click();
  await page.locator(".map-crumbs", { hasText: /^System › Inside$/ }).waitFor();
  const { diagrams } = await api("api/analysis");
  assert.ok(
    diagrams.some((diagram) => diagram.parent === store),
    "a diagram under Store",
  );
});

test("Trace from here on a node starts at its id, its chip clears, and a flow's draft holds Quit and Send", async (t) => {
  const { page, api } = await openAnalyzed(t);
  await page.locator("#toggle-map").click();
  await page.locator(".map-empty").getByRole("button", { name: "Analyze" }).click();
  await node(page, "Page").waitFor();
  const pageId = await node(page, "Page").getAttribute("data-node-id");

  // From the dialog, then the chip's × clears where it starts.
  await node(page, "Page").dblclick();
  await page.locator("#node-dialog").getByRole("button", { name: "Trace from here" }).click();
  const chip = page.locator(".map-flow-chip");
  await chip.locator(".map-flow-chip-label", { hasText: /^From Page$/ }).waitFor();
  await chip.locator(".map-flow-chip-clear").click();
  await chip.waitFor({ state: "hidden" });

  // From the right-click menu on the node; the flow asked for starts at its id.
  await kindButton(page, "Architecture").click();
  await node(page, "Page").click({ button: "right" });
  await menuItem(page, "Trace from Here").click();
  await chip.locator(".map-flow-chip-label", { hasText: /^From Page$/ }).waitFor();
  const question = page.locator(".map-flow-question");
  await question.fill("How is a page drawn?");
  await question.press("Enter");
  await node(page, "send").waitFor();
  const [id] = (await api("api/runs")).runs;
  assert.deepEqual((await api(`api/run?id=${id}`)).analysis, { kind: "flow", of: pageId });

  // A question not asked yet holds Quit and Send, as a note's draft does, and goes once cleared.
  await page.evaluate(() => {
    window.posted = [];
    window.ipc.postMessage = (message) => window.posted.push(message);
  });
  await question.fill("Half a question");
  await page.locator("#code").click();
  await page.evaluate(() => window.postil.menu("quit"));
  const problem = page.locator(".map-flow-problem");
  await problem.filter({ hasText: /^Trace or clear this question before quitting\.$/ }).waitFor();
  await page.waitForFunction(() => document.activeElement?.classList.contains("map-flow-question"));
  assert.deepEqual(await page.evaluate(() => window.posted), []);

  const note = { path: "src/a.rs", lines: { start: 1, end: 1 }, snippet: "fn a() {" };
  await api("api/notes", {
    method: "POST",
    body: JSON.stringify({ ...note, body: "Rename one." }),
  });
  await page.evaluate(() => dispatchEvent(new Event("focus")));
  await page.locator("#notes").getByText("Rename one.").waitFor();
  await page.locator("#send-button").click();
  await problem.filter({ hasText: /^Trace or clear this question before sending\.$/ }).waitFor();
  assert.ok(await page.locator("#prompt-preview").isHidden(), "the send step did not open");

  await question.fill("");
  await page.evaluate(() => window.postil.menu("quit"));
  assert.deepEqual(await page.evaluate(() => window.posted), ["quit"]);
});

test("a flow's question is refused in the page, Trace from Here starts at the file, and Delete takes two clicks", async (t) => {
  const { page, api } = await openAnalyzed(t);
  await page.locator("#toggle-map").click();
  // No Architecture yet: Flows is open to choose all the same.
  await kindButton(page, "Flows").click();
  const question = page.locator(".map-flow-question");
  const problem = page.locator(".map-flow-problem");
  await page.locator(".map-flow-trace").click();
  await problem.filter({ hasText: /^Ask a question/ }).waitFor();
  await question.fill("q".repeat(301));
  await question.press("Enter");
  await problem.filter({ hasText: /not 301\.$/ }).waitFor();
  assert.deepEqual((await api("api/runs")).runs, [], "no run started");

  await openFile(page, "src/a.rs");
  await page.locator('#code-rows .ln[data-line="1"] .c').click({ button: "right" });
  await menuItem(page, "Trace from Here").click();
  await page.locator(".map-flow-chip", { hasText: /^From src\/a\.rs/ }).waitFor();
  assert.ok(await problem.isHidden(), "a fresh question has nothing to say yet");
  await page.waitForFunction(() => document.activeElement?.classList.contains("map-flow-question"));
  await question.fill("How is a note sent?");
  await question.press("Enter");

  // The new flow opens, its steps numbered in the order the Analyzer gave them.
  await node(page, "send").waitFor();
  const steps = await page
    .locator("#map .map-node")
    .evaluateAll((cards) =>
      cards.map(
        (card) =>
          `${card.querySelector(".map-node-step").textContent} ${card.querySelector(".map-node-label").textContent}`,
      ),
    );
  assert.deepEqual(steps, ["1 send", "2 check", "3 apply"]);
  const [id] = (await api("api/runs")).runs;
  const run = await api(`api/run?id=${id}`);
  assert.deepEqual(run.analysis, { kind: "flow", of: "src/a.rs" });

  // One click arms Delete and the pointer leaving its row disarms it; two delete the flow.
  // WebKit gives a clicked button no focus, so it is the row's mouseleave that disarms here,
  // not the button's blur (dom.js, `actOnSecondClick`).
  const row = page.locator(".map-flow-item", {
    has: page.locator(".map-flow-open", { hasText: /^A flow$/ }),
  });
  const remove = row.locator(".map-flow-delete");
  await remove.click();
  await remove.filter({ hasText: /^Delete$/ }).waitFor();
  await question.hover();
  await remove.filter({ hasText: /^×$/ }).waitFor();
  await remove.click();
  await remove.click();
  await row.waitFor({ state: "detached" });
  await page.locator(".map-empty", { hasText: /^No flow yet/ }).waitFor();
  const left = await api("api/analysis");
  assert.deepEqual(
    left.diagrams.filter((diagram) => diagram.kind === "flow"),
    [],
  );
});

test("an outdated node is marked, Not on the map lists 20 files of 205, and Update renews both", async (t) => {
  const { page, api } = await openAnalyzed(t);
  await page.locator("#toggle-map").click();
  await page.locator(".map-empty").getByRole("button", { name: "Analyze" }).click();
  await node(page, "Page").waitFor();

  // Page's line goes, and 205 files no node covers are committed, for the Analyzer to read.
  const { root } = await api("api/repo");
  writeFileSync(join(root, "src/a.rs"), "fn a() {\n}\n");
  mkdirSync(join(root, "extra"));
  for (let n = 0; n < 205; n += 1) {
    writeFileSync(join(root, `extra/e${String(n).padStart(3, "0")}.rs`), "fn e() {}\n");
  }
  execFileSync("git", ["-C", root, "add", "-A"]);
  execFileSync("git", ["-C", root, "commit", "-q", "-m", "Add extra"]);
  await page.evaluate(() => dispatchEvent(new Event("focus")));

  await page.waitForFunction(() =>
    document.querySelector('.map-node[data-node-id="D1.1"]')?.classList.contains("outdated"),
  );
  const unmapped = page.locator(".map-unmapped");
  await unmapped.locator(".map-unmapped-title", { hasText: /^Not on the map · 205$/ }).click();
  assert.equal(await unmapped.locator(".map-unmapped-file").count(), 20);
  await unmapped.locator(".map-unmapped-more", { hasText: /^and 185 more$/ }).waitFor();
  await unmapped
    .locator(".map-unmapped-note", { hasText: /^Update sends at most 200 paths\.$/ })
    .waitFor();

  // The fake Analyzer places e000 alone: the other 204 stay listed, the diagram's run being
  // the one that made it, which did not see them.
  await page.locator(".map-update").click();
  await node(page, "Renewed").waitFor();
  await unmapped.locator(".map-unmapped-title", { hasText: /^Not on the map · 204$/ }).waitFor();
  assert.equal(
    await unmapped.locator(".map-unmapped-file", { hasText: /^extra\/e000\.rs$/ }).count(),
    0,
  );

  // A file in the list opens in the code.
  await unmapped.locator(".map-unmapped-file", { hasText: /^extra\/e001\.rs$/ }).click();
  await page.locator("#crumbs b", { hasText: /^e001\.rs$/ }).waitFor();

  // Analyze deeper of a folder no node covers adds a second Architecture at the top, and the
  // picker chooses between them.
  await page.locator('#files .dir[data-path="extra"]').click({ button: "right" });
  await menuItem(page, "Analyze Deeper").click();
  await page.locator(".map-crumbs b", { hasText: /^Inside$/ }).waitFor();
  const picker = page.locator(".map-top");
  assert.deepEqual(await picker.locator("option").allTextContents(), ["System", "Inside"]);
  await picker.selectOption({ label: "System" });
  await page.locator(".map-crumbs b", { hasText: /^System$/ }).waitFor();
});
