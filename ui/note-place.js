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
