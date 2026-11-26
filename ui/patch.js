// Reads the unified diff `git diff` prints (src/repo.rs, `Repo::diff`) into files and hunks,
// for the changes view. Plain functions, apart from the document, so `node --test` runs them.

import { onLines } from "./note-place.js";

// The files of `patch`, in its order. Each is `{ path, status, binary, added, removed, hunks }`:
// `status` is "added", "deleted" or "modified"; a hunk is `{ header, oldStart, oldCount,
// newStart, newCount, lines }`, with its header's ranges, and a line is `{ kind, old, new,
// text }` with `kind` "context", "add" or "del" and the line numbers it has on each side (null
// on the side it is not on). A side with no lines (a new file's old side, a deleted or emptied
// file's new side) has count 0.
export function parsePatch(patch) {
  const files = [];
  let file = null;
  let hunk = null;
  let old = 0;
  let next = 0;
  for (const line of patch.split("\n")) {
    if (line.startsWith("diff --git ")) {
      file = {
        path: pathOf(line),
        status: "modified",
        binary: false,
        added: 0,
        removed: 0,
        hunks: [],
      };
      files.push(file);
      hunk = null;
      continue;
    }
    if (!file) continue;
    if (hunk === null) {
      // The lines between `diff --git` and the first hunk say what kind of change it is.
      if (line.startsWith("new file mode")) file.status = "added";
      else if (line.startsWith("deleted file mode")) file.status = "deleted";
      else if (line.startsWith("Binary files ")) file.binary = true;
      else if (line.startsWith("+++ ") && line !== "+++ /dev/null")
        file.path = sidePath(line.slice(4), "b/");
      else if (line.startsWith("--- ") && file.status === "deleted")
        file.path = sidePath(line.slice(4), "a/");
    }
    const header = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/.exec(line);
    if (header) {
      old = Number(header[1]);
      next = Number(header[3]);
      // git leaves out a count of 1 (`@@ -0,0 +1 @@`).
      hunk = {
        header: line,
        oldStart: old,
        oldCount: Number(header[2] ?? 1),
        newStart: next,
        newCount: Number(header[4] ?? 1),
        lines: [],
      };
      file.hunks.push(hunk);
      continue;
    }
    if (!hunk) continue;
    if (line.startsWith("+")) {
      hunk.lines.push({ kind: "add", old: null, new: next++, text: line.slice(1) });
      file.added += 1;
    } else if (line.startsWith("-")) {
      hunk.lines.push({ kind: "del", old: old++, new: null, text: line.slice(1) });
      file.removed += 1;
    } else if (line.startsWith(" ")) {
      hunk.lines.push({ kind: "context", old: old++, new: next++, text: line.slice(1) });
    }
    // "\ No newline at end of file", and the empty string after the last newline, are not
    // lines of either side.
  }
  return files;
}

// The path a `diff --git a/x b/x` line names: its second half. Renames are off (`--no-renames`),
// so both halves name the same file; a path with spaces keeps them. A path git quotes (one
// with a control character, a quote or a backslash: src/repo.rs turns quoting of other bytes
// off) is quoted in both halves.
function pathOf(line) {
  const rest = line.slice("diff --git ".length);
  if (rest.startsWith('"')) {
    const second = rest.slice(quotedEnd(rest) + 1).trimStart();
    return sidePath(second, "b/");
  }
  const half = (rest.length - " ".length) / 2;
  return rest.slice(half + " ".length + "b/".length);
}

// A side's path from `a/x`, `b/x` or a quoted `"b/x"`, without its prefix.
function sidePath(text, prefix) {
  const path = unquote(text);
  return path.startsWith(prefix) ? path.slice(prefix.length) : path;
}

// The index just past the closing quote of the quoted string `text` starts with.
function quotedEnd(text) {
  let index = 1;
  while (index < text.length && text[index] !== '"') index += text[index] === "\\" ? 2 : 1;
  return index;
}

