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
      else if (line.startsWith("+++ ") && line !== "+++ /dev/null") file.path = sidePath(line.slice(4), "b/");
      else if (line.startsWith("--- ") && file.status === "deleted") file.path = sidePath(line.slice(4), "a/");
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
