// A memory's dialog: its summary, author and times, its body (`get`) and its history
// (`history`) with where each change was anchored (docs/review-assist.md, "Postil shows
// memory"). The body is a teammate's writing: it is drawn as the Markdown subset, node by node
// (dialog-parts.js, `fillMarkdown`), never as HTML, and cut at MEMORY_BODY_LIMIT.
//
// And the Remember dialog (docs/review-assist.md, "Postil writes memory"): the agent's draft of
// a team memory from a note, which the engineer edits and saves. It is a `<dialog>` of its own,
// so a memory opened from "Already recorded?" shows over it and closes back to the draft.

import { fillMarkdown } from "./dialog-parts.js";
import { attachDictation } from "./dictation.js";
import { el } from "./dom.js";
import {
  MEMORY_BODY_LIMIT,
  MEMORY_POLL_MS,
  MEMORY_WRITE_LIMIT,
  NOTE_TYPES,
  anchorWords,
  checkMemoryDraft,
  cutBody,
  isMemoryId,
  isNearDuplicateError,
  memoryAge,
  pollMemory,
  replyProblem,
  shortAuthor,
  utf8Length,
} from "./memory.js";
import { recallView } from "./memory-tab.js";

// How long the summary rests before its duplicate check runs, in ms. Arbitrary: a pause
// between words, so typing a summary asks once, not once per key.
const RECALL_PAUSE_MS = 300;

// From how many UTF-8 bytes the body's size shows beside it: three quarters of what a Save
// sends, so the count appears before the limit is near.
const COUNT_FROM = (MEMORY_WRITE_LIMIT * 3) / 4;

// `dialogEl` is the `<dialog>`. `open(id)` shows memory `id`, asking for it until it is in;
// an id that is not a memory id opens nothing.
export function memoryDialog(dialogEl) {
  let stop = null;
  dialogEl.addEventListener("close", () => {
    stop?.();
    stop = null;
  });

  function open(id) {
    if (!isMemoryId(id)) return;
    stop?.();
    draw(id, { state: "pending" });
    if (!dialogEl.open) {
      dialogEl.showModal();
      dialogEl.focus();
    }
    stop = pollMemory(`api/memory?id=${encodeURIComponent(id)}`, {
      // `open` drew the pending state; drawing it again would replace the header under a
      // click on esc.
      onReply: (reply) => {
        if (reply.state !== "pending") draw(id, reply);
      },
    });
  }

  function draw(id, reply) {
    const ready = reply.state === "ready";
    const body = el("div", "note-dialog-body");
    if (reply.state === "pending") body.append(el("div", "empty", "Loading…"));
    else if (!ready) body.append(el("div", "run-notice", replyProblem(reply)));
    else body.append(...detailParts(reply));
    dialogEl.replaceChildren(header(ready ? reply.summary : id, ready ? reply.noteType : ""), body);
  }

  function header(title, where) {
    const head = el("header", "note-dialog-header");
    const heading = el("h2", "note-dialog-title", title);
    heading.id = "memory-dialog-title";
    const close = el("button", "button", "esc");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => dialogEl.close());
    const actions = el("span", "note-dialog-actions");
    actions.append(close);
    head.append(heading, el("span", "note-dialog-where", where), actions);
    return head;
  }

  return { open };
}

