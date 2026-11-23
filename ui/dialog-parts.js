// Parts that the note dialog (note-dialog.js) and the run dialog (run-dialog.js) both draw.

import { el } from "./dom.js";
import { partLines } from "./run.js";

// A diff as lines marked removed, added or unchanged (run.js, `partLines`).
export function diffView(diff) {
  const view = el("pre", "tool-diff");
  const marks = { same: "  ", del: "- ", add: "+ " };
  for (const line of partLines(diff)) {
    view.append(el("span", line.kind, `${marks[line.kind]}${line.text}\n`));
  }
  return view;
}

// A Review section, its label `title` and the button that starts the settings' reviewers,
// `names` in order, with `start()`. The button says "Review again" once the run has a review
// (`reviewed`), and is off while `busy` (a run is under way) or with no reviewers set. Below
// the label, before any review, a line says who would review.
export function reviewSection(title, { reviewed, names, busy, start, hintClass }) {
  const part = el("section", "note-dialog-section");
  const label = el("div", "note-dialog-label");
  label.append(el("span", "", title));
  const button = el("button", "button", reviewed ? "Review again" : "Review");
  button.type = "button";
  button.disabled = busy || names.length === 0;
  button.title = names.length
    ? `Reviewers in order: ${names.join(", ")}`
    : "Add reviewers in Settings first";
  button.addEventListener("click", () => {
    // Once: a second click would only be refused, as a run is then under way.
    button.disabled = true;
    start();
  });
  label.append(button);
  part.append(label);
  if (!reviewed) {
    const hint = names.length
      ? `Not reviewed yet. Reviewers: ${names.join(", ")}.`
      : "Not reviewed yet. Add reviewers in Settings.";
    part.append(el("div", hintClass, hint));
  }
  return part;
}
