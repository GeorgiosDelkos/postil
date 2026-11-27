// A memory's dialog: its summary, author and times, its body (`get`) and its history
// (`history`) with where each change was anchored (docs/review-assist.md, "Postil shows
// memory"). The body is a teammate's writing: it is drawn as the Markdown subset, node by node
// (dialog-parts.js, `fillMarkdown`), never as HTML, and cut at MEMORY_BODY_LIMIT.

import { fillMarkdown } from "./dialog-parts.js";
import { el } from "./dom.js";
import {
  MEMORY_BODY_LIMIT,
  anchorWords,
  cutBody,
  isMemoryId,
  memoryAge,
  pollMemory,
  replyProblem,
  shortAuthor,
} from "./memory.js";

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
