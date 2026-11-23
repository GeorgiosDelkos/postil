// The note detail: a dialog over the window that shows one note and what happened to it, from
// a card's click (docs/design.md, "Notes and their status"). It is drawn from the note and its
// run (status.js), and drawn again when either changes while it is open.

import { diffView, reviewSection } from "./dialog-parts.js";
import { actOnSecondClick, el, statusIcon } from "./dom.js";
import { keptQuestion } from "./question-form.js";
import { foldRun, isLive, isResolved, openQuestion, statusLine } from "./run.js";
import {
  agentSaid,
  fileDiffs,
  fileStats,
  noteDialogKey,
  noteHistory,
  noteStatus,
  runFailure,
  runStatus,
} from "./status.js";

// `dialogEl` is the `<dialog>`. `find(id)` gives `{ note, run }` for a note id, or null once
// the note is gone; `canSend(note)` whether it can be sent now. The callbacks act on the note:
// `showInCode(note)`, `send(note, { review })` (review: open the send step with it alone
// ticked), `remove(note)`, `resolve(note)`, `reopen(note)`, `editNote(note, body)` (resolves
// to whether the new text was saved), `showRun()`, and `answerQuestion(request, content)` for
// the run's open question (`content` null to skip). `agent()` names the agent Send sends to, the
// default one, or null when none is set. `reviewOf(runId)` gives a run's latest review
// (status.js), `reviewers()` the settings' reviewers, `startReview(runId)` starts them, and
// `live()` says whether a run is under way.
export function noteDialog(
  dialogEl,
  {
    find,
    canSend,
    agent,
    showInCode,
    send,
    remove,
    resolve,
    reopen,
    editNote,
    showRun,
    answerQuestion,
    reviewOf,
    reviewers,
    startReview,
    live,
  },
) {
  let shownId = null;
  // The open question's form, kept while it stays open: a redraw must not drop what the
  // engineer has chosen or typed.
  let asking = null;
  // The editor of the note's text while it is open, `{ id, form, hold(action) }`, kept across
  // redraws for the same reason.
  let editing = null;
  // What the dialog was drawn from (`noteDialogKey`): drawn again only when this changes.
  let drawn = null;
  // The note whose dialog body's scroll a redraw keeps.
  let scrolledId = null;

  dialogEl.addEventListener("close", () => {
    shownId = null;
    drawn = null;
    scrolledId = null;
    asking = null;
    editing = null;
  });
  dialogEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.metaKey) {
      event.preventDefault();
      // While the text is edited, Cmd-Enter saves it, from any control: sending would send
      // the old text.
      if (editing) editing.form.requestSubmit();
      else dialogEl.querySelector(".note-dialog-send:not(:disabled)")?.click();
    }
  });

  function open(id) {
    shownId = id;
    drawn = null;
    draw();
    if (!dialogEl.open) dialogEl.showModal();
    dialogEl.querySelector(".note-dialog-show")?.focus();
  }

  // Draws the dialog again if what it shows changed; closes it when its note is gone.
  function draw() {
    if (shownId === null) return;
    const found = find(shownId);
    if (!found) {
      dialogEl.close();
      return;
    }
    const { note, run } = found;
    const review = run ? reviewOf(run.id) : null;
    const key = noteDialogKey(note, run, review, [canSend(note), agent(), reviewers(), live()]);
    if (key === drawn) return;
    drawn = key;
    const same = shownId === scrolledId;
    scrolledId = shownId;
    // The question's form is the same node across redraws, and so is the control that had
    // the focus in it, which gets it back; a button outside it is found by its class.
    const active = document.activeElement;
    const typing = [asking?.form, editing?.form].some((form) => form?.contains(active))
      ? active
      : null;
    const focused = typing ? null : active?.className;
    const scroll = dialogEl.querySelector(".note-dialog-body")?.scrollTop ?? 0;
    dialogEl.replaceChildren(header(note, run), body(note, run), footer(note, run));
    typing?.focus();
    if (focused) dialogEl.getElementsByClassName(focused)[0]?.focus();
    // The same note drawn again keeps its place; another note starts at its top.
    if (same) dialogEl.querySelector(".note-dialog-body").scrollTop = scroll;
  }

  function header(note, run) {
    const status = noteStatus(note, run);
    const head = el("header", "note-dialog-header");
    const title = el("h2", `note-dialog-title ${status.tone}`, status.label);
    title.id = "note-dialog-title";
    const close = el("button", "button note-dialog-close", "esc");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => dialogEl.close());
    const show = el("button", "button note-dialog-show", "Show in code");
    show.type = "button";
    show.addEventListener("click", () => {
      dialogEl.close();
      showInCode(note);
    });
    const actions = el("span", "note-dialog-actions");
    actions.append(show, close);
    head.append(
      statusIcon(status.kind),
      title,
      el("span", "note-dialog-where", where(note)),
      actions,
    );
    return head;
  }

  function body(note, run) {
    const main = el("div", "note-dialog-body");
    // The agent's question comes first: it is why a note needs the engineer (board 9).
    asking = keptQuestion(asking, run, answerQuestion);
    if (asking) main.append(asking.form);
    main.append(noteSection(note));

    const lost = note.anchor === "lost";
    const label = lost ? "Its lines when written; they are no longer in the file" : "Its lines";
    main.append(section(label, snippetView(note)));

    if (run) {
      main.append(section(`What ${run.agent} said about ${note.id}`, said(note, run)));
      // What the run did to the note's file, whole: the card only says the note's status.
      const diffs = fileDiffs(note, run);
      if (diffs.length) {
        const stats = fileStats(note, run);
        const changed = section(`What changed in ${note.path}`, diffsView(diffs));
        changed.firstChild.append(
          el("span", "note-dialog-stats", `+${stats.added} -${stats.removed}`),
        );
        main.append(changed);
      }
      main.append(section(`History · ${run.id} · ${run.agent}`, history(note, run)));
      main.append(reviewPart(note, run));
    }
    return main;
  }

  // The note's text, with Edit; or, while it is edited, the editor.
  function noteSection(note) {
    if (editing?.id === note.id) return section("Your note", editing.form);
    const part = section("Your note", el("div", "note-dialog-note", note.body));
    const button = el("button", "button note-dialog-edit", "Edit");
    button.type = "button";
    button.addEventListener("click", () => startEditing(note));
    part.firstChild.append(button);
    return part;
  }

  function startEditing(note) {
    const form = el("form", "note-edit");
    const input = el("textarea", "note-edit-input");
    input.value = note.body;
    input.rows = 3;
    input.setAttribute("aria-label", `The text of ${note.id}`);
    const cancel = el("button", "button", "Cancel");
    cancel.type = "button";
    const save = el("button", "button primary");
    save.type = "submit";
    save.append("Save", el("kbd", "", "⌘↵"));
    const why = el("span", "note-dialog-why");
    const actions = el("div", "note-edit-actions");
    actions.append(why, cancel, save);
    form.append(input, actions);
    const stop = () => {
      editing = null;
      drawn = null;
      draw();
      // Back to Edit: the focus kept by class could otherwise land on Send.
      dialogEl.querySelector(".note-dialog-edit")?.focus();
    };
    cancel.addEventListener("click", stop);
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const text = input.value.trim();
      if (!text || save.disabled) return;
      save.disabled = true;
      if (await editNote(note, text)) stop();
      else save.disabled = false;
    });
    input.addEventListener("keydown", (event) => {
      // Kept from the dialog: Cmd-Enter there sends the note, and Esc closes it.
      if (event.key === "Enter" && event.metaKey) {
        event.preventDefault();
        event.stopPropagation();
        form.requestSubmit();
      } else if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        stop();
      }
    });
    // A changed text is a draft: `holdDraft` keeps it from being lost.
    const hold = (action) => {
      const text = input.value.trim();
      if (!text || text === note.body) return false;
      why.textContent = `Save or cancel this edit before ${action}.`;
      why.classList.add("failed");
      input.focus();
      return true;
    };
    editing = { id: note.id, form, hold };
    drawn = null;
    draw();
    input.focus();
  }

  // When the note's text is being edited and changed, says so in the editor, focuses it, and
  // returns true: what would end the page (`action`, "quitting") waits for the engineer.
  function holdDraft(action) {
    return editing?.hold(action) ?? false;
  }

  function said(note, run) {
    const line = agentSaid(run, note.id);
    if (line !== null) return el("div", "note-dialog-said", line);
    const words = {
      starting: `${run.agent} is starting.`,
      working: `${run.agent} is working on it. Its line comes when the run ends.`,
      needs: openQuestion(foldRun(run.entries).items)
        ? `${run.agent} is waiting for your answer above.`
        : `${run.agent} is waiting for you in the run.`,
      silent: `Nothing: the run ended without a line for ${note.id}.`,
      stopped: "Nothing: the run was stopped before the agent wrote its summary.",
      failed: run.problem ?? runFailure(run) ?? "The run failed.",
    }[runStatus(note, run).kind];
    const text = el("div", "note-dialog-said muted", words ?? "");
    // A permission request with no allow option waits in the run, not here.
    if (run.status === "waiting" && !openQuestion(foldRun(run.entries).items)) {
      const go = el("button", "button note-dialog-run", "Open the run");
      go.type = "button";
      go.addEventListener("click", () => {
        dialogEl.close();
        showRun();
      });
      text.append(" ", go);
    }
    return text;
  }

  // The review of the note's run: the Review button, and every finding about this note with
  // its verdict, in full (docs/design.md, "Reviews").
  function reviewPart(note, run) {
    const review = reviewOf(run.id);
    const names = reviewers();
    const mine = review ? review.findings.filter((finding) => finding.note === note.id) : [];
    const title = review
      ? `Review of ${run.id} · ${mine.length} about ${note.id}`
      : `Review of ${run.id}`;
    const part = reviewSection(title, {
      reviewed: Boolean(review),
      names,
      busy: live(),
      start: () => startReview(run.id),
      hintClass: "note-dialog-said muted",
    });
    if (!review) return part;
    if (mine.length === 0) {
      part.append(
        el(
          "div",
          "note-dialog-said muted",
          `No finding about ${note.id}; ${review.findings.length} about the run.`,
        ),
      );
      return part;
    }
    for (const finding of mine) {
      const item = el("div", "review-finding");
      const head = el("div", "review-finding-head");
      head.append(
        el("span", "note-id", finding.id),
        el(
          "span",
          `review-verdict ${finding.verdict.word.replace(" ", "-")}`,
          finding.verdict.word,
        ),
      );
      if (finding.path)
        head.append(
          el(
            "span",
            "note-dialog-where",
            finding.line ? `${finding.path}:${finding.line}` : finding.path,
          ),
        );
      item.append(head, el("div", "note-dialog-said", finding.text));
      if (finding.fix) item.append(el("div", "note-dialog-said muted", `Proposed: ${finding.fix}`));
      if (finding.verdict.text)
        item.append(el("div", "note-dialog-said", `Judged: ${finding.verdict.text}`));
      part.append(item);
    }
    return part;
  }

  function footer(note, run) {
    const foot = el("footer", "note-dialog-footer");
    const remove = deleteButton(note);
    const sendable = canSend(note);
    // The default agent, which Send uses, whichever agent ran the note before.
    const to = agent();
    const button = el("button", "button primary note-dialog-send");
    button.type = "button";
    button.disabled = !sendable;
    const again = run && !isLive(run) ? "Send again" : "Send";
    button.append(to ? `${again} to ${to}` : again, el("kbd", "", "⌘↵"));
    button.title =
      "Sends this note alone to the default agent. Shift-click shows the prompt first.";
    button.addEventListener("click", (event) => {
      dialogEl.close();
      send(note, { review: event.shiftKey });
    });
    // A disabled button says why beside it, not only in a tooltip.
    const why = sendable
      ? ""
      : isResolved(note)
        ? "Reopen it to send it again."
        : "Wait for the run under way, or stop it.";
    foot.append(
      remove,
      el("span", "spacer"),
      el("span", "note-dialog-why", why),
      resolveButton(note),
      button,
    );
    return foot;
  }

  // Resolve ends the note's loop, kept with the run it was sent in; Reopen undoes it.
  function resolveButton(note) {
    const done = isResolved(note);
    const button = el("button", "button note-dialog-resolve", done ? "Reopen" : "Resolve");
    button.type = "button";
    button.title = done ? "Back to where it was before you resolved it." : "Mark the note done.";
    button.addEventListener("click", () => (done ? reopen(note) : resolve(note)));
    return button;
  }

  // A note is the engineer's writing, so deleting takes two clicks, as on the card.
  function deleteButton(note) {
    const button = el("button", "button note-dialog-delete", "Delete note");
    button.type = "button";
    actOnSecondClick(button, {
      arm: () => {
        button.textContent = "Click again to delete";
      },
      disarm: () => {
        button.textContent = "Delete note";
      },
      act: () => {
        dialogEl.close();
        remove(note);
      },
    });
    return button;
  }

  return { open, draw, holdDraft, isOpen: () => dialogEl.open };
}

