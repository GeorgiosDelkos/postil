// The file tree in the sidebar: the repository's paths as folders and files, and the rows to
// draw for the folders that are open. Plain data in, plain data out; app.js draws the rows.

// Names sort the way file managers and IDEs list them: case ignored, and digits by value, so
// "file2" comes before "file10". Names equal under those rules fall back to code-unit order,
// so the order never depends on the input order.
const collator = new Intl.Collator("en", { numeric: true, sensitivity: "base" });
const byName = (a, b) => collator.compare(a.name, b.name) || (a.name < b.name ? -1 : 1);

// Builds the tree from repository paths, in any order. A folder is
// { name, path, folders: [folder], files: [{ name, path }] }, children sorted by name; the
// root folder has the path "".
export function buildTree(paths) {
  const root = newFolder("", "");
  // While building, `folders` is a Map by name for lookup; `sortTree` turns it into an array.
  for (const path of paths) {
    const parts = path.split("/");
    let folder = root;
    for (let depth = 0; depth < parts.length - 1; depth += 1) {
      const name = parts[depth];
      let child = folder.folders.get(name);
      if (!child) {
        child = newFolder(name, parts.slice(0, depth + 1).join("/"));
        folder.folders.set(name, child);
      }
      folder = child;
    }
    folder.files.push({ name: parts.at(-1), path });
  }
  return sortTree(root);
}

function newFolder(name, path) {
  return { name, path, folders: new Map(), files: [] };
}

function sortTree(folder) {
  return {
    name: folder.name,
    path: folder.path,
    folders: [...folder.folders.values()].map(sortTree).sort(byName),
    files: folder.files.sort(byName),
  };
}

// The rows to draw, top to bottom: each folder's subfolders, then its files, and inside a
// subfolder only when its path is in `expanded`. A row is
// { kind: "folder" | "file", path, name, depth, open }, `open` only meaningful for folders.
// A folder inside a closed folder keeps its own entry in `expanded`, so it reopens as it was.
export function visibleRows(root, expanded) {
  const rows = [];
  const visit = (folder, depth) => {
    for (const child of folder.folders) {
      const open = expanded.has(child.path);
      rows.push({ kind: "folder", path: child.path, name: child.name, depth, open });
      if (open) visit(child, depth + 1);
    }
    for (const file of folder.files) {
      rows.push({ kind: "file", path: file.path, name: file.name, depth, open: false });
    }
  };
  visit(root, 0);
  return rows;
}

// Whether showing file `path` reveals it in the tree (opens its folders and scrolls to it).
// `previous` is the file shown before, or undefined; `keepView` is true when the same file
// is read again after it may have changed (a refresh, on focus or as a run edits). A file
// opened, or another file, is revealed. The same file read again is not: the refresh would
// open again every folder the engineer closed above it, and scroll the list back to it.
export function shouldReveal(previous, path, { keepView }) {
  return !keepView || previous !== path;
}

// The folders that contain `path`, outermost first: the ones to open to show it.
export function ancestors(path) {
  const parts = path.split("/").slice(0, -1);
  return parts.map((_, index) => parts.slice(0, index + 1).join("/"));
}

// Note counts per folder: each folder's count is the sum over the files below it, at any
// depth. `counts` maps file paths to their note counts.
export function folderCounts(counts) {
  const totals = new Map();
  for (const [path, count] of counts) {
    for (const folder of ancestors(path)) {
      totals.set(folder, (totals.get(folder) ?? 0) + count);
    }
  }
  return totals;
}
