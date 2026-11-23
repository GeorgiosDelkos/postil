// The code pane: the open file as a virtual list, the selection of lines, copying, the
// right-click menu's actions on the code, and the composer that writes a note.

import { $, el } from "./dom.js";
import { shouldReveal } from "./tree.js";

// `.code-rows` has this much padding above the first and below the last line (app.css).
const ROWS_PADDING = 12;
// Lines rendered beyond each edge of the viewport. Arbitrary: enough that a fast flick
// does not show blank space before the next render, few enough that a render stays cheap.
const OVERSCAN = 40;
// The composer's height before it has been measured once. A guess: label, two text rows,
// actions, and margins.
const COMPOSER_HEIGHT_GUESS = 138;

// `state` is the page's state: the view keeps `file`, `wanted`, `selection` and `composer`,
// and reads `noteByLine`. `menu` is the context menu (context-menu.js) and `files` the file
// list (file-list.js), which `reveal`s the open file. The callbacks:
// - `indexNotes()` derives `noteByLine` and `noteCountByPath` again after the file or the
//   notes changed;
// - `deleteNote(note)` deletes a note, from the right-click menu;
// - `beforeOpen()` clears what covers the code pane before a file opens there;
// - `headerHeld()` says whether another view holds the pane's header, so a file read again
//   under it leaves the header alone;
// - `onComposerClosed()` redraws what a saved note changes outside the code.
export function codeView({
  state,
  codeEl,
  rowsEl,
  codeEmptyEl,
  menu,
  files,
  indexNotes,
  deleteNote,
  beforeOpen,
  headerHeld,
  onComposerClosed,
}) {
  // One code line's height in px: `--line-height` in app.css until the settings arrive, then
  // what they give (`setLineHeight`).
  let lineHeight = parseFloat(
    getComputedStyle(document.documentElement).getPropertyValue("--line-height"),
  );
  // The rendered window of lines: [first, last).
  let rendered = { first: 0, last: 0 };
  let composerHeight = COMPOSER_HEIGHT_GUESS;

  // The reply to `api/file`: { ok, body }, the body a file view or the reason it failed.
  async function fetchFile(path) {
    const response = await fetch(`api/file?path=${encodeURIComponent(path)}`);
    return { ok: response.ok, body: response.ok ? await response.json() : await response.text() };
  }

  // Opens `path` in the code pane. Returns whether it was shown: not when a draft note would
  // be lost (the composer is focused instead), and not when a newer open overtook this one.
  async function openFile(path) {
    if (state.composer?.text.trim()) {
      state.composer.node.querySelector("textarea").focus();
      return false;
    }
    // The changes view and the send step's prompt cover the code pane: the file opened takes
    // their place.
    beforeOpen();
    state.composer = null;
    state.wanted = path;
    const reply = await fetchFile(path);
    if (state.wanted !== path) return false;
    showFile(path, reply, { keepView: false });
    return true;
  }

  // Draws a reply of `fetchFile`. `keepView` is for the same file read again after it may have
  // changed on disk: the scroll position stays, and so does the selection while it still fits
  // in the file.
  function showFile(path, reply, { keepView }) {
    const previous = state.file?.path;
    const lines = reply.ok ? reply.body.lines.length : 0;
    const selected = Math.max(state.selection?.anchor ?? 0, state.selection?.head ?? 0);
    if (!keepView || selected >= lines) {
      state.selection = null;
    }
    if (reply.ok) {
      state.file = reply.body;
      codeEmptyEl.hidden = true;
      // The gutter fits the file's largest line number, and at least three digits so short
      // files keep one width.
      const digits = Math.max(3, String(lines).length);
      rowsEl.style.setProperty("--gutter-digits", String(digits));
    } else {
      state.file = null;
      codeEmptyEl.hidden = false;
      codeEmptyEl.textContent = reply.body;
    }

    indexNotes();
    // The changes view and the send step's prompt hold the header while they are open over the
    // file; a refresh of the file under them (on focus, or as a run moves on) leaves it alone.
    if (!headerHeld()) renderHeader(path);
    if (shouldReveal(previous, path, { keepView })) files.reveal(previous, path);
    if (!keepView) codeEl.scrollTop = 0;
    renderCode({ force: true });
  }

  function renderHeader(path) {
    const parts = path.split("/");
    const crumbs = $("crumbs");
    crumbs.replaceChildren(parts.slice(0, -1).map((part) => `${part} / `).join(""));
    crumbs.append(el("b", "", parts.at(-1)));
    $("file-meta").textContent = state.file ? `${state.file.lines.length} lines` : "";
  }

  // Gives the pane's header back to the open file when a view over it closes; clears it when
  // no file is open.
  function restoreHeader() {
    if (state.file) renderHeader(state.file.path);
    else {
      $("crumbs").replaceChildren();
      $("file-meta").textContent = "";
    }
  }

  async function showNote(note) {
    if (state.file?.path !== note.path && !(await openFile(note.path))) return;
    // A third of the way down reads better than the very top edge.
    const top = lineTop(note.lines.start - 1) - codeEl.clientHeight / 3;
    codeEl.scrollTo({ top, behavior: "smooth" });
  }

  // ---- A virtual list ---------------------------------------------------------------------
  //
  // Only the lines near the viewport exist as elements. `.code-rows` gets top and bottom
  // padding equal to the height of the lines left out, so the scroll height is that of the
  // whole file. The composer sits in the flow after its last line and adds its height to
  // every line below it.

  function lineCount() {
    return state.file ? state.file.lines.length : 0;
  }

  // The y position of the top of line `index` inside the scroll content.
  function lineTop(index) {
    const pushed = state.composer && index > state.composer.end ? composerHeight : 0;
    return ROWS_PADDING + index * lineHeight + pushed;
  }

  // The line at scroll offset `y`: the inverse of `lineTop`.
  function lineAt(y) {
    const composer = state.composer;
    if (composer) {
      const composerTop = lineTop(composer.end) + lineHeight;
      if (y >= composerTop + composerHeight) y -= composerHeight;
      else if (y >= composerTop) return composer.end;
    }
    const index = Math.floor((y - ROWS_PADDING) / lineHeight);
    return Math.max(0, Math.min(lineCount(), index));
  }

  function renderCode({ force = false } = {}) {
    const count = lineCount();
    const visibleFirst = lineAt(codeEl.scrollTop);
    const visibleLast = Math.min(count, lineAt(codeEl.scrollTop + codeEl.clientHeight) + 1);

    // Scrolling inside the rendered window needs no new elements.
    const { first, last } = rendered;
    const covered = visibleFirst >= first && visibleLast <= last;
    const nearEdge =
      (visibleFirst - first < OVERSCAN / 2 && first > 0) ||
      (last - visibleLast < OVERSCAN / 2 && last < count);
    if (!force && covered && !nearEdge) return;

    const newFirst = Math.max(0, visibleFirst - OVERSCAN);
    const newLast = Math.min(count, visibleLast + OVERSCAN);
    rendered = { first: newFirst, last: newLast };

    const fragment = document.createDocumentFragment();
    for (let index = newFirst; index < newLast; index += 1) {
      fragment.append(lineRow(index));
      if (state.composer?.end === index) fragment.append(state.composer.node);
    }
    rowsEl.replaceChildren(fragment);
    setRowsPadding();
  }

  // Stands in for the lines that are not rendered, and for the composer when its line is
  // outside the rendered window.
  function setRowsPadding() {
    const { first, last } = rendered;
    const composer = state.composer;
    const above = composer && composer.end < first ? composerHeight : 0;
    const below = composer && composer.end >= last ? composerHeight : 0;
    rowsEl.style.paddingTop = `${ROWS_PADDING + first * lineHeight + above}px`;
    rowsEl.style.paddingBottom = `${ROWS_PADDING + (lineCount() - last) * lineHeight + below}px`;
  }

  function lineRow(index) {
    const row = el("div", "ln");
    row.dataset.line = String(index);
    // The + that hovering a line number shows, as in GitHub's and GitLab's diff views. It is
    // out of the Tab order: the keyboard way is a selection and `c`.
    const add = el("button", "add-note", "+");
    add.type = "button";
    add.tabIndex = -1;
    add.setAttribute("aria-label", `Add a note on line ${index + 1}`);
    row.append(add, el("span", "n", String(index + 1)));

    const code = el("span", "c");
    for (const [className, text] of state.file.lines[index]) {
      code.append(className ? el("span", className, text) : text);
    }
    row.append(code);

    decorateRow(row, index);
    return row;
  }

  // Applies selection and note styling to a row. Called for new rows and again when the
  // selection or the notes change, so those changes do not rebuild the rows.
  function decorateRow(row, index) {
    row.classList.toggle("selected", isSelected(index));

    const note = state.noteByLine.get(index);
    row.classList.toggle("noted", Boolean(note));
    row.querySelector(".remark")?.remove();
    if (note && note.lines.end - 1 === index) {
      row.append(el("span", "remark", `${note.id} · ${note.body}`));
    }
  }

  function redecorateRows() {
    for (const row of rowsEl.querySelectorAll(".ln")) {
      decorateRow(row, Number(row.dataset.line));
    }
  }

  // A new code font size: the line at the top stays at the top. The scroll offset is in
  // pixels, and the same offset at another line height would show another part of the file.
  function setLineHeight(height) {
    if (height === lineHeight) return;
    const top = lineAt(codeEl.scrollTop);
    lineHeight = height;
    if (!state.file) return;
    // Rows are placed by line height, so they are placed again, and the padding that stands
    // in for the other lines grows or shrinks with them. Only then can the scroll offset go
    // where the top line now is: before, the browser would cut it to the old height of the
    // file.
    renderCode({ force: true });
    codeEl.scrollTop = lineTop(top);
    renderCode({ force: true });
  }

  // ---- Selection --------------------------------------------------------------------------

  function selectedRange() {
    const selection = state.selection;
    if (!selection) return null;
    return {
      start: Math.min(selection.anchor, selection.head),
      end: Math.max(selection.anchor, selection.head),
    };
  }

  function isSelected(index) {
    const range = state.composer ?? selectedRange();
    return Boolean(range) && range.start <= index && index <= range.end;
  }

  function lineOfEvent(event) {
    const row = event.target.closest?.(".ln");
    return row ? Number(row.dataset.line) : null;
  }

  let dragging = false;

  rowsEl.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || !event.target.classList.contains("n")) return;
    const line = lineOfEvent(event);
    if (line === null || state.composer) return;

    event.preventDefault();
    codeEl.focus();
    // A text selection elsewhere would otherwise stay highlighted, and be what Copy copies.
    window.getSelection().removeAllRanges();
    const anchor = event.shiftKey && state.selection ? state.selection.anchor : line;
    state.selection = { anchor, head: line };
    dragging = true;
    redecorateRows();
  });

  rowsEl.addEventListener("mousemove", (event) => {
    if (!dragging) return;
    const line = lineOfEvent(event);
    if (line === null || line === state.selection.head) return;
    state.selection.head = line;
    redecorateRows();
  });

  // A drag across the code text selects the lines it touches, as a drag in the gutter does, so
  // the next `c` or right-click acts on them. The text stays highlighted for copying. A plain
  // click in the code clears the selection, as in an editor.
  //
  // While the button is down, `textDrag.lines` follows the browser's selection. It is kept
  // rather than read once at the release: a drag that scrolls the pane makes the virtual list
  // rebuild its rows, and the browser drops the selection with the nodes it was in. The
  // release itself may land anywhere, the sidebar included.
  let textDrag = null;

  rowsEl.addEventListener("mousedown", (event) => {
    // A press on the + starts neither a text selection nor a drag: its click adds the note.
    if (event.target.closest(".add-note")) {
      event.preventDefault();
      return;
    }
    if (event.button !== 0 || event.target.classList.contains("n") || state.composer) return;
    textDrag = { lines: null };
  });

  // The + on a line: a note on that line, or on the whole selection when the line is in it.
  rowsEl.addEventListener("click", (event) => {
    if (!event.target.closest(".add-note")) return;
    const line = lineOfEvent(event);
    const range = selectedRange();
    const inSelection = range && range.start <= line && line <= range.end;
    openComposer(inSelection ? range : { start: line, end: line });
  });

  document.addEventListener("selectionchange", () => {
    if (textDrag) textDrag.lines = currentDragLines(textDrag.lines);
  });

  window.addEventListener("mouseup", (event) => {
    dragging = false;
    if (!textDrag || event.button !== 0) return;
    // `selectionchange` is delivered after the event that caused it, so the last change of
    // the drag may not have arrived yet: read the selection once more here.
    const lines = currentDragLines(textDrag.lines);
    textDrag = null;
    if (state.composer) return;
    state.selection = lines ? { anchor: lines.start, head: lines.end } : null;
    redecorateRows();
  });

  // The lines the drag covers now: those of the selection when it is in the code (none for a
  // collapsed one, a plain click), else the last ones seen (the rows it was in were rebuilt).
  function currentDragLines(last) {
    const selection = window.getSelection();
    const inCode = selection.rangeCount > 0 && rowsEl.contains(selection.anchorNode);
    return inCode ? textSelectionLines() : last;
  }

  // The lines the browser's text selection covers in the code, or null when nothing is
  // selected there. As in VS Code, the line a selection starts on counts even when it starts
  // at that line's end, and the line it ends on does not count when it ends at its start.
  function textSelectionLines() {
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || selection.rangeCount === 0) return null;
    const range = selection.getRangeAt(0);
    const lineOf = (node) => {
      const element = node.nodeType === Node.TEXT_NODE ? node.parentElement : node;
      const row = element?.closest(".ln");
      return row && rowsEl.contains(row) ? row : null;
    };
    const first = lineOf(range.startContainer);
    const last = lineOf(range.endContainer);
    if (!first || !last) return null;

    const start = Number(first.dataset.line);
    let end = Number(last.dataset.line);
    // A selection that runs to the very start of a line (a triple click, or a drag down to
    // the left edge) takes none of that line's text, so the line is not part of it.
    if (end > start) {
      const before = document.createRange();
      before.setStart(last.querySelector(".c") ?? last, 0);
      before.setEnd(range.endContainer, range.endOffset);
      if (before.toString() === "") end -= 1;
    }
    return { start, end };
  }

  function clearSelection() {
    state.selection = null;
    redecorateRows();
  }

  // ---- Copy -------------------------------------------------------------------------------

  // The text of lines `start` to `end` (0-based, inclusive) as the page shows them.
  function linesText({ start, end }) {
    const lines = state.file.lines.slice(start, end + 1);
    return lines.map((tokens) => tokens.map(([, text]) => text).join("")).join("\n");
  }

  // The code the text selection covers, without the line numbers and note remarks that sit
  // in the same rows: WebKit's own selection text includes them even though they cannot be
  // selected (seen 2026-11-22: a two-line selection copied the gutter's "3"). Null when the
  // selection is not in the code, or covers other lines than the selected ones (a leftover
  // from before the gutter or a right-click chose new lines).
  function selectedCodeText() {
    const lines = textSelectionLines();
    const chosen = selectedRange();
    if (!lines || !chosen || lines.start !== chosen.start || lines.end !== chosen.end) {
      return null;
    }
    const range = window.getSelection().getRangeAt(0);
    const code = (line) => rowsEl.querySelector(`.ln[data-line="${line}"] .c`);
    // How many characters of `cell` come before (container, offset), or null when that
    // position is not inside the cell (it is in the gutter, or on another line).
    const offsetIn = (cell, container, offset) => {
      if (!cell?.contains(container)) return null;
      const before = document.createRange();
      before.setStart(cell, 0);
      before.setEnd(container, offset);
      return before.toString().length;
    };

    const text = linesText(lines);
    const from = offsetIn(code(lines.start), range.startContainer, range.startOffset) ?? 0;
    const lastLine = linesText({ start: lines.end, end: lines.end });
    const to = offsetIn(code(lines.end), range.endContainer, range.endOffset) ?? lastLine.length;
    return text.slice(from, text.length - (lastLine.length - to));
  }

  rowsEl.addEventListener("copy", (event) => {
    const text = selectedCodeText();
    if (text === null) return;
    event.preventDefault();
    event.clipboardData.setData("text/plain", text);
  });

  // Cmd-C with lines selected. With text selected, the browser copies and the `copy` handler
  // fixes the text: returns false. Lines chosen in the gutter have no text selection, so the
  // whole lines are copied here: returns true.
  function copyLines() {
    if (selectedCodeText() !== null) return false;
    copyText(linesText(selectedRange()));
    return true;
  }

  // The Clipboard API needs a secure context, which a custom URL scheme may not count as; the
  // older copy command works from any page with a selection, so it is the fallback.
  async function copyText(text) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // Fall through to the copy command below.
    }
    const area = el("textarea");
    area.value = text;
    area.className = "copy-buffer";
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
    codeEl.focus();
  }

  // ---- Context menu -----------------------------------------------------------------------
  //
  // Right-click on the code opens actions for the selected lines. A click outside the
  // selection first selects the clicked line, as editors move the cursor there.

  rowsEl.addEventListener("contextmenu", (event) => {
    // The composer's textarea keeps the system menu: spelling, paste, and the like.
    if (event.target.closest(".composer")) return;
    const line = lineOfEvent(event);
    if (line === null || !state.file) return;
    event.preventDefault();

    const range = selectedRange();
    if (!range || line < range.start || line > range.end) {
      window.getSelection().removeAllRanges();
      state.selection = { anchor: line, head: line };
      redecorateRows();
    }
    menu.open(event.clientX, event.clientY, codeActions(line));
  });

  // The actions for the selected lines; `line` is the one right-clicked, for its note.
  function codeActions(line) {
    const range = selectedRange();
    const first = range.start + 1;
    const last = range.end + 1;
    const reference = `${state.file.path}:${first === last ? first : `${first}-${last}`}`;
    // The text selection when there is one inside the code, else the whole selected lines.
    const text = selectedCodeText() ?? linesText(range);
    // `matches` is the action's own key, which works while the menu is open, as it is shown.
    const actions = [
      {
        label: "Add Note",
        key: "C",
        matches: (event) => event.key === "c" && !event.metaKey,
        run: () => openComposer(range),
        disabled: Boolean(state.composer),
      },
      {
        label: "Copy",
        key: "⌘C",
        matches: (event) => event.key === "c" && event.metaKey,
        run: () => copyText(text),
      },
      { label: "Copy Reference", detail: reference, run: () => copyText(reference) },
    ];
    const note = state.noteByLine.get(line);
    if (note) {
      // A separator sets the one destructive item apart, as macOS menus do.
      actions.push({
        label: `Delete Note ${note.id}`,
        separated: true,
        run: () => deleteNote(note),
      });
    }
    return actions;
  }

  // ---- Composer ---------------------------------------------------------------------------

  // Opens the composer on `range` (0-based lines), by default the selection. The context
  // menu passes the lines it was opened on, so its Add note acts on them whatever happened
  // to the selection between the right-click and the click.
  function openComposer(range = selectedRange()) {
    if (!range || state.composer) return;
    state.selection = { anchor: range.start, head: range.end };

    // The composer opens under the selection, so the selection must be on screen to see it.
    const top = lineTop(range.start);
    const bottom = lineTop(range.end) + lineHeight;
    if (top < codeEl.scrollTop || bottom > codeEl.scrollTop + codeEl.clientHeight) {
      codeEl.scrollTop = top - codeEl.clientHeight / 3;
    }

    const composer = { ...range, text: "", saving: false, node: null };
    composer.node = buildComposer(composer);
    state.composer = composer;
    renderCode({ force: true });
    composer.node.querySelector("textarea").focus();
  }

  // Closes the composer, saving its text as a note first when `save` is set. When saving
  // fails, the composer stays open with the reason in its label, so the text is not lost.
  async function closeComposer({ save }) {
    const composer = state.composer;
    if (!composer || composer.saving) return;
    if (save && composer.text.trim()) {
      composer.saving = true;
      let failure;
      try {
        failure = await saveNote(composer);
      } catch (error) {
        failure = `cannot save the note: ${error}`;
      } finally {
        composer.saving = false;
      }
      if (failure) {
        const label = composer.node.querySelector(".composer-label");
        label.textContent = failure;
        label.classList.add("failed");
        return;
      }
    }
    state.composer = null;
    state.selection = null;
    renderCode({ force: true });
    onComposerClosed();
    codeEl.focus();
  }

  // Sends the composer's note to the Rust process with the text the page showed on its lines,
  // so a file changed on disk meanwhile cannot attach the note to other text. Returns the
  // reason on failure, or null.
  async function saveNote(composer) {
    const request = {
      path: state.file.path,
      lines: { start: composer.start + 1, end: composer.end + 1 },
      snippet: linesText(composer),
      body: composer.text,
    };
    const response = await fetch("api/notes", { method: "POST", body: JSON.stringify(request) });
    if (!response.ok) return await response.text();
    state.notes = (await response.json()).notes;
    indexNotes();
    return null;
  }

  // Builds the composer's element once, when it opens. `renderCode` moves this same node.
  function buildComposer(composer) {
    const node = $("composer-template").content.firstElementChild.cloneNode(true);

    const label =
      composer.start === composer.end
        ? `Note on line ${composer.start + 1}`
        : `Note on lines ${composer.start + 1} to ${composer.end + 1}`;
    node.querySelector("label").textContent = label;

    const textarea = node.querySelector("textarea");
    textarea.addEventListener("input", () => {
      composer.text = textarea.value;
      // Grow with the text, then tell the virtual list the new height.
      textarea.style.height = "auto";
      textarea.style.height = `${textarea.scrollHeight}px`;
      measureComposer(node);
    });
    textarea.addEventListener("keydown", (event) => {
      // Cmd-Shift-Enter is Send, which the window's handler answers.
      if (event.key === "Enter" && event.metaKey && !event.shiftKey) {
        closeComposer({ save: true });
      }
    });

    node.querySelector("[data-action=save]").addEventListener("click", () => {
      closeComposer({ save: true });
    });
    node.querySelector("[data-action=discard]").addEventListener("click", () => {
      closeComposer({ save: false });
    });

    // Measured after it is in the document; until then the guess stands in.
    requestAnimationFrame(() => measureComposer(node));
    return node;
  }

  function measureComposer(node) {
    if (!node.isConnected) return;
    const style = getComputedStyle(node);
    const height =
      node.offsetHeight + parseFloat(style.marginTop) + parseFloat(style.marginBottom);
    if (height !== composerHeight) {
      composerHeight = height;
      setRowsPadding();
    }
  }

  // ---- Scrolling --------------------------------------------------------------------------

  let renderQueued = false;
  codeEl.addEventListener(
    "scroll",
    () => {
      menu.close();
      if (renderQueued) return;
      renderQueued = true;
      requestAnimationFrame(() => {
        renderQueued = false;
        renderCode();
      });
    },
    { passive: true },
  );

  new ResizeObserver(() => renderCode()).observe(codeEl);

  return {
    fetchFile,
    openFile,
    showFile,
    restoreHeader,
    showNote,
    lineCount,
    render: renderCode,
    redecorate: redecorateRows,
    setLineHeight,
    clearSelection,
    copyLines,
    openComposer,
    closeComposer,
  };
}