// `dialogEl` is the Remember `<dialog>`. `memoryRepo()` gives the repository's name in team
// memory, or null when it has none (no origin remote); `openMemory(id)` opens a memory's
// dialog; `onDraftStarted()` says a draft run started, which the runs follow; `onSaved(reply)`
// follows a Save, with what hippius-mem wrote. `open({ note, where, body })` drafts from note
// `note` (`where` names it in the header, `body` is its text, what the dialog falls back to
// when the draft cannot even start).
export function rememberDialog(dialogEl, { memoryRepo, openMemory, onDraftStarted, onSaved }) {
  // The dialog while open: what it drafts from, the draft run while it runs, and the form
  // once there is one. Null while closed.
  let shown = null;

  dialogEl.addEventListener("close", () => {
    if (!shown) return;
    for (const stop of shown.stops.values()) stop();
    // Closed while the agent drafts: its run is stopped, as Stop would.
    if (shown.draftRun) {
      void fetch("api/run/cancel", {
        method: "POST",
        body: JSON.stringify({ run: shown.draftRun }),
      });
    }
    shown = null;
  });
  dialogEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.metaKey && shown?.form) {
      event.preventDefault();
      save(false);
    }
  });

  function open({ note, where, body }) {
    if (shown && dialogEl.open) dialogEl.close();
    shown = { note, where, fallback: body, draftRun: null, form: null, stops: new Map() };
    const drafting = shown;
    dialogEl.replaceChildren(
      header(where),
      bodyPart(el("div", "note-dialog-said muted remember-drafting", "Drafting from the thread…")),
    );
    if (!dialogEl.open) {
      dialogEl.showModal();
      dialogEl.focus();
    }
    void startDraft(drafting);
  }

  async function startDraft(drafting) {
    let reply;
    try {
      const response = await fetch("api/memory/draft", {
        method: "POST",
        body: JSON.stringify({ note: drafting.note }),
      });
      reply = response.ok ? await response.json() : { kind: "error", error: await response.text() };
    } catch (error) {
      reply = { kind: "error", error: String(error) };
    }
    if (shown !== drafting) {
      // Closed before the draft's start was answered: its run is stopped, as on a close after.
      if (reply.kind === "run") {
        void fetch("api/run/cancel", { method: "POST", body: JSON.stringify({ run: reply.run }) });
      }
      return;
    }
    if (reply.kind === "fallback") return edit({ summary: "", body: reply.body, notice: null });
    if (reply.kind === "error") {
      return edit({ summary: "", body: drafting.fallback, notice: `No draft: ${reply.error}` });
    }
    drafting.fallback = reply.fallback;
    drafting.draftRun = reply.run;
    onDraftStarted();
    watchDraft(drafting, reply.run);
  }

  // Follows draft run `run` until its view holds the draft, then opens the form with it, or
  // with the note's text and why there is no draft.
  function watchDraft(drafting, run) {
    let total = 0;
    let timer = null;
    let stopped = false;
    drafting.stops.set("draft", () => {
      stopped = true;
      clearTimeout(timer);
    });
    const done = (draft) => {
      drafting.stops.delete("draft");
      drafting.draftRun = null;
      if (draft.state === "ready") edit({ summary: draft.summary, body: draft.body, notice: null });
      else edit({ summary: "", body: drafting.fallback, notice: `No draft: ${draft.problem}` });
    };
    async function ask() {
      try {
        const response = await fetch(`api/run?id=${encodeURIComponent(run)}&after=${total}`);
        if (!response.ok) throw new Error(await response.text());
        const view = await response.json();
        if (stopped) return;
        total = view.total;
        if (view.draft) return done(view.draft);
      } catch (error) {
        if (!stopped) done({ state: "failed", problem: String(error.message ?? error) });
        return;
      }
      timer = setTimeout(ask, MEMORY_POLL_MS);
    }
    void ask();
  }

  // Opens the form with `summary` and `body`, and `notice` above it when there is no draft.
  function edit({ summary, body, notice }) {
    const repo = memoryRepo();
    const form = {
      typeEl: select(
        "remember-type",
        "Type",
        NOTE_TYPES.map((type) => [type, type]),
      ),
      scopeEl: select("remember-scope", "Scope", [
        ["repo", repo ? `This repository (${repo})` : "This repository"],
        ["global", "The whole team"],
      ]),
      summaryEl: el("input", "field remember-summary"),
      bodyEl: el("textarea", "field remember-body"),
      countEl: el("span", "meta remember-count"),
      checkEl: el("div", "remember-check"),
      whyEl: el("span", "note-dialog-why"),
      saveEl: el("button", "button primary remember-save"),
      anywayEl: el("button", "button remember-anyway", "Save anyway"),
      saving: false,
      edited: false,
    };
    shown.form = form;
    form.typeEl.control.value = "gotcha";
    // No origin remote: the repository has no name in team memory to write it under.
    form.scopeEl.control.value = repo ? "repo" : "global";
    form.scopeEl.options[0].disabled = !repo;
    form.summaryEl.value = summary;
    form.summaryEl.setAttribute("aria-label", "Summary");
    form.summaryEl.placeholder = "One line, with the words a teammate would search for";
    form.bodyEl.value = body;
    form.bodyEl.rows = 8;
    form.bodyEl.setAttribute("aria-label", "The memory");
    form.saveEl.type = "button";
    form.saveEl.append("Save", el("kbd", "", "⌘↵"));
    form.saveEl.addEventListener("click", () => save(false));
    form.anywayEl.type = "button";
    form.anywayEl.hidden = true;
    form.anywayEl.title = "hippius-mem holds a memory with a summary like this one";
    form.anywayEl.addEventListener("click", () => save(true));
    form.summaryEl.addEventListener("input", () => {
      form.edited = true;
      checkLater();
    });
    form.bodyEl.addEventListener("input", () => {
      form.edited = true;
      count();
    });
    count();

    const main = bodyPart();
    const check = section("Already recorded?", form.checkEl);
    if (notice) main.append(el("div", "run-notice", notice));
    const summaryRow = el("div", "dictation-field");
    summaryRow.append(form.summaryEl, attachDictation(form.summaryEl).button);
    const bodyLabel = el("div", "note-dialog-label");
    bodyLabel.append(el("span", "", "Memory"), attachDictation(form.bodyEl).button, form.countEl);
    const fields = el("div", "remember-fields");
    fields.append(form.typeEl.label, form.scopeEl.label);
    main.append(
      check,
      fields,
      section("Summary", summaryRow),
      bodyLabel,
      form.bodyEl,
      el("div", "note-dialog-said muted", "Sent as shown above."),
    );
    const cancel = el("button", "button", "Cancel");
    cancel.type = "button";
    cancel.addEventListener("click", () => dialogEl.close());
    const foot = el("footer", "note-dialog-footer");
    foot.append(el("span", "spacer"), form.whyEl, cancel, form.anywayEl, form.saveEl);
    dialogEl.replaceChildren(header(shown.where), main, foot);
    checkNow();
    form.summaryEl.focus();
  }

  // The body's size, shown once it nears what a Save sends.
  function count() {
    const { bodyEl, countEl } = shown.form;
    const bytes = utf8Length(bodyEl.value);
    countEl.textContent = bytes >= COUNT_FROM ? `${bytes} / ${MEMORY_WRITE_LIMIT} bytes` : "";
    countEl.classList.toggle("failed", bytes > MEMORY_WRITE_LIMIT);
  }

  function checkLater() {
    const form = shown.form;
    shown.stops.get("check")?.();
    const timer = setTimeout(checkNow, RECALL_PAUSE_MS);
    shown.stops.set("check", () => clearTimeout(timer));
    form.checkEl.replaceChildren();
  }

  // The duplicate check: a recall of the summary, drawn above the form. One that cannot run
  // says so and leaves Save as it was.
  function checkNow() {
    const form = shown.form;
    shown.stops.get("check")?.();
    shown.stops.delete("check");
    const summary = form.summaryEl.value;
    if (!summary.trim()) {
      form.checkEl.replaceChildren(el("div", "empty", "A summary is checked once written."));
      return;
    }
    const stop = pollMemory(`api/memory/recall?text=${encodeURIComponent(summary)}`, {
      onReply: (reply) => {
        if (reply.state === "pending" || reply.state === "ready") {
          form.checkEl.replaceChildren(
            ...recallView(reply, { nothing: "Nothing like it is recorded.", openMemory }),
          );
          return;
        }
        const reason = replyProblem(reply).replace(/\.$/, "");
        form.checkEl.replaceChildren(el("div", "empty", `Check did not run: ${reason}.`));
      },
    });
    shown.stops.set("check", stop);
  }

  // Saves the text as the form shows it, unchanged; `force` when hippius-mem refused the
  // summary as a near-duplicate and the engineer saves anyway. Ignored while a Save runs, so
  // two clicks write once.
  function save(force) {
    const form = shown?.form;
    if (!form || form.saving) return;
    const draft = {
      summary: form.summaryEl.value,
      body: form.bodyEl.value,
      scope: form.scopeEl.control.value,
    };
    const problem = checkMemoryDraft({ ...draft, memoryRepo: memoryRepo() });
    if (problem) return say(problem);
    const request = { ...draft, noteType: form.typeEl.control.value };
    if (shown.note) request.note = shown.note;
    if (force) request.force = true;
    form.saving = true;
    form.saveEl.disabled = true;
    form.anywayEl.disabled = true;
    say("Saving…", false);
    const saving = shown;
    const stop = pollMemory("api/memory/remember", {
      method: "POST",
      body: JSON.stringify(request),
      onReply: (reply) => {
        if (reply.state === "pending" || shown !== saving) return;
        saving.stops.delete("save");
        form.saving = false;
        form.saveEl.disabled = false;
        form.anywayEl.disabled = false;
        if (reply.state === "ready") {
          dialogEl.close();
          onSaved(reply);
          return;
        }
        const error = replyProblem(reply);
        form.anywayEl.hidden = !isNearDuplicateError(error);
        say(`Not saved: ${error}`);
      },
    });
    shown.stops.set("save", stop);
  }

  function say(text, failed = true) {
    const { whyEl } = shown.form;
    whyEl.textContent = text;
    whyEl.classList.toggle("failed", failed);
  }

  // When the dialog holds text the engineer may want, says so, focuses it, and returns true:
  // what would end the page (`action`, "quitting") waits for the engineer.
  function holdDraft(action) {
    const form = shown?.form;
    if (!dialogEl.open || !form) return false;
    if (!form.edited && !form.summaryEl.value.trim() && !form.bodyEl.value.trim()) return false;
    say(`Save or cancel this memory before ${action}.`);
    form.summaryEl.focus();
    return true;
  }

  function header(where) {
    const head = el("header", "note-dialog-header");
    const heading = el("h2", "note-dialog-title", "Remember");
    heading.id = "remember-dialog-title";
    const close = el("button", "button", "esc");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => dialogEl.close());
    const actions = el("span", "note-dialog-actions");
    actions.append(close);
    head.append(heading, el("span", "note-dialog-where", where), actions);
    return head;
  }

  return { open, holdDraft, isOpen: () => dialogEl.open };
}

