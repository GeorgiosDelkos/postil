// What a note is on, and how the page names that place. A note's `target` is "lines", "file"
// or "folder" (src/notes.rs, `Target`); only a line note has `lines` and `snippet`. Line notes
// come from a selection in the code, file and folder notes from the file list's right-click
// menu (path-note-dialog.js).

// Whether the note is on lines of its file, and so has `lines` and `snippet`.
export function onLines(note) {
  return note.target === "lines";
}

// Where the note is, short, for a card: "a.rs:3-5", "README.md", or "acp/" for a folder.
export function shortPlace(note) {
  return place(note, note.path.split("/").at(-1));
}

// Where the note is, whole: "src/a.rs:3-5", "README.md", or "src/acp/" for a folder.
export function fullPlace(note) {
  return place(note, note.path);
}

function place(note, name) {
  if (note.target === "folder") return `${name}/`;
  if (note.target === "file") return name;
  const { start, end } = note.lines;
  return `${name}:${start === end ? start : `${start}-${end}`}`;
}

// Whether a change to the file at `path` concerns the note: its own file, or for a folder
// note any file below its folder, at any depth.
export function covers(note, path) {
  return note.target === "folder" ? path.startsWith(`${note.path}/`) : path === note.path;
}

// How the open file's notes are drawn on its lines, as a Map from each covered 0-based line
// to `{ note, noteId, label, resolved, chip, key }` (mirroring `fileMarks` in ui/diagram.js):
// - `note` is the innermost (narrowest span, newest on ties) found line note covering the line;
// - `chip`, on the first line a note wins unless the line before already shows the same note,
//   is `{ noteId, label, resolved, others }`, `others` the other notes covering that line;
// - `key` changes whenever anything drawn on the row does, so `decorateRow` skips unchanged rows.
// A note past the end of a file that shrank draws only up to its end.
export function noteMarks(notes, path, lineCount) {
  const marks = new Map();
  if (!notes || !path || lineCount === 0) return marks;

  const byLine = new Map();
  for (const note of notes) {
    if (!onLines(note) || note.path !== path || note.anchor !== "found") continue;
    if (note.lines.start > lineCount) continue;
    const last = Math.min(note.lines.end, lineCount);
    for (let line = note.lines.start; line <= last; line += 1) {
      if (!byLine.has(line)) byLine.set(line, []);
      byLine.get(line).push(note);
    }
  }

  const shown = new Set();
  const lines = [...byLine.keys()].sort((a, b) => a - b);
  for (const line of lines) {
    const covering = byLine.get(line);
    const winner = innermostNote(covering);
    const before = marks.get(line - 2);
    const opens = !shown.has(winner) && before?.noteId !== winner.id;
    shown.add(winner);
    const label = `${winner.id} · ${winner.body}`;
    const resolved = winner.state === "resolved";
    const others = opens
      ? covering
          .filter((other) => other.id !== winner.id)
          .map((other) => ({
            noteId: other.id,
            body: other.body,
            label: `${other.id} · ${other.body}`,
          }))
      : null;
    const chip = opens ? { noteId: winner.id, label, resolved, others } : null;
    const mark = {
      note: winner,
      noteId: winner.id,
      label,
      resolved,
      chip,
      key: JSON.stringify([
        winner.id,
        winner.body,
        resolved,
        others ? others.map((other) => [other.noteId, other.body]) : null,
      ]),
    };
    marks.set(line - 1, mark);
  }
  return marks;
}

// The note covering a line with the narrowest span; ties go to the later (newer) note.
function innermostNote(covering) {
  let best = covering[0];
  let bestSpan = best.lines.end - best.lines.start;
  for (let index = 1; index < covering.length; index += 1) {
    const candidate = covering[index];
    const span = candidate.lines.end - candidate.lines.start;
    if (span <= bestSpan) {
      best = candidate;
      bestSpan = span;
    }
  }
  return best;
}