function section(label, content) {
  const part = el("section", "note-dialog-section");
  const title = el("div", "note-dialog-label");
  title.append(el("span", "", label));
  part.append(title, content);
  return part;
}

// Diffs, one block each.
function diffsView(diffs) {
  const view = el("div", "note-dialog-diffs");
  for (const diff of diffs) view.append(diffView(diff));
  return view;
}

// The note's lines as they were when it was written, numbered from where they are now.
function snippetView(note) {
  const view = el("pre", "note-dialog-snippet");
  note.snippet.split("\n").forEach((text, index) => {
    const row = el("span", "row");
    row.append(
      el("span", "number", String(note.lines.start + index)),
      el("span", "text", text || " "),
    );
    view.append(row);
  });
  return view;
}

function where(note) {
  const { start, end } = note.lines;
  return `${note.id} · ${note.path}:${start === end ? start : `${start}-${end}`}`;
}

function history(note, run) {
  const list = el("ol", "note-dialog-history");
  const others = run.notes.filter((id) => id !== note.id);
  const sentWith = others.length ? ` with ${others.join(", ")}` : " alone";
  list.append(el("li", "", `Sent to ${run.agent}${sentWith}`));
  for (const step of noteHistory(note, run)) {
    const item = el("li", step.kind === "tool" ? "tool" : "step");
    if (step.kind === "tool") {
      item.append(el("span", "verb", step.verb), el("span", "target", step.target));
      if (step.state) item.append(el("span", "state", step.state));
    } else {
      item.append(
        el("span", "verb", step.status === "completed" ? "done" : "plan"),
        el("span", "target", step.text),
      );
    }
    list.append(item);
  }
  if (!isLive(run)) list.append(el("li", "end", `Run ended: ${statusLine(run).text}`));
  return list;
}