// A path as git quotes it: in double quotes, with C escapes and octal bytes of UTF-8. Text
// that does not start with a quote is returned as it is.
export function unquote(text) {
  if (!text.startsWith('"')) return text;
  const escapes = { a: 7, b: 8, t: 9, n: 10, v: 11, f: 12, r: 13 };
  const bytes = [];
  const encoder = new TextEncoder();
  const end = quotedEnd(text);
  for (let index = 1; index < end; index += 1) {
    if (text[index] !== "\\") {
      // A whole code point: a character outside the BMP is two code units, and encoding each
      // half alone gives two U+FFFD.
      const char = String.fromCodePoint(text.codePointAt(index));
      bytes.push(...encoder.encode(char));
      index += char.length - 1;
      continue;
    }
    index += 1;
    const next = text[index];
    if (/[0-7]/.test(next)) {
      bytes.push(Number.parseInt(text.slice(index, index + 3), 8));
      index += 2;
    } else bytes.push(escapes[next] ?? next.charCodeAt(0));
  }
  return new TextDecoder().decode(new Uint8Array(bytes));
}

// Where each of `notes` shows in `file`'s hunks: a map from the new-side line a note's lines
// end on to the notes there. A note whose lines are not in any hunk is left out: the changes
// view shows only the changed lines and their context. A file or folder note has no line to
// show beside, and is left out too.
export function notesByLine(file, notes) {
  const shown = shownLines(file);
  const placed = new Map();
  for (const note of notes) {
    if (!onLines(note) || note.path !== file.path || !shown.has(note.lines.end)) continue;
    placed.set(note.lines.end, [...(placed.get(note.lines.end) ?? []), note]);
  }
  return placed;
}

// The author's report's steps (src/report.rs) laid over `files`, for the walkthrough
// (docs/review-assist.md, "Guided walkthrough and risk"): `{ groups, rest }`. Each group is
// `{ step, number, entries, shownIn }`, one per step in the report's order, `number` from 1.
// `entries` are the `{ file, hunk }` the step's lines overlap, in the patch's order; a file
// with no hunks (a binary one) is one entry with `hunk` null. `rest` holds the entries no step
// overlaps. Every entry is in exactly one place: an entry two steps overlap is the first's,
// and the later step lists the first's number in `shownIn`. A step whose `entries` and
// `shownIn` are both empty names no changed lines.
export function walkthrough(files, steps) {
  const groups = steps.map((step, index) => ({
    step,
    number: index + 1,
    entries: [],
    shownIn: [],
  }));
  const rest = [];
  for (const file of files) {
    for (const hunk of file.hunks.length > 0 ? file.hunks : [null]) {
      const [first, ...later] = groups.filter((group) => covers(group.step, file, hunk));
      if (!first) {
        rest.push({ file, hunk });
        continue;
      }
      first.entries.push({ file, hunk });
      for (const group of later)
        if (!group.shownIn.includes(first.number)) group.shownIn.push(first.number);
    }
  }
  for (const group of groups) group.shownIn.sort((a, b) => a - b);
  return { groups, rest };
}

// Whether `step` covers `hunk` of `file`. By path alone when either side has no lines to
// compare: a step with no `lines` (its file is gone, src/report.rs), a hunk with no new side
// (a deleted or emptied file), or a file with no hunks.
function covers(step, file, hunk) {
  if (step.path !== file.path) return false;
  if (!step.lines || !hunk || hunk.newCount === 0) return true;
  const last = hunk.newStart + hunk.newCount - 1;
  return step.lines.start <= last && hunk.newStart <= step.lines.end;
}

// The rating `hunk` shows: `eyes` when a finding is on one of its new-side lines (`placed`,
// from `findingsByLine`), whatever the report said, since a reviewer disputed it or found
// something there; else `risk`, the step's. A finding carries no field for a dispute (owner,
// 2026-11-25).
export function hunkRisk(risk, hunk, placed) {
  const found = hunk?.lines.some((line) => line.new !== null && placed.has(line.new));
  return found ? "eyes" : risk;
}

// Where each of a review's `findings` on `file` shows: `placed`, a map from the new-side line
// it names to the findings there, as `notesByLine` places notes; and `unplaced`, those with
// no line or one outside the hunks, which show with the file. Findings on other files, or on
// none, are left out.
export function findingsByLine(file, findings) {
  const shown = shownLines(file);
  const placed = new Map();
  const unplaced = [];
  for (const finding of findings) {
    if (finding.path !== file.path) continue;
    if (finding.line === null || finding.line === undefined || !shown.has(finding.line)) {
      unplaced.push(finding);
      continue;
    }
    placed.set(finding.line, [...(placed.get(finding.line) ?? []), finding]);
  }
  return { placed, unplaced };
}

// The new-side line numbers `file`'s hunks show.
function shownLines(file) {
  return new Set(
    file.hunks
      .flatMap((hunk) => hunk.lines.map((line) => line.new))
      .filter((line) => line !== null),
  );
}
