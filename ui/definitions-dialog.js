// The list a name with several definitions opens: one-line cards, nearest first (as
// `GET api/definitions` ranks them), each its kind and `path:line`. Up and Down move between
// them, Enter or a click picks one, Escape closes the list.

import { el } from "./dom.js";
import { countLine } from "./definitions.js";

// `dialogEl` is the `<dialog>`.
export function definitionsDialog(dialogEl) {
  // Resolves the open list's promise; null while none is open.
  let settle = null;

  dialogEl.addEventListener("close", () => finish(null));
  dialogEl.addEventListener("keydown", (event) => {
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    event.preventDefault();
    const cards = [...dialogEl.querySelectorAll(".definition-card")];
    const index = cards.indexOf(document.activeElement);
    const next = event.key === "ArrowDown" ? index + 1 : index - 1;
    cards[Math.max(0, Math.min(cards.length - 1, next))]?.focus();
  });

  function finish(definition) {
    const done = settle;
    settle = null;
    done?.(definition);
  }

  // Lists `definitions` of `name`, `total` in all, and resolves to the one picked, or null
  // when the list is closed without a pick.
  function open(name, definitions, total) {
    const head = el("header", "note-dialog-header");
    const title = el("h2", "note-dialog-title", `Definitions of ${name}`);
    title.id = "definitions-title";
    const close = el("button", "button note-dialog-close", "esc");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => dialogEl.close());
    const actions = el("span", "note-dialog-actions");
    actions.append(close);
    head.append(
      title,
      el("span", "note-dialog-where", countLine(definitions.length, total)),
      actions,
    );

    const list = el("div", "definitions-list");
    for (const definition of definitions) {
      const card = el("button", "definition-card");
      card.type = "button";
      card.append(
        el("span", "definition-kind", definition.kind),
        el("span", "definition-place", `${definition.path}:${definition.line}`),
      );
      card.addEventListener("click", () => {
        finish(definition);
        dialogEl.close();
      });
      list.append(card);
    }

    dialogEl.replaceChildren(head, list);
    const picked = new Promise((resolve) => {
      settle = resolve;
    });
    dialogEl.showModal();
    list.firstElementChild?.focus();
    return picked;
  }

  return { open };
}