function bodyPart(...children) {
  const main = el("div", "note-dialog-body");
  main.append(...children);
  return main;
}

function section(label, content) {
  const part = el("section", "note-dialog-section");
  const title = el("div", "note-dialog-label");
  title.append(el("span", "", label));
  part.append(title, content);
  return part;
}

// A labelled `<select>` of `choices`, `[value, text]` each: `{ label, control, options }`.
function select(className, text, choices) {
  const label = el("label", `remember-field ${className}`);
  const control = el("select", "field");
  const options = choices.map(([value, words]) => {
    const option = el("option", "", words);
    option.value = value;
    return option;
  });
  control.append(...options);
  label.append(el("span", "", text), control);
  return { label, control, options };
}

function ago(ms, now) {
  const age = memoryAge(ms, now);
  return age === "now" ? "just now" : `${age} ago`;
}

function detailParts(detail) {
  const now = Date.now();
  const author = el("span", "", shortAuthor(detail.author));
  author.title = detail.author;
  const said = el("div", "note-dialog-said muted");
  said.append(
    author,
    ` · written ${ago(detail.created, now)} · changed ${ago(detail.updated, now)}`,
  );
  const parts = [said];
  const { text, cut } = cutBody(detail.body);
  const message = el("div", "run-message");
  fillMarkdown(message, text);
  parts.push(message);
  if (cut) {
    const limit = `${MEMORY_BODY_LIMIT / 1024} KiB`;
    parts.push(el("div", "note-dialog-said muted", `Cut at ${limit}.`));
  }
  if (detail.tags.length) parts.push(el("div", "note-dialog-said muted", detail.tags.join(", ")));
  const history = el("ol", "memory-history");
  for (const op of detail.history) {
    const row = el("li", "");
    const who = el("span", "", shortAuthor(op.author));
    who.title = op.author;
    row.append(`${op.kind} · `, who, ` · ${anchorWords(op.anchor)}`);
    history.append(row);
  }
  const section = el("section", "note-dialog-section");
  section.append(el("div", "note-dialog-label", "History"), history);
  parts.push(section);
  return parts;
}
