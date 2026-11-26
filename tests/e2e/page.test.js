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

// A new project on the bridge, `src/a.rs`, `b.rs` and `c.rs` in a fresh repository, open in a
// new page, its agent in the fake agent's `scenario`. `api` calls its routes from the test, as
// the page does. `route` is the bridge's route that makes the site (src/site/tests/page_e2e.rs).
async function openProject(t, scenario = "flood", route = "new") {
  const body = JSON.stringify({ scenario });
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

test("a right-click on a folder writes a note on all of it, and Send sends it at once", async (t) => {
  const { page, api } = await openProject(t);
  await page.locator('#files .dir[data-path="src"]').click({ button: "right" });
  await page
    .locator("#context-menu")
    .getByRole("menuitem", { name: /Add Note/ })
    .click();

  const dialog = page.locator("#path-note-dialog");
  await dialog.locator(".note-dialog-where").filter({ hasText: "src/" }).waitFor();
  await dialog.locator("textarea").fill("Split this module.");
  await dialog.getByRole("button", { name: /Send to Fake/ }).click();
  await dialog.waitFor({ state: "hidden" });

  // Stored first, then sent alone: the run's prompt names the folder, and its card stays.
  await page.locator("#notes .note-place").filter({ hasText: "src/" }).waitFor();
  const { notes } = await api("api/notes");
  assert.deepEqual(
    notes.map(({ id, path, target, body }) => [id, path, target, body]),
    [["N1", "src", "folder", "Split this module."]],
  );
  await page.waitForFunction(() => !document.getElementById("tab-run").hidden);
  const run = await api("api/run");
  assert.deepEqual(run.notes, ["N1"]);
  assert.match(run.prompt, /\[N1\] src\/ \(folder\)\nSplit this module\.\n$/);

  // The folder's row counts the note on it, closed and open: no file below it carries it.
  const folder = page.locator('#files .dir[data-path="src"]');
  await folder.locator(".count").filter({ hasText: "1" }).waitFor();
  await folder.click();
  await page.locator('#files .file[data-path="src/a.rs"]').waitFor();
  await folder.locator(".count").filter({ hasText: "1" }).waitFor();
});

test("c on a file's row writes a note on the file, and Save keeps it without a run", async (t) => {
  const { page, api } = await openProject(t);
  await page.locator('#files .dir[data-path="src"]').click();
  await page.locator('#files .file[data-path="src/b.rs"]').focus();
  await page.keyboard.press("c");

  const dialog = page.locator("#path-note-dialog");
  await dialog.locator(".note-dialog-where").filter({ hasText: "src/b.rs" }).waitFor();
  // An empty note is refused in the dialog, which stays open.
  await dialog.getByRole("button", { name: "Save" }).click();
  await dialog.locator(".note-dialog-why").filter({ hasText: "Write the note first." }).waitFor();
  await dialog.locator("textarea").fill("Fold into a.rs.");
  await dialog.getByRole("button", { name: "Save" }).click();
  await dialog.waitFor({ state: "hidden" });

  await page.locator("#notes .note-place").filter({ hasText: "b.rs" }).waitFor();
  const { notes } = await api("api/notes");
  assert.deepEqual(
    notes.map(({ path, target }) => [path, target]),
    [["src/b.rs", "file"]],
  );
  assert.equal(await api("api/run"), null, "Save sends nothing");
  // The file's row counts its note.
  const count = page.locator('#files .file[data-path="src/b.rs"] .count');
  await count.filter({ hasText: "1" }).waitFor();
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

test("Settings lists the models the agent reports and saves one per slot", async (t) => {
  const { page, api } = await openProject(t, "plain");
  await page.locator("#open-settings").click();
  const dialog = page.locator("#settings");

  // The fake agent reports m, pro, fast and, in a group, deep (tests/fake-agent.mjs).
  const model = (key) => dialog.locator(`[data-key="${key}"]`);
  await model("agent-model").locator('option[value="deep"]').waitFor({ state: "attached" });
  await model("agent-model").selectOption("pro");
  // The same agent reviews twice, each time on a model of its own.
  await dialog.locator('[data-key="reviewer-add"]').selectOption("Fake");
  await model("reviewer-model-0").locator('option[value="fast"]').waitFor({ state: "attached" });
  await dialog.locator('[data-key="reviewer-add"]').selectOption("Fake");
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
