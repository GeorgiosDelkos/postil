// The note detail: a dialog over the window that shows one note and what happened to it, from
// a card's click (docs/design.md, "Notes and their status"). It is drawn from the note and its
// run (status.js), and drawn again when either changes while it is open.

import { questionContent, questionFields } from "./question.js";
import { isLive, openQuestion, foldRun, partLines, statusLine } from "./run.js";
import { agentSaid, fileDiffs, fileStats, noteHistory, noteStatus } from "./status.js";

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// The status icon of a card or the dialog: its shape is CSS (`.status-icon.<kind>`).
export function statusIcon(kind) {
  const glyph = { needs: "!", addressed: "✓", silent: "–", failed: "×" }[kind] ?? "";
  const icon = el("span", `status-icon ${kind}`, glyph);
  icon.setAttribute("aria-hidden", "true");
  return icon;
}

// The agent's open question as a form (board 9 on the design canvas): its message, a field
// per form field, Skip and Send answer (Cmd-Enter). `answer(content)` sends the reply: the
// form's values, or `null` to skip. Shared by the note and run dialogs.
export function questionForm(question, agent, answer) {
  const fields = questionFields(question.fields);
  const values = {};
  const form = el("form", "question");
  form.append(el("div", "question-label", `${agent} has a question`), el("div", "question-message", question.message));
  fields.forEach((field, index) => form.append(fieldView(field, index, values)));
  const actions = el("div", "question-actions");
  const skip = el("button", "button", `Skip, let ${agent} decide`);
  skip.type = "button";
  const send = el("button", "button primary");
  send.type = "submit";
  send.append("Send answer", el("kbd", "", "⌘↵"));
  actions.append(skip, el("span", "spacer"), send);
  form.append(actions);
  // Sent once: the buttons stay disabled while the reply is on its way, and the run's next
  // draw replaces the form.
  const reply = (content) => {
    skip.disabled = true;
    send.disabled = true;
    answer(content);
  };
  skip.addEventListener("click", () => reply(null));
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    reply(questionContent(fields, values));
  });
  form.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.metaKey) {
      event.preventDefault();
      event.stopPropagation();
      form.requestSubmit();
    }
  });
  return form;
}

// One field of a question's form; what the engineer enters goes into `values` by key.
function fieldView(field, index, values) {
  const part = el("fieldset", `question-field ${field.kind}`);
  part.append(el("legend", "question-title", field.title));
  if (field.description) part.append(el("div", "question-description", field.description));
  const name = `question-${index}`;
  if (field.kind === "one" || field.kind === "many" || field.kind === "yesno") {
    const options =
      field.kind === "yesno"
        ? [{ value: true, title: "Yes", description: "" }, { value: false, title: "No", description: "" }]
        : field.options;
    const chosen = new Set();
    for (const option of options) {
      const choice = el("label", "question-choice");
      const input = el("input");
      input.type = field.kind === "many" ? "checkbox" : "radio";
      input.name = name;
      input.addEventListener("change", () => {
        if (field.kind === "many") {
          if (input.checked) chosen.add(option.value);
          else chosen.delete(option.value);
          values[field.key] = [...chosen];
        } else values[field.key] = option.value;
      });
      const text = el("span", "question-choice-text");
      text.append(el("span", "question-choice-title", option.title));
      if (option.description) text.append(el("span", "question-choice-description", option.description));
      choice.append(input, text);
      part.append(choice);
    }
    return part;
  }
  const input = field.kind === "text" ? el("textarea") : el("input");
  if (field.kind === "text") input.rows = 2;
  else input.type = "number";
  input.className = "question-input";
  input.setAttribute("aria-label", field.title);
  input.addEventListener("input", () => {
    values[field.key] = input.value;
  });
  part.append(input);
  return part;
}

