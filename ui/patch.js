// Reads the unified diff `git diff` prints (src/repo.rs, `Repo::diff`) into files and hunks,
// for the changes view. Plain functions, apart from the document, so `node --test` runs them.

// The files of `patch`, in its order. Each is `{ path, status, binary, added, removed, hunks }`:
// `status` is "added", "deleted" or "modified"; a hunk is `{ header, lines }`, and a line is
// `{ kind, old, new, text }` with `kind` "context", "add" or "del" and the line numbers it has
// on each side (null on the side it is not on).
export function parsePatch(patch) {
  const files = [];
  let file = null;
  let hunk = null;
  let old = 0;
  let next = 0;
  for (const line of patch.split("\n")) {
    if (line.startsWith("diff --git ")) {
      file = { path: pathOf(line), status: "modified", binary: false, added: 0, removed: 0, hunks: [] };
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
      else if (line.startsWith("+++ b/")) file.path = line.slice("+++ b/".length);
      else if (line.startsWith("--- a/") && file.status === "deleted") file.path = line.slice("--- a/".length);
    }
    const header = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (header) {
      old = Number(header[1]);
      next = Number(header[2]);
      hunk = { header: line, lines: [] };
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
// so both halves name the same file; a path with spaces keeps them.
function pathOf(line) {
  const rest = line.slice("diff --git a/".length);
  const half = (rest.length - " b/".length) / 2;
  return rest.slice(half + " b/".length);
}

// Where each of `notes` shows in `file`'s hunks: a map from the new-side line a note's lines
// end on to the notes there. A note whose lines are not in any hunk is left out: the changes
// view shows only the changed lines and their context.
export function notesByLine(file, notes) {
  const shown = new Set(file.hunks.flatMap((hunk) => hunk.lines.map((line) => line.new)).filter((line) => line !== null));
  const placed = new Map();
  for (const note of notes) {
    if (note.path !== file.path || !shown.has(note.lines.end)) continue;
    placed.set(note.lines.end, [...(placed.get(note.lines.end) ?? []), note]);
  }
  return placed;
}
