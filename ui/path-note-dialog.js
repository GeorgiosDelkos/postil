// The dialog that writes a note on a whole file or folder, opened from the file list's
// right-click menu or with `c` on a row of the tree (docs/design.md, "The loop", step 3), or
// asks a question about one, with Ask or `q` (docs/review-assist.md, "Ask without changing").
// Like the inline composer for line notes, it saves the note to the tray (or discards it),
// leaving when and how to send it to the tray and the note's detail dialog; a question goes to
// the default agent at once.

import { askWarning } from "./dialog-parts.js";
import { attachDictation } from "./dictation.js";
import { el, icon } from "./dom.js";
import { fileIcon, folderIcon } from "./icons.js";

// `dialogEl` is the `<dialog>`. The callbacks:
// - `add(path, target, body)` stores the note and resolves to null, or the reason it was refused;
// - `ask(path, target, body)` asks the question, and resolves as `add` does;
// - `uncommitted(path, target)` resolves to the working tree's changes since the last commit
//   (`GET api/run/uncommitted`), or null when they cannot be read;
// - `openSend()` opens the tray's send step when `Cmd-Shift-Enter` is pressed with no draft.
export function pathNoteDialog(dialogEl, { add, ask, uncommitted, openSend }) {
  // The path, kind ("file" or "folder") and intent ("change" or "ask") of the note being
  // written, while the dialog is open.
  let writing = null;

  dialogEl.addEventListener("close", () => {
    writing = null;
  });
  dialogEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.metaKey) {
      event.preventDefault();
      if (event.shiftKey) {
        if (!holdDraft("sending")) {
          dialogEl.close();
          openSend?.();
        }
      } else {
        // Cmd-Enter saves the note, or asks, as in the line-note composer.
        dialogEl.querySelector(".path-note-save")?.click();
      }
    }
  });

  function open(path, target, { intent = "change" } = {}) {
    const current = { path, target, intent, saving: false };
    writing = current;
    const asks = intent === "ask";
    const input = el("textarea", "field note-edit-input path-note-input");
    input.rows = 5;
    input.placeholder = asks
      ? `What do you want to know about this ${target}?`
      : `What should the agent do with this ${target}?`;
    input.setAttribute("aria-label", `${asks ? "Question" : "Note"} on the ${target} ${path}`);
    const main = body(input, asks);
    dialogEl.replaceChildren(header(path, target, asks), main, footer(input, asks));
    dialogEl.showModal();
    input.focus();
    if (asks) showWarning(current, main);
  }

  // The working tree's changes since the last commit, above the question once read: the agent
  // reads that commit. Nothing when they cannot be read, which the send step reports.
  async function showWarning(current, main) {
    const reply = await Promise.resolve()
      .then(() => uncommitted(current.path, current.target))
      .catch(() => null);
    const warning = reply ? askWarning(reply) : null;
    if (warning && writing === current) main.prepend(warning);
  }

  function header(path, target, asks) {
    const head = el("header", "note-dialog-header");
    const name = path.split("/").at(-1);
    const words = asks ? `Ask about a ${target}` : `Note on a ${target}`;
    const title = el("h2", "note-dialog-title", words);
    title.id = "path-note-title";
    const close = el("button", "button note-dialog-close", "esc");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => dialogEl.close());
    const actions = el("span", "note-dialog-actions");
    actions.append(close);
    const where = el("span", "note-dialog-where", target === "folder" ? `${path}/` : path);
    head.append(
      icon(target === "folder" ? folderIcon(name, false) : fileIcon(name)),
      title,
      where,
      actions,
    );
    return head;
  }

  function body(input, asks) {
    const main = el("div", "note-dialog-body");
    const part = el("section", "note-dialog-section");
    const label = el("div", "note-dialog-label");
    label.append(el("span", "", asks ? "Your question" : "Your note"));
    part.append(label, input);
    main.append(part);
    return main;
  }

  function footer(input, asks) {
    const foot = el("footer", "note-dialog-footer");
    const why = el("span", "note-dialog-why");
    const discard = el("button", "button path-note-discard", "Discard");
    discard.type = "button";
    discard.addEventListener("click", () => dialogEl.close());

    const save = el("button", "button primary path-note-save");
    save.type = "button";
    save.append(asks ? "Ask" : "Save note", el("kbd", "", "⌘↵"));
    save.title = asks
      ? "Asks the default agent, which only reads, in a thread of its own."
      : "Keeps the note in the tray, to send when you choose.";
    save.addEventListener("click", () => submit(input, why));

    const dictation = attachDictation(input);
    foot.append(
      el("span", "hint", "esc discard"),
      el("span", "spacer"),
      why,
      dictation.button,
      discard,
      save,
    );
    return foot;
  }

  // Stores the note in the tray, or asks the question, and closes the dialog. A refusal is
  // shown in the dialog, which stays open with the text.
  async function submit(input, why) {
    const current = writing;
    if (!current || current.saving) return;
    const asks = current.intent === "ask";
    const text = input.value.trim();
    if (!text) {
      fail(why, asks ? "Write the question first." : "Write the note first.");
      input.focus();
      return;
    }
    current.saving = true;
    let failure;
    try {
      failure = await (asks ? ask : add)(current.path, current.target, text);
    } catch (error) {
      failure = `cannot ${asks ? "ask" : "save the note"}: ${error}`;
    } finally {
      current.saving = false;
    }
    if (failure) {
      // The dialog may have been closed while the request ran; the text is gone with it.
      if (writing === current) fail(why, failure);
      return;
    }
    if (writing === current) dialogEl.close();
  }

  function fail(why, text) {
    why.textContent = text;
    why.classList.add("failed");
  }

  // A draft lives only in the dialog, so anything that would end the page (quitting, switching
  // projects) first sends the engineer back to it. Returns whether there is one.
  function holdDraft(action) {
    if (!dialogEl.open) return false;
    const input = dialogEl.querySelector(".path-note-input");
    if (!input?.value.trim()) return false;
    const draft =
      writing?.intent === "ask" ? "Ask or discard this question" : "Save or discard this note";
    fail(dialogEl.querySelector(".note-dialog-why"), `${draft} before ${action}.`);
    input.focus();
    return true;
  }

  return { open, holdDraft };
}