// `dialogEl` is the `<dialog>`. `find(id)` gives `{ note, run }` for a note id, or null once
// the note is gone; `canSend(note)` whether it can be sent now. The callbacks act on the note:
// `showInCode(note)`, `send(note, { review })` (review: open the send step with it alone
// ticked), `remove(note)`, `showRun()`, and `answerQuestion(request, content)` for the run's
// open question (`content` null to skip). `agent()` names the agent Send sends to, the
// default one, or null when none is set.
export function noteDialog(
  dialogEl,
  { find, canSend, agent, showInCode, send, remove, showRun, answerQuestion },
) {
  let shownId = null;
  // The open question's form, kept while it stays open: a redraw must not drop what the
  // engineer has chosen or typed.
  let asking = null;
  // What the dialog was drawn from: drawn again only when this changes, so a run's every
  // token leaves the dialog, its scroll and its focus alone.
  let drawn = null;

  dialogEl.addEventListener("close", () => {
    shownId = null;
    drawn = null;
    asking = null;
  });
  dialogEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.metaKey) {
      event.preventDefault();
      dialogEl.querySelector(".note-dialog-send:not(:disabled)")?.click();
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
    const key = JSON.stringify([note, run?.id, run?.status, run?.total, canSend(note), agent()]);
    if (key === drawn) return;
    drawn = key;
    const focused = document.activeElement?.className;
    dialogEl.replaceChildren(header(note, run), body(note, run), footer(note, run));
    // A redraw keeps the focus on the button that had it.
    if (focused) dialogEl.getElementsByClassName(focused)[0]?.focus();
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
    head.append(statusIcon(status.kind), title, el("span", "note-dialog-where", where(note)), actions);
    return head;
  }

  function body(note, run) {
    const main = el("div", "note-dialog-body");
    // The agent's question comes first: it is why a note needs the engineer (board 9).
    const question = run && isLive(run) ? openQuestion(foldRun(run.entries).items) : null;
    if (question) {
      if (asking?.request !== question.request) {
        const form = questionForm(question, run.agent, (content) => answerQuestion(question.request, content));
        asking = { request: question.request, form };
      }
      main.append(asking.form);
    } else asking = null;
    main.append(section("Your note", el("div", "note-dialog-note", note.body)));

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
        changed.firstChild.append(el("span", "note-dialog-stats", `+${stats.added} -${stats.removed}`));
        main.append(changed);
      }
      main.append(section(`History · ${run.id} · ${run.agent}`, history(note, run)));
    }
    return main;
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
      failed: run.problem ?? failure(run) ?? "The run failed.",
    }[noteStatus(note, run).kind];
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
        item.append(el("span", "verb", step.status === "completed" ? "done" : "plan"), el("span", "target", step.text));
      }
      list.append(item);
    }
    if (!isLive(run)) list.append(el("li", "end", `Run ended: ${statusLine(run).text}`));
    return list;
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
    button.title = "Sends this note alone to the default agent. Shift-click shows the prompt first.";
    button.addEventListener("click", (event) => {
      dialogEl.close();
      send(note, { review: event.shiftKey });
    });
    // A disabled button says why beside it, not only in a tooltip.
    const why = sendable ? "" : "Wait for the run under way, or stop it.";
    foot.append(remove, el("span", "spacer"), el("span", "note-dialog-why", why), button);
    return foot;
  }

  // A note is the engineer's writing, so deleting takes two clicks, as on the card.
  function deleteButton(note) {
    const button = el("button", "button note-dialog-delete", "Delete note");
    button.type = "button";
    button.addEventListener("click", () => {
      if (button.dataset.confirming) {
        dialogEl.close();
        remove(note);
        return;
      }
      button.dataset.confirming = "1";
      button.textContent = "Click again to delete";
    });
    button.addEventListener("blur", () => {
      delete button.dataset.confirming;
      button.textContent = "Delete note";
    });
    return button;
  }

  return { open, draw, isOpen: () => dialogEl.open };
}

function section(label, content) {
  const part = el("section", "note-dialog-section");
  const title = el("div", "note-dialog-label");
  title.append(el("span", "", label));
  part.append(title, content);
  return part;
}

// Diffs as lines marked removed, added or unchanged (run.js `partLines`), one block each.
function diffsView(diffs) {
  const view = el("div", "note-dialog-diffs");
  const marks = { same: "  ", del: "- ", add: "+ " };
  for (const diff of diffs) {
    const block = el("pre", "tool-diff");
    for (const line of partLines(diff)) block.append(el("span", line.kind, `${marks[line.kind]}${line.text}\n`));
    view.append(block);
  }
  return view;
}

// The note's lines as they were when it was written, numbered from where they are now.
function snippetView(note) {
  const view = el("pre", "note-dialog-snippet");
  note.snippet.split("\n").forEach((text, index) => {
    const row = el("span", "row");
    row.append(el("span", "number", String(note.lines.start + index)), el("span", "text", text || " "));
    view.append(row);
  });
  return view;
}

function where(note) {
  const { start, end } = note.lines;
  return `${note.id} · ${note.path}:${start === end ? start : `${start}-${end}`}`;
}

function failure(run) {
  return run.entries.findLast((entry) => entry.kind === "failed")?.error ?? null;
}
