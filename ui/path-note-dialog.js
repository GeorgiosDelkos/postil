// The dialog that writes a note on a whole file or folder, opened from the file list's
// right-click menu or with `c` on a row of the tree (docs/design.md, "The loop", step 3).
// Save keeps the note in the tray; Send keeps it and sends it alone to the default agent at
// once, as the arrow on a card does. Either way the note is stored first (src/notes.rs, `add`),
// so a send that fails leaves it in the tray to send again.

import { el, icon } from "./dom.js";
import { fileIcon, folderIcon } from "./icons.js";

// `dialogEl` is the `<dialog>`. The callbacks:
// - `add(path, target, body)` stores the note and resolves to `{ note }`, or `{ error }` with
//   the reason it was refused;
// - `send(note, { review })` sends a stored note alone (review: open the send step with it
//   alone ticked instead);
// - `agent()` names the default agent, or null when none is set.
//
// Send is never turned off while a run is under way: the dialog would have to follow the run
// to turn it back on. A send then is refused with the reason, after the note is saved, as the
// arrow on a card is.
export function pathNoteDialog(dialogEl, { add, send, agent }) {
  // The path and kind ("file" or "folder") of the note being written, while the dialog is open.
  let writing = null;

  dialogEl.addEventListener("close", () => {
    writing = null;
  });
  dialogEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.metaKey) {
      event.preventDefault();
      // Cmd-Enter sends, as in the note dialog.
      dialogEl.querySelector(".path-note-send")?.click();
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
    close.setAttribute("aria-label", "Discard");
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
    const save = el("button", "button path-note-save", "Save");
    save.type = "button";
    save.title = "Keeps the note in the tray, to send later.";
    save.addEventListener("click", () => submit(input, why, null));

    const to = agent();
    const sendButton = el("button", "button primary path-note-send");
    sendButton.type = "button";
    sendButton.append(to ? `Send to ${to}` : "Send…", el("kbd", "", "⌘↵"));
    sendButton.title =
      "Keeps the note in the tray and sends it alone to the default agent. " +
      "Shift-click shows the prompt first.";
    sendButton.addEventListener("click", (event) => submit(input, why, { review: event.shiftKey }));

    foot.append(el("span", "hint", "esc discard"), el("span", "spacer"), why, save, sendButton);
    return foot;
  }

  // Stores the note, then sends it when `sending` is given. A refusal is shown in the dialog,
  // which stays open with the text.
  async function submit(input, why, sending) {
    const current = writing;
    if (!current || current.saving) return;
    const text = input.value.trim();
    if (!text) {
      fail(why, "Write the note first.");
      input.focus();
      return;
    }
    current.saving = true;
    const reply = await add(current.path, current.target, text).finally(() => {
      current.saving = false;
    });
    if (reply.error) {
      // The dialog may have been closed while the request ran; the text is gone with it.
      if (writing === current) fail(why, reply.error);
      return;
    }
    if (writing === current) dialogEl.close();
    if (sending) send(reply.note, sending);
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
