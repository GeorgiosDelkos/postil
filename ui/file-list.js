// The file list in the sidebar: the repository's files as a tree, or, while the filter has
// text, the best fuzzy matches. ui/tree.js and ui/fuzzy.js have the rules.

import { el, icon, showToast } from "./dom.js";
import { rank } from "./fuzzy.js";
import { fileIcon, folderIcon } from "./icons.js";
import { ancestors, folderCounts, visibleRows } from "./tree.js";

// The most fuzzy matches the file filter lists. Arbitrary: more than fit on a screen, few
// enough that every keystroke rebuilds the list without a visible delay.
const MAX_MATCHES = 100;

// Pixels of indent per level of the tree, and before the first level.
const INDENT = 12;
const INDENT_BASE = 4;

const rowSelector = (path) => `[data-path="${CSS.escape(path)}"]`;

// What a note on a row's whole path is on: "folder" for a folder's row, "file" otherwise.
const rowTarget = (row) => (row.classList.contains("dir") ? "folder" : "file");

// `state` is the page's state: the list reads `repo.files`, `tree`, `expanded`,
// `noteCountByPath` and `file`, and keeps `expanded`. `openFile(path)` opens a file in the code
// pane and resolves to whether it was shown; `codeEl` takes the focus when the filter is left.
// `menu` is the context menu (context-menu.js), and `writeNote(path, target)` opens the dialog
// for a note on the whole file or folder (`target` "file" or "folder").
export function fileList({ state, filesEl, filterEl, codeEl, openFile, menu, writeNote }) {
  // The index of the highlighted row while the file filter has text.
  let activeMatch = 0;

  function renderFiles() {
    const needle = filterEl.value.trim().toLowerCase();
    // A row with keyboard focus is replaced; focus moves to the new row for the same path.
    const focused = filesEl.contains(document.activeElement)
      ? document.activeElement.dataset.path
      : null;

    if (needle) {
      filesEl.removeAttribute("role");
      replaceRows(filesEl, filteredFileRows(needle));
    } else {
      filesEl.setAttribute("role", "tree");
      replaceRows(filesEl, treeRows());
      setTabStop(filesEl.querySelector(".file.open") ?? filesEl.querySelector(".file, .dir"));
    }
    if (focused) filesEl.querySelector(rowSelector(focused))?.focus();
  }

  // Replaces the children one by one instead of spreading them into one call: a folder can
  // hold more children than an engine accepts as arguments (about 65,000 in JavaScriptCore).
  function replaceRows(container, rows) {
    const fragment = document.createDocumentFragment();
    for (const row of rows) fragment.append(row);
    container.replaceChildren(fragment);
  }

  // With a filter: a flat list of the best fuzzy matches, with the matched characters marked.
  // The row at `activeMatch` is the one Enter opens.
  function filteredFileRows(needle) {
    const matches = rank(needle, state.repo.files, MAX_MATCHES);
    activeMatch = Math.min(activeMatch, Math.max(0, matches.length - 1));
    return matches.map(({ path, positions }, index) => {
      const row = fileRow(path, markedPath(path, positions));
      row.prepend(icon(fileIcon(path.split("/").at(-1))));
      row.classList.toggle("active", index === activeMatch);
      return row;
    });
  }

  // `path` as a label with the characters at `positions` wrapped in <mark>.
  function markedPath(path, positions) {
    const label = el("span", "name");
    const marked = new Set(positions);
    for (let index = 0; index < path.length; index += 1) {
      label.append(marked.has(index) ? el("mark", "", path[index]) : path[index]);
    }
    return label;
  }

  // Without a filter: the tree, showing the contents of open folders only. A closed folder
  // carries the count of notes inside it, so collapsing it does not hide that they exist; an
  // open folder, the count of notes on the folder itself.
  function treeRows() {
    const totals = folderCounts(state.noteCountByPath);
    return visibleRows(state.tree, state.expanded).map((row) => {
      const button = row.kind === "folder" ? folderRow(row, totals) : fileRow(row.path, row.name);
      // Files get an empty chevron slot, so names line up with their folder's siblings.
      if (row.kind === "file") button.prepend(el("span", "chevron"), icon(fileIcon(row.name)));
      button.style.paddingLeft = `${INDENT_BASE + INDENT * row.depth}px`;
      button.setAttribute("role", "treeitem");
      button.setAttribute("aria-level", String(row.depth + 1));
      return button;
    });
  }

  function folderRow(row, totals) {
    const button = el("button", "dir");
    button.type = "button";
    button.dataset.path = row.path;
    button.setAttribute("aria-expanded", String(row.open));
    button.append(
      el("span", "chevron"),
      icon(folderIcon(row.name, row.open)),
      el("span", "name", row.name),
    );
    // Open, its files show their own counts, so it shows only the notes on the folder itself.
    const count = row.open ? state.noteCountByPath.get(row.path) : totals.get(row.path);
    if (count) button.append(el("span", "count", String(count)));
    return button;
  }

  // `label` is the text to show, or an element (the filter marks matched characters).
  function fileRow(path, label) {
    const row = el("button", "file");
    row.type = "button";
    row.dataset.path = path;
    row.append(typeof label === "string" ? el("span", "name", label) : label);
    if (state.file?.path === path) row.classList.add("open");

    const count = state.noteCountByPath.get(path);
    if (count) row.append(el("span", "count", String(count)));
    return row;
  }

  // One row of the tree is in the Tab order; the arrow keys move between rows (the ARIA tree
  // pattern). Without this, Tab would stop on every visible file.
  function setTabStop(row) {
    for (const other of filesEl.querySelectorAll(".file, .dir")) {
      if (other !== row) other.tabIndex = -1;
    }
    if (row) row.tabIndex = 0;
  }

  function setFolderOpen(path, open) {
    if (open) state.expanded.add(path);
    else state.expanded.delete(path);
    renderFiles();
    saveFolders();
  }

  // The folders open when the project was last shown (src/folders.rs), read once as it opens.
  // One that cannot be read leaves the tree closed, as before folders were kept.
  async function loadFolders() {
    const response = await fetch("api/folders");
    if (response.ok) state.expanded = new Set((await response.json()).open);
  }

  // Saves the open folders a moment after the last change: a key held on a folder, or opening
  // a file deep in the tree, changes several at once.
  let foldersTimer = null;
  function saveFolders() {
    clearTimeout(foldersTimer);
    foldersTimer = setTimeout(() => {
      const body = JSON.stringify({ open: [...state.expanded].sort() });
      fetch("api/folders", { method: "PUT", body })
        .then(async (response) => {
          if (!response.ok) showToast(await response.text());
        })
        .catch((error) => showToast(String(error)));
    }, 300);
  }

  // Opens the folders around `path` so its row exists, marks it as the open file, and scrolls
  // it into view. Moves the mark without rebuilding the list when no folder had to open.
  function revealFile(previous, path) {
    const closed = ancestors(path).filter((folder) => !state.expanded.has(folder));
    for (const folder of closed) state.expanded.add(folder);

    if (closed.length > 0) {
      renderFiles();
      saveFolders();
    } else {
      if (previous) {
        filesEl.querySelector(`.file${rowSelector(previous)}`)?.classList.remove("open");
      }
      const row = filesEl.querySelector(`.file${rowSelector(path)}`);
      row?.classList.add("open");
      // Tab into the tree lands on the open file, as it does after a rebuild.
      if (row && filesEl.getAttribute("role") === "tree") setTabStop(row);
    }
    filesEl.querySelector(`.file${rowSelector(path)}`)?.scrollIntoView({ block: "nearest" });
  }

  filesEl.addEventListener("click", (event) => {
    const row = event.target.closest(".file, .dir");
    if (!row) return;
    if (row.classList.contains("dir")) {
      setFolderOpen(row.dataset.path, row.getAttribute("aria-expanded") !== "true");
    } else {
      openFile(row.dataset.path);
    }
  });

  // Right-click on a row: a note on the whole file or folder, for an agent.
  filesEl.addEventListener("contextmenu", (event) => {
    const row = event.target.closest(".file, .dir");
    if (!row) return;
    event.preventDefault();
    const target = rowTarget(row);
    // The context-menu key gives no pointer position: the menu opens at the row.
    const box = row.getBoundingClientRect();
    const x = event.clientX || box.left + 24;
    const y = event.clientY || box.bottom;
    menu.open(x, y, [
      {
        label: "Add Note…",
        detail: `on this ${target}`,
        key: "C",
        matches: (key) => key.key === "c" && !key.metaKey,
        run: () => writeNote(row.dataset.path, target),
      },
    ]);
  });

  filesEl.addEventListener("focusin", (event) => {
    const row = event.target.closest(".file, .dir");
    if (row && filesEl.getAttribute("role") === "tree") setTabStop(row);
  });

  // The keys of a file tree in an IDE: up and down move between visible rows; right opens a
  // folder, or moves into an open one; left closes an open folder, or moves to the parent.
  // Enter and Space are the buttons' own: open the file, or toggle the folder.
  filesEl.addEventListener("keydown", (event) => {
    const row = event.target.closest(".file, .dir");
    if (!row || filesEl.getAttribute("role") !== "tree") return;

    const rows = [...filesEl.querySelectorAll(".file, .dir")];
    const index = rows.indexOf(row);
    const path = row.dataset.path;
    const folder = row.classList.contains("dir");
    const open = row.getAttribute("aria-expanded") === "true";

    if (event.key === "ArrowDown") rows[index + 1]?.focus();
    else if (event.key === "ArrowUp") rows[index - 1]?.focus();
    else if (event.key === "Home") rows[0]?.focus();
    else if (event.key === "End") rows.at(-1)?.focus();
    else if (event.key === "ArrowRight" && folder && !open) setFolderOpen(path, true);
    else if (event.key === "ArrowRight" && folder) rows[index + 1]?.focus();
    else if (event.key === "ArrowLeft" && folder && open) setFolderOpen(path, false);
    else if (event.key === "ArrowLeft") {
      const parent = ancestors(path).at(-1);
      if (parent !== undefined) filesEl.querySelector(rowSelector(parent))?.focus();
    } else return;
    event.preventDefault();
  });

  filterEl.addEventListener("input", () => {
    activeMatch = 0;
    renderFiles();
  });
  filterEl.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      // The highlight moves among the rows shown. Ranking every path again on each key took
      // 27 ms in a repository of 50,000 files.
      const rows = filterEl.value.trim() ? filesEl.querySelectorAll(".file") : [];
      if (rows.length === 0) return;
      rows[activeMatch].classList.remove("active");
      const step = event.key === "ArrowDown" ? 1 : -1;
      activeMatch = Math.min(rows.length - 1, Math.max(0, activeMatch + step));
      rows[activeMatch].classList.add("active");
      rows[activeMatch].scrollIntoView({ block: "nearest" });
    } else if (event.key === "Enter") {
      // During IME composition Enter confirms the text; with no filter text there is no match.
      if (event.isComposing || !filterEl.value.trim()) return;
      const active = filesEl.querySelector(".file.active") ?? filesEl.querySelector(".file");
      // Focus moves to the code so the next `c` opens the composer instead of typing a letter.
      if (active) openFile(active.dataset.path).then((shown) => shown && codeEl.focus());
    } else if (event.key === "Escape") {
      // The window's Escape handler would otherwise discard an open draft note.
      event.stopPropagation();
      filterEl.value = "";
      renderFiles();
      codeEl.focus();
    }
  });

  // Opens the folders down to the folder `path` and the folder itself, and puts the focus on
  // its row: where a folder note's dialog shows the folder.
  function showFolder(path) {
    // A filter shows matching files only, never a folder's row.
    filterEl.value = "";
    for (const folder of [...ancestors(path), path]) state.expanded.add(folder);
    renderFiles();
    saveFolders();
    const row = filesEl.querySelector(`.dir${rowSelector(path)}`);
    row?.scrollIntoView({ block: "nearest" });
    row?.focus();
  }

  return {
    render: renderFiles,
    reveal: revealFile,
    showFolder,
    // The file or folder of the row that has the focus, for `c`; null when no row has it.
    focusedRow: () => {
      const row = filesEl.contains(document.activeElement)
        ? document.activeElement.closest(".file, .dir")
        : null;
      return row ? { path: row.dataset.path, target: rowTarget(row) } : null;
    },
    loadFolders,
    // A save still waiting would land in the next project: its folders are dropped instead.
    dropPendingSave: () => clearTimeout(foldersTimer),
  };
}
