// The dialog that writes a note on a whole file or folder, opened from the file list's
// right-click menu or with `c` on a row of the tree (docs/design.md, "The loop", step 3).
// Like the inline composer for line notes, it saves the note to the tray (or discards it),
// leaving when and how to send it to the tray and the note's detail dialog.

import { el, icon } from "./dom.js";
import { fileIcon, folderIcon } from "./icons.js";

// `dialogEl` is the `<dialog>`. The callbacks:
// - `add(path, target, body)` stores the note and resolves to null, or the reason it was refused;
// - `openSend()` opens the tray's send step when `Cmd-Shift-Enter` is pressed with no draft.
export function pathNoteDialog(dialogEl, { add, openSend }) {
  // The path and kind ("file" or "folder") of the note being written, while the dialog is open.
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
        // Cmd-Enter saves the note, as in the line-note composer.
        dialogEl.querySelector(".path-note-save")?.click();
      }
    }
  });

  function open(path, target) {
    writing = { path, target, saving: false };
    const input = el("textarea", "field note-edit-input path-note-input");
    input.rows = 5;
    input.placeholder = `What should the agent do with this ${target}?`;
    input.setAttribute("aria-label", `Note on the ${target} ${path}`);
    dialogEl.replaceChildren(header(path, target), body(input), footer(input));
    dialogEl.showModal();
    input.focus();
  }

  function header(path, target) {
    const head = el("header", "note-dialog-header");
    const name = path.split("/").at(-1);
    const title = el("h2", "note-dialog-title", `Note on a ${target}`);
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

  function body(input) {
    const main = el("div", "note-dialog-body");
    const part = el("section", "note-dialog-section");
    const label = el("div", "note-dialog-label");
    label.append(el("span", "", "Your note"));
    part.append(label, input);
    main.append(part);
    return main;
  }

  function footer(input) {
    const foot = el("footer", "note-dialog-footer");
    const why = el("span", "note-dialog-why");
    const discard = el("button", "button path-note-discard", "Discard");
    discard.type = "button";
    discard.addEventListener("click", () => dialogEl.close());

    const save = el("button", "button primary path-note-save");
    save.type = "button";
    save.append("Save note", el("kbd", "", "⌘↵"));
    save.title = "Keeps the note in the tray, to send when you choose.";
    save.addEventListener("click", () => submit(input, why));

    foot.append(el("span", "hint", "esc discard"), el("span", "spacer"), why, discard, save);
    return foot;
  }

  // Stores the note in the tray and closes the dialog. A refusal is shown in the dialog, which
  // stays open with the text.
  async function submit(input, why) {
    const current = writing;
    if (!current || current.saving) return;
    const text = input.value.trim();
    if (!text) {
      fail(why, "Write the note first.");
      input.focus();
      return;
    }
    current.saving = true;
    let failure;
    try {
      failure = await add(current.path, current.target, text);
    } catch (error) {
      failure = `cannot save the note: ${error}`;
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
    fail(dialogEl.querySelector(".note-dialog-why"), `Save or discard this note before ${action}.`);
    input.focus();
    return true;
  }

  return { open, holdDraft };
}
