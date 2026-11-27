// The changes view: what a run, or a thread of runs, changed, shown in the code pane over the
// open file (docs/design.md, "The changes view"). One unified diff per file, from the snapshot
// before the first run to the one after the last (`GET api/run/changes`), with each note
// beside the line its lines end on. A click on a line number of the new side opens that file
// at that line with the composer, so the engineer writes the next note on the change itself.
// ui/patch.js reads the diff.
//
// A single run with the author's report shows in Walkthrough order (docs/review-assist.md,
// "Guided walkthrough and risk"): the report's steps, each with the hunks its lines overlap,
// then the hunks no step names. Each hunk shows its rating, `mechanical` hunks start folded,
// and a review's findings show beside their lines. Files order is the diff's own.

import { el } from "./dom.js";
import { findingMemories, findingMemoryLink } from "./memory-tab.js";
import { findingsByLine, hunkRisk, notesByLine, parsePatch, walkthrough } from "./patch.js";
import { segmented } from "./settings-controls.js";

// How many lines of one file, or of one step in Walkthrough order, are drawn before the rest
// waits for a click. Arbitrary: a few screens of a review, well below the thousands a
// regenerated lockfile brings, each of which is four elements.
const LINES_SHOWN = 400;

// `viewEl` is the view's element. `setHeader(title, meta, controls)` fills the code pane's
// header, `controls` being elements to show beside the meta; `notes()` gives the notes;
// `noteCard(note)` builds a note's card, which opens its dialog; `addNote(path, line)` writes a
// note on 1-based line `line` of `path`; `onError(text)` shows a failure; `onClose()` runs when
// the view closes, to give the pane back to the file; `openMemory(id)` opens the team memory a
// finding names.
export function changesView({
  viewEl,
  setHeader,
  notes,
  noteCard,
  addNote,
  onError,
  onClose,
  openMemory,
}) {
  // What is shown: `{ from, to, run }`, or null while the view is closed.
  let shown = null;
  let request = 0;
  // The diff once read: `{ files, sinceNow }`.
  let read = null;
  // "walkthrough" or "files", and whether steps rated `eyes` come first.
  let order = "files";
  let eyesFirst = false;
  // The folded hunks a click opened, kept while the view shows the same changes.
  let opened = new Set();
  // The index of the current step among the drawn `.changes-step` sections.
  let current = 0;
  // The memories the findings name: drawn again where it was as each summary is read.
  const cited = findingMemories({
    onSettled: () => {
      if (!read) return;
      const top = viewEl.scrollTop;
      draw();
      viewEl.scrollTop = top;
    },
  });
  // The `match(file, hunk)` of the latest `focusOn`, which says whether a hunk is wanted (`hunk`
  // null for a file with none): kept while these changes show, so every draw of them, a
  // memory's summary read or another order, draws the wanted hunks whole.
  let focus = null;
  // While `focusOn`'s draw runs, the first element drawn for a wanted hunk, which it scrolls
  // to; `aiming` keeps a later draw from scrolling.
  let aiming = false;
  let target = null;

  // Shows what runs `from` to `to` changed. `run` is `{ report, findings }` for a single run:
  // its author's report (null when it gave none) and its latest review's findings. Null for a
  // thread of several runs, which keeps Files order. With `match`, scrolled to the first hunk
  // it holds (`focusOn`): a click on a touched node of the change map. The same changes shown
  // already, between two snapshots, are not read again, and keep their order and the hunks
  // opened; compared with the files as they are now, they may have changed since, and are.
  async function show(from, to, run = null, { match = null } = {}) {
    if (shown && read && !read.sinceNow && shown.from === from && shown.to === to) {
      // A review that ended since brings its findings.
      shown.run = run;
      viewEl.focus();
      focusOn(match);
      return;
    }
    shown = { from, to, run };
    read = null;
    focus = null;
    opened = new Set();
    current = 0;
    eyesFirst = false;
    order = hasSteps() ? "walkthrough" : "files";
    const asked = ++request;
    viewEl.hidden = false;
    setHeader(title(from, to), "", []);
    viewEl.replaceChildren(el("div", "empty", "Reading the changes…"));
    viewEl.focus();
    let reply;
    try {
      const query = `from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`;
      const response = await fetch(`api/run/changes?${query}`);
      reply = response.ok ? await response.json() : { problem: await response.text() };
    } catch (error) {
      reply = { problem: String(error.message ?? error) };
    }
    // A newer show, or a close, while this was in flight.
    if (asked !== request || !shown) return;
    if (reply.problem) {
      viewEl.replaceChildren(el("div", "empty", reply.problem));
      onError(reply.problem);
      return;
    }
    read = { files: parsePatch(reply.patch), sinceNow: reply.after === null };
    focusOn(match);
  }

  // Draws the view and scrolls it to the first hunk `match(file, hunk)` holds, or to the first
  // file with no hunks it holds (`hunk` null), opened if folded and drawn whole wherever it
  // is, so neither a fold nor the `LINES_SHOWN` limit hides it, in this draw and the later ones
  // of these changes. With no match, draws only.
  function focusOn(match) {
    focus = match;
    target = null;
    if (match) {
      for (const file of read.files)
        for (const hunk of file.hunks) if (match(file, hunk)) opened.add(hunk);
    }
    aiming = true;
    draw();
    aiming = false;
    if (!target) return;
    const step = target.closest(".changes-step");
    if (step) {
      current = steps().indexOf(step);
      markCurrent();
    }
    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
    const padding = parseFloat(getComputedStyle(viewEl).paddingTop) || 0;
    const top =
      target.getBoundingClientRect().top -
      viewEl.getBoundingClientRect().top +
      viewEl.scrollTop -
      padding;
    viewEl.scrollTo({ top, behavior: reduce ? "auto" : "smooth" });
    target = null;
  }

  // Whether `focus` wants one of `entries` (`{ file, hunk }`): they are then drawn whole.
  function holdsFocus(entries) {
    return Boolean(focus) && entries.some(({ file, hunk }) => focus(file, hunk));
  }

  // `element`, drawn for `hunk` of `file`, is what `focusOn`'s draw scrolls to when it is the
  // first that `focus` asks for.
  function aim(element, file, hunk) {
    if (aiming && focus && !target && focus(file, hunk)) target = element;
  }

  // A file's hunks as entries, or the file alone when it has none.
  function entriesOf(file) {
    return file.hunks.length ? file.hunks.map((hunk) => ({ file, hunk })) : [{ file, hunk: null }];
  }

  function hasSteps() {
    return (shown.run?.report?.steps.length ?? 0) > 0;
  }

  function title(from, to) {
    return from === to ? `Changes in ${from}` : `Changes in ${from} to ${to}`;
  }

  function draw() {
    const { files, sinceNow } = read;
    const added = files.reduce((sum, file) => sum + file.added, 0);
    const removed = files.reduce((sum, file) => sum + file.removed, 0);
    const count = files.length === 1 ? "1 file" : `${files.length} files`;
    setHeader(title(shown.from, shown.to), `${count} · +${added} -${removed}`, controls());
    const parts = [];
    // The run is under way, or ended without a snapshot after it: the files as they are now
    // stand in for it, and may hold changes made since.
    if (sinceNow) {
      parts.push(
        el(
          "div",
          "changes-notice",
          "Compared with the files as they are now: the run has no snapshot after it.",
        ),
      );
    }
    if (files.length === 0) parts.push(el("div", "empty", "No file changed."));
    const marks = marksOf(files);
    const guide = shown.run?.report ? walkthrough(files, shown.run.report.steps) : null;
    const loose = (shown.run?.findings ?? []).filter(
      (finding) =>
        !finding.path || (order === "files" && !files.some((f) => f.path === finding.path)),
    );
    if (loose.length) parts.push(findingCards(loose));
    if (order === "walkthrough") parts.push(...walkthroughParts(guide, files, marks));
    else {
      const risks = risksOf(guide);
      for (const file of files) {
        const limit = holdsFocus(entriesOf(file)) ? Infinity : LINES_SHOWN;
        parts.push(fileView(file, marks, risks, limit));
      }
    }
    viewEl.replaceChildren(...parts);
    markCurrent();
  }

  // The header's controls: the order, and in Walkthrough order the rating's. Each gives focus
  // back to the view, which holds the keys, since the header is outside it.
  function controls() {
    const choose = (change) => (value) => {
      change(value);
      current = 0;
      draw();
      viewEl.focus();
    };
    const orderControl = segmented(
      "changes-order",
      [
        ["walkthrough", "Walkthrough"],
        ["files", "Files"],
      ],
      order,
      choose((value) => (order = value)),
      { disabled: !hasSteps() },
    );
    if (shown.run === null) orderControl.title = "A walkthrough shows one run: this is a thread";
    else if (!shown.run.report) orderControl.title = "The run has no report";
    else if (!hasSteps()) orderControl.title = "The run's report names no steps";
    const riskControl = segmented(
      "changes-risk",
      [
        ["all", "All"],
        ["eyes", "Eyes first"],
      ],
      eyesFirst ? "eyes" : "all",
      choose((value) => (eyesFirst = value === "eyes")),
      { disabled: order !== "walkthrough" },
    );
    if (order !== "walkthrough") riskControl.title = "Ratings order the walkthrough's steps";
    return [orderControl, riskControl];
  }

  // Each file's notes and findings by line, read once per draw.
  function marksOf(files) {
    const all = notes();
    const findings = shown.run?.findings ?? [];
    return new Map(
      files.map((file) => [
        file,
        { notes: notesByLine(file, all), findings: findingsByLine(file, findings) },
      ]),
    );
  }

  // The rating of each hunk under the report: its step's, `eyes` for a hunk no step names.
  // Empty without a report, which rates nothing.
  function risksOf(guide) {
    const risks = new Map();
    if (!guide) return risks;
    for (const group of guide.groups)
      for (const { hunk } of group.entries) if (hunk) risks.set(hunk, group.step.risk);
    for (const { hunk } of guide.rest) if (hunk) risks.set(hunk, "eyes");
    return risks;
  }

  function walkthroughParts(guide, files, marks) {
    const { groups, rest } = guide;
    // A finding with no changed line to sit beside shows in the first step that names its
    // file, else with the hunks no step names.
    const looseFindings = new Map(groups.map((group) => [group, []]));
    const restFindings = [];
    for (const finding of shown.run.findings ?? []) {
      if (!finding.path) continue;
      const file = files.find((f) => f.path === finding.path);
      if (file && marks.get(file).findings.placed.has(finding.line)) continue;
      const group = groups.find((g) => g.step.path === finding.path);
      if (group) looseFindings.get(group).push(finding);
      else restFindings.push(finding);
    }
    const named = groups.filter((group) => group.entries.length || group.shownIn.length);
    const empty = groups.filter((group) => !group.entries.length && !group.shownIn.length);
    const isEyes = (group) =>
      group.step.risk === "eyes" ||
      looseFindings.get(group).length > 0 ||
      group.entries.some(({ file, hunk }) => {
        return hunkRisk(group.step.risk, hunk, marks.get(file).findings.placed) === "eyes";
      });
    // Eyes first reorders the steps and hides none.
    const ordered = eyesFirst
      ? [...named.filter(isEyes), ...named.filter((group) => !isEyes(group))]
      : named;
    const limit = (entries) => (holdsFocus(entries) ? Infinity : LINES_SHOWN);
    const parts = ordered.map((group) =>
      stepView(
        group,
        groups.length,
        { marks, findings: looseFindings.get(group) },
        limit(group.entries),
      ),
    );
    if (rest.length || restFindings.length) {
      parts.push(restView(rest, marks, restFindings, limit(rest)));
    }
    // A step that names no changed line is listed last, so a wrong report is visible.
    for (const group of empty) parts.push(stepView(group, groups.length, { marks, findings: [] }));
    return parts;
  }

  function stepView(group, total, { marks, findings }, limit = LINES_SHOWN) {
    const { step, number } = group;
    const part = el("section", "changes-step");
    part.dataset.risk = step.risk;
    const head = el("header", "changes-step-head");
    head.append(el("span", "changes-step-number", `Step ${number} of ${total}`));
    head.append(riskLabel(step.risk));
    const lines = step.lines ? `:${step.lines.start}-${step.lines.end}` : "";
    head.append(el("span", "changes-path", `${step.path}${lines}`));
    part.append(head);
    if (step.why) part.append(el("div", "changes-why", step.why));
    const earlier = group.shownIn.join(", ");
    if (group.shownIn.length && !group.entries.length) {
      part.append(el("div", "changes-notice", `Shown in step ${earlier}.`));
    } else if (group.shownIn.length) {
      part.append(el("div", "changes-notice", `Some of its changes are shown in step ${earlier}.`));
    } else if (!group.entries.length) {
      part.append(el("div", "changes-notice", "Names no changed lines."));
    }
    if (findings.length) part.append(findingCards(findings));
    const risks = new Map(group.entries.map(({ hunk }) => [hunk, step.risk]));
    const more = () =>
      redraw(part, stepView(group, total, { marks, findings }, limit + LINES_SHOWN));
    part.append(...entriesView(group.entries, { marks, risks, limit, more }));
    return part;
  }

  function restView(entries, marks, findings, limit = LINES_SHOWN) {
    const part = el("section", "changes-step");
    part.dataset.risk = "eyes";
    const head = el("header", "changes-step-head");
    head.append(el("span", "changes-step-number", "Not in the walkthrough"), riskLabel("eyes"));
    part.append(head);
    part.append(el("div", "changes-why", "No step of the report names these changes."));
    if (findings.length) part.append(findingCards(findings));
    const risks = new Map(entries.map(({ hunk }) => [hunk, "eyes"]));
    const more = () => redraw(part, restView(entries, marks, findings, limit + LINES_SHOWN));
    part.append(...entriesView(entries, { marks, risks, limit, more }));
    return part;
  }

  // The entries of a step, each file's under its header, up to `limit` lines in all, then a
  // button that calls `more()` to draw them all.
  function entriesView(entries, { marks, risks, limit, more }) {
    const nodes = [];
    let rows = null;
    let file = null;
    let used = 0;
    let total = 0;
    for (const entry of entries) {
      total += entry.hunk?.lines.length ?? 0;
      if (used >= limit && entry.hunk) continue;
      if (entry.file !== file) {
        file = entry.file;
        const box = el("div", "changes-file");
        box.append(fileHead(file));
        rows = el("div", "changes-rows");
        box.append(rows);
        nodes.push(box);
      }
      if (!entry.hunk) {
        rows.append(noHunks(file));
        aim(rows.parentElement, file, null);
        continue;
      }
      const risk = risks.get(entry.hunk) ?? null;
      const drawn = hunkView(file, entry.hunk, {
        risk,
        marks: marks.get(file),
        limit: limit - used,
      });
      used += drawn.used;
      rows.append(drawn.node);
    }
    if (used < total) nodes.push(moreButton(total - used, more));
    return nodes;
  }

  function fileView(file, marks, risks, limit = LINES_SHOWN) {
    const part = el("section", "changes-file");
    part.append(fileHead(file));
    const unplaced = marks.get(file).findings.unplaced;
    if (unplaced.length) part.append(findingCards(unplaced));
    if (file.binary || file.hunks.length === 0) {
      part.append(noHunks(file));
      aim(part, file, null);
      return part;
    }
    const rows = el("div", "changes-rows");
    let used = 0;
    const total = file.hunks.reduce((sum, hunk) => sum + hunk.lines.length, 0);
    for (const hunk of file.hunks) {
      if (used >= limit) break;
      const risk = risks.get(hunk) ?? null;
      const drawn = hunkView(file, hunk, { risk, marks: marks.get(file), limit: limit - used });
      used += drawn.used;
      rows.append(drawn.node);
    }
    part.append(rows);
    if (used < total) {
      part.append(
        moreButton(total - used, () =>
          redraw(part, fileView(file, marks, risks, limit + LINES_SHOWN)),
        ),
      );
    }
    return part;
  }

  function fileHead(file) {
    const head = el("header", "changes-file-head");
    const status = { added: "new", deleted: "deleted", modified: "" }[file.status];
    head.append(el("span", "changes-path", file.path));
    if (status) head.append(el("span", `changes-status ${file.status}`, status));
    head.append(el("span", "changes-stats", `+${file.added} -${file.removed}`));
    return head;
  }

  function noHunks(file) {
    const text = file.binary
      ? "A binary file: its change is not shown."
      : "No line changed: only the file's mode or its existence.";
    return el("div", "changes-notice changes-no-hunks", text);
  }

  function moreButton(count, more) {
    const button = el("button", "button changes-more", `Show the other ${count} lines`);
    button.type = "button";
    button.addEventListener("click", more);
    return button;
  }

  // One hunk in a wrapper of its own, so it can be drawn alone, folded and labeled: its header
  // with its rating (`risk`, null without a report), then up to `limit` of its lines with the
  // notes and findings beside them (`marks`). A `mechanical` hunk starts folded to one line
  // with its counts, unless a note or a finding is on it: nothing a reviewer wrote is hidden.
  // Returns the node and how many of `limit` it used: all its lines when folded, which stand
  // for them.
  function hunkView(file, hunk, { risk, marks, limit }) {
    const shownRisk = risk && hunkRisk(risk, hunk, marks.findings.placed);
    const block = el("div", "changes-hunk-block");
    block.dataset.path = file.path;
    block.dataset.oldStart = String(hunk.oldStart);
    aim(block, file, hunk);
    if (shownRisk) block.dataset.risk = shownRisk;
    const marked = hunk.lines.some((line) => line.new !== null && marks.notes.has(line.new));
    if (shownRisk === "mechanical" && !marked && !opened.has(hunk)) {
      const added = hunk.lines.filter((line) => line.kind === "add").length;
      const removed = hunk.lines.filter((line) => line.kind === "del").length;
      const fold = el("button", "changes-hunk changes-fold");
      fold.type = "button";
      fold.title = "Show the hunk";
      fold.append(
        el("span", "changes-hunk-header", hunk.header),
        el("span", "changes-stats", `+${added} -${removed}`),
        riskLabel(shownRisk),
      );
      fold.addEventListener("click", () => {
        opened.add(hunk);
        redraw(block, hunkView(file, hunk, { risk, marks, limit: Infinity }).node);
      });
      block.append(fold);
      return { node: block, used: hunk.lines.length };
    }
    const head = el("div", "changes-hunk");
    head.append(el("span", "changes-hunk-header", hunk.header));
    if (shownRisk) head.append(riskLabel(shownRisk));
    block.append(head);
    let used = 0;
    for (const line of hunk.lines) {
      if (used >= limit) break;
      used += 1;
      block.append(lineRow(file, line));
      const placedNotes = line.new === null ? undefined : marks.notes.get(line.new);
      const placedFindings = line.new === null ? undefined : marks.findings.placed.get(line.new);
      if (placedNotes || placedFindings) {
        const cards = el("div", "changes-notes");
        for (const note of placedNotes ?? []) cards.append(noteCard(note));
        for (const finding of placedFindings ?? []) cards.append(findingCard(finding));
        block.append(cards);
      }
    }
    return { node: block, used };
  }

  function riskLabel(risk) {
    return el("span", `changes-risk ${risk}`, risk);
  }

  function findingCards(findings) {
    const cards = el("div", "changes-notes changes-loose");
    for (const finding of findings) cards.append(findingCard(finding));
    return cards;
  }

  // A reviewer's finding (src/review.rs) as a card, with the judge's verdict when there is one.
  function findingCard(finding) {
    const card = el("div", "note changes-finding");
    const head = el("div", "note-where");
    const place = finding.path
      ? `${finding.path}${finding.line ? `:${finding.line}` : ""}`
      : "the whole change";
    head.append(
      el("span", "changes-finding-verdict", finding.verdict?.word ?? ""),
      el("span", "note-place", place),
      el("span", "note-id", finding.id),
    );
    card.append(head, el("div", "note-body", finding.text));
    if (finding.fix) card.append(el("div", "changes-finding-fix", `Fix: ${finding.fix}`));
    if (finding.memory) {
      card.append(findingMemoryLink(finding.memory, cited.summary(finding.memory), openMemory));
    }
    return card;
  }

  function lineRow(file, line) {
    const row = el("div", `changes-line ${line.kind}`);
    const sign = { add: "+", del: "-", context: " " }[line.kind];
    row.append(el("span", "changes-number", line.old === null ? "" : String(line.old)));
    // Only a line that is in the file now can carry a new note.
    if (line.new !== null) {
      const button = el("button", "changes-number new", String(line.new));
      button.type = "button";
      button.title = `Write a note on ${file.path}:${line.new}`;
      button.dataset.path = file.path;
      button.dataset.line = String(line.new);
      row.append(button);
    } else row.append(el("span", "changes-number", ""));
    row.append(el("span", "changes-sign", sign), el("span", "changes-text", line.text));
    return row;
  }

  // Puts `node` in the place of `old`, a part drawn again whole. A step replaced loses its
  // mark as the current one, and the button clicked to replace it, which held the focus, is
  // gone: both come back, so `n` and `p` go on from where they were.
  function redraw(old, node) {
    old.replaceWith(node);
    markCurrent();
    viewEl.focus({ preventScroll: true });
  }

  function steps() {
    return [...viewEl.querySelectorAll(".changes-step")];
  }

  function markCurrent() {
    steps().forEach((step, index) => {
      if (index === current) step.setAttribute("aria-current", "step");
      else step.removeAttribute("aria-current");
    });
  }

  let viewPaddingTop = null;
  function paddingTop() {
    if (viewPaddingTop === null)
      viewPaddingTop = parseFloat(getComputedStyle(viewEl).paddingTop) || 0;
    return viewPaddingTop;
  }

  // Moves to the next (`by` 1) or previous (-1) step. Nothing at either end.
  function move(by) {
    const all = steps();
    const next = current + by;
    if (next < 0 || next >= all.length) return;
    // Read layout before mutating `aria-current` so the browser does not force a reflow.
    const top = all[next].offsetTop - paddingTop();
    current = next;
    markCurrent();
    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
    viewEl.scrollTo({ top, behavior: reduce ? "auto" : "smooth" });
  }

  function close() {
    if (!shown) return;
    shown = null;
    read = null;
    focus = null;
    cited.stop();
    request += 1;
    viewEl.hidden = true;
    viewEl.replaceChildren();
    onClose();
  }

  // One listener for the line numbers of the new side, which a file has up to `LINES_SHOWN` of
  // (and all of them once shown whole): one each took 0.8 ms of a 400-line file's 10 ms draw.
  viewEl.addEventListener("click", (event) => {
    const number = event.target.closest(".changes-number.new");
    if (!number) return;
    close();
    addNote(number.dataset.path, Number(number.dataset.line));
  });

  viewEl.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      close();
      return;
    }
    const held = event.metaKey || event.ctrlKey || event.altKey || event.shiftKey;
    if (held || order !== "walkthrough" || !read) return;
    if (event.key === "n" || event.key === "p") {
      event.preventDefault();
      move(event.key === "n" ? 1 : -1);
    }
  });

  // Moves to the next (`by` 1) or previous (-1) step, or hunk when in Files order.
  function step(by) {
    if (!shown || !read) return false;
    if (order === "walkthrough") {
      const all = steps();
      const next = current + by;
      if (next < 0 || next >= all.length) return false;
      move(by);
      return true;
    }
    const blocks = [...viewEl.querySelectorAll(".changes-hunk-block")];
    if (blocks.length === 0) return false;
    const next = Math.max(0, Math.min(blocks.length - 1, current + by));
    if (next === current && by !== 0) return false;
    current = next;
    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
    const padding = parseFloat(getComputedStyle(viewEl).paddingTop) || 0;
    viewEl.scrollTo({
      top: blocks[next].offsetTop - padding,
      behavior: reduce ? "auto" : "smooth",
    });
    return true;
  }

  // Returns `{ path, line }` for the first new-side line of the current walkthrough step or hunk.
  function currentTarget() {
    if (!shown) return null;
    const scope =
      order === "walkthrough"
        ? (steps()[current] ?? viewEl)
        : ([...viewEl.querySelectorAll(".changes-hunk-block")][current] ?? viewEl);
    const number = scope.querySelector(".changes-number.new");
    if (!number) return null;
    return { path: number.dataset.path, line: Number(number.dataset.line) };
  }

  return { show, close, step, currentTarget, isOpen: () => shown !== null };
}
