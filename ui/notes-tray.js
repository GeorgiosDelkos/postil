// The tray's notes: a card per note, grouped by where it is in the loop (status.js), and the
// send step.
//
// Sending (docs/design.md, "The loop", step 5): Cmd-Shift-Enter or the Send button opens the
// send step. The tray's notes get checkboxes, the open ones ticked (run.js, `sendable`),
// and the code pane shows the prompt exactly as the agent will receive it. Cmd-Shift-Enter
// again, or Send, sends; Escape closes the step. The run then shows in the tray's Run tab
// (run-panel.js), which takes the front whenever the agent needs the engineer.

import { $, actOnSecondClick, agentLogoIcon, el, showToast, statusIcon } from "./dom.js";
import { fullPlace, shortPlace } from "./note-place.js";
import { isAsk, isResolved, promptLines, sendable, waitsToSend } from "./run.js";
import { formatSlot } from "./settings.js";
import { groupNotes, noteStatus, planProgress, runOf } from "./status.js";

// `state` is the page's state: the tray reads `notes`, `settings` and `projectOpen`, and keeps
// `send` and `tab`. `runs` is the run panel (run-panel.js) and `menu` the context menu
// (context-menu.js). The callbacks:
// - `openNote(id)` opens a note's dialog, and `drawNoteDetail()` draws it again if open;
// - `deleteNote(note)` deletes a note;
// - `openSettings()` opens the settings;
// - `beforeSend()` clears the way for the send step, and returns false when it cannot open;
// - `restoreHeader()` gives the code pane's header back to the open file.
export function notesTray({
  state,
  notesEl,
  runs,
  menu,
  openNote,
  drawNoteDetail,
  deleteNote,
  openSettings,
  beforeSend,
  restoreHeader,
}) {
  const previewEl = $("prompt-preview");
  const sendBarEl = $("send-bar");

  // The tray: in the send step, every note with its checkbox, in the prompt's order; otherwise
  // the notes grouped by where they are in the loop (status.js), each card with its status.
  // Drawn again only when what it shows changed: a run updates many times a second, and a
  // redraw would drop a card's hover and a delete waiting for its second click.
  let drawnNotes = null;
  function renderNotes() {
    const count = state.notes.length;
    $("note-count").textContent = state.send
      ? `${state.send.ticked.size} of ${count} selected`
      : count === 1
        ? "1 note"
        : `${count} notes`;
    renderSendBar();
    drawNoteDetail();
    if (count === 0) {
      drawnNotes = null;
      const empty = el("div", "empty", "Select lines, then press ");
      empty.append(
        el("kbd", "", "c"),
        " to write a note, or right-click a file or folder for a note on all of it.",
      );
      notesEl.replaceChildren(empty);
      return;
    }
    if (state.send) {
      drawnNotes = null;
      // A resolved note is sent only once reopened (src/notes.rs, `resolve`), and an ask never.
      const offered = state.notes.filter((note) => !isResolved(note) && !isAsk(note));
      notesEl.replaceChildren(...offered.map(sendItem));
      return;
    }

    const live = runs.liveRuns();
    const cards = new Map(state.notes.map((note) => [note.id, cardOf(note)]));
    const key = JSON.stringify([
      ...cards.values(),
      ...live.map((run) => [run.id, run.status, planProgress(run)]),
    ]);
    if (key === drawnNotes) return;
    drawnNotes = key;

    const groups = groupNotes(state.notes, (note) => cards.get(note.id).status);
    const parts = [];
    if (groups.inRun.length && live.length) {
      // One run says how far it is; several, side by side, name themselves.
      const progress = live.length === 1 ? planProgress(live[0]) : null;
      const title =
        live.length === 1
          ? `In ${live[0].id} · ${live[0].agent}`
          : `In ${live.map((run) => run.id).join(", ")}`;
      parts.push(
        groupHeader(title, progress ? `${progress.done} of ${progress.total} steps` : ""),
        ...groups.inRun.map((note) => noteItem(note, cards.get(note.id))),
      );
    }
    if (groups.open.length) {
      parts.push(
        groupHeader(`Open · ${groups.open.length}`, ""),
        ...groups.open.map((note) => noteItem(note, cards.get(note.id))),
      );
    }
    if (groups.done.length) {
      parts.push(
        groupHeader(`Done · ${groups.done.length}`, "Newest first"),
        ...groups.done.map((note) => noteItem(note, cards.get(note.id))),
      );
    }
    if (groups.resolved.length) {
      parts.push(
        groupHeader(`Resolved · ${groups.resolved.length}`, ""),
        ...groups.resolved.map((note) => noteItem(note, cards.get(note.id))),
      );
    }
    notesEl.replaceChildren(...parts);
  }

  // The run a note's status comes from, or null (status.js, `runOf`).
  function runFor(note) {
    return runOf(note, { get: (id) => runs.byId(id) }, runs.liveRuns());
  }

  // Everything a note's card shows, as plain data: two cards with equal data look the same. A
  // card is small, its status and the note in two lines at most, so the tray scans fast; what
  // the agent said and did is in the note's detail. The owner set this for every view on
  // 2026-11-23: compact cards, full information in the dialogs.
  function cardOf(note) {
    const run = runFor(note);
    const status = noteStatus(note, run);
    return {
      id: note.id,
      place: shortPlace(note),
      fullPlace: fullPlace(note),
      body: note.body,
      outdated: note.anchor === "lost",
      status,
      sendable: canSendAlone() && !isResolved(note) && !isAsk(note) && !inLiveRun(note),
    };
  }

  // Whether the arrow on a card sends its note now: fewer runs under way than a repository may
  // have (run-store.js, `MAX_LIVE`). A note whose lines moved can be sent too: the prompt
  // quotes the lines as they were (src/notes.rs, `prompt`), and an agent's edit is the usual
  // reason they moved.
  function canSendAlone() {
    return !runs.full();
  }

  function groupHeader(left, right) {
    const header = el("div", "note-group");
    header.append(el("span", "", left), el("span", "", right));
    return header;
  }

  // A note's card: its status, its place and id, and the note in two lines at most.
  function noteCard(card, onClick) {
    const button = el("button", `note ${card.status.kind}${card.outdated ? " outdated" : ""}`);
    button.type = "button";
    const head = el("div", "note-where");
    const label = el("span", `note-status tone-${card.status.tone}`);
    const said = card.outdated ? `${card.status.label} · outdated` : card.status.label;
    label.append(statusIcon(card.status.kind), said);
    const place = el("span", "note-place", card.place);
    place.title = card.fullPlace;
    head.append(label, place, el("span", "note-id", card.id));
    button.append(head, el("div", "note-body", card.body));
    button.addEventListener("click", onClick);
    return button;
  }

  function noteItem(note, card) {
    const item = el("div", "note-item");
    item.append(noteCard(card, () => openNote(note.id)));
    if (card.sendable) item.append(sendButton(note));
    item.append(deleteButton(item, note));
    return item;
  }

  // The arrow on a card: sends the note alone to the default agent, or with Shift opens the
  // send step with only it ticked (decided with the owner on 2026-11-23).
  function sendButton(note) {
    const button = el("button", "note-send", "↗");
    button.type = "button";
    button.setAttribute("aria-label", `Send note ${note.id} alone`);
    const agent = state.settings?.agent ? formatSlot(state.settings.agent) : "the agent";
    button.title = `Send ${note.id} to ${agent} now · Shift-click to see the prompt first`;
    button.addEventListener("click", (event) => sendAlone(note, { review: event.shiftKey }));
    return button;
  }

  // A card of the send step: the same card, with its checkbox, and a click that ticks it.
  function sendItem(note) {
    const item = el("div", "note-item");
    const card = noteCard(cardOf(note), () => toggleTick(note.id));
    item.append(tickBox(note), card, deleteButton(item, note));
    return item;
  }

  // The checkbox that puts a note in the send, or takes it out.
  function tickBox(note) {
    const box = el("input", "note-tick");
    box.type = "checkbox";
    box.checked = state.send.ticked.has(note.id);
    box.setAttribute("aria-label", `Send note ${note.id}`);
    box.addEventListener("change", () => toggleTick(note.id));
    return box;
  }

  function toggleTick(id) {
    if (state.send.ticked.has(id)) state.send.ticked.delete(id);
    else state.send.ticked.add(id);
    renderNotes();
    showPrompt();
  }

  // The ×  on a note card. A note is the engineer's writing, so deleting takes two clicks: the
  // first turns the button into "Delete", the second deletes. Leaving the card undoes the first.
  function deleteButton(item, note) {
    const button = el("button", "note-delete", "×");
    button.type = "button";
    button.setAttribute("aria-label", `Delete note ${note.id}`);
    actOnSecondClick(button, {
      area: item,
      arm: () => {
        item.classList.add("confirming");
        button.textContent = "Delete";
      },
      disarm: () => {
        item.classList.remove("confirming");
        button.textContent = "×";
      },
      act: () => deleteNote(note),
    });
    return button;
  }

  // ---- The send step ----------------------------------------------------------------------

  // The agent of this send, or the settings' agent.
  function chosenAgent() {
    return state.send?.agent ?? state.settings?.agent?.name ?? null;
  }

  // The notes ticked for the send, in the tray's order, which is the prompt's order.
  function tickedIds() {
    return state.notes.filter((note) => state.send.ticked.has(note.id)).map((note) => note.id);
  }

  function renderSendBar() {
    renderFocusPill();
    const open = state.notes.filter(waitsToSend).length;
    const shown =
      state.projectOpen &&
      state.tab === "notes" &&
      !runs.full() &&
      (state.send !== null || open > 0);
    sendBarEl.hidden = !shown;
    if (!shown) return;

    // In the send step the bar is one split button: the wide part sends and names the agent,
    // the narrow part chooses it (chosen with the owner on 2026-11-23 over a separate picker).
    const picker = $("agent-picker");
    picker.hidden = !state.send;
    sendBarEl.classList.toggle("split", Boolean(state.send));
    const button = $("send-button");
    const count = state.send ? tickedIds().length : open;
    const noun = count === 1 ? "1 note" : `${count} notes`;
    const agent = chosenAgent();
    const words = !state.send
      ? `Send ${noun}…`
      : agent
        ? `Send ${noun} to ${agent}`
        : "Choose an agent";
    button.replaceChildren(el("span", "send-words", words), el("kbd", "", "⌘⇧↵"));
    button.disabled = Boolean(state.send) && agent !== null && (count === 0 || state.send.sending);
  }

  // Focus mode's pill (app.js, `toggleFocus`): the open notes' count and Send.
  function renderFocusPill() {
    const pill = $("focus-pill");
    const open = state.notes.filter(waitsToSend).length;
    pill.replaceChildren(open === 1 ? "1 open note" : `${open} open notes`);
    if (open > 0 && !runs.full()) pill.append(" · Send", el("kbd", "", "⌘⇧↵"));
    pill.disabled = open === 0 || runs.full();
  }

  // Whether a run of this window under way carries `note`. A note is marked sent only once
  // its agent takes the turn, so until then it reads as open; sending it again would put it in
  // two threads at once, which the Rust process refuses.
  function inLiveRun(note) {
    return runs.liveRuns().some((run) => !run.review && run.notes.includes(note.id));
  }

  // The ids of the notes a send takes now (`sendable`), those in a run under way left out.
  function sendableNow() {
    return sendable(state.notes.filter((note) => !inLiveRun(note)));
  }

  // Opens the send step with `ids` ticked: the open notes not in a run under way, or the one
  // note of a Shift-click.
  function openSend(ids = sendableNow()) {
    if (!beforeSend()) return;
    state.send = { ticked: new Set(ids), agent: null, sending: false };
    showTab("notes");
    renderNotes();
    showPrompt();
  }

  function closeSend() {
    if (!state.send) return;
    state.send = null;
    previewEl.hidden = true;
    restoreHeader();
    renderNotes();
  }

  // Shows the prompt of the ticked notes in the code pane, as the Rust process builds it.
  let promptRequest = 0;
  async function showPrompt() {
    if (!state.send) return;
    const ids = tickedIds();
    const request = ++promptRequest;
    $("crumbs").replaceChildren(el("b", "", "What the agent receives"));
    previewEl.hidden = false;
    if (ids.length === 0) {
      $("file-meta").textContent = "";
      previewEl.replaceChildren(el("div", "empty", "Tick the notes to send."));
      return;
    }
    const response = await fetch(`api/run/preview?notes=${ids.join(",")}`);
    const reply = response.ok ? await response.json() : await response.text();
    // A later tick, or the step closed, while this was in flight.
    if (request !== promptRequest || !state.send) return;
    if (!response.ok) {
      previewEl.replaceChildren(el("div", "empty", reply));
      return;
    }

    // The files linked after the text, as the Rust process links them: a folder note's folder
    // is named in the text only.
    const files = reply.files;
    const notes = ids.length === 1 ? "1 note" : `${ids.length} notes`;
    const fileCount = files.length === 1 ? "1 file" : `${files.length} files`;
    $("file-meta").textContent = `${notes} · ${fileCount}`;
    const text = el("pre", "prompt-text");
    for (const line of promptLines(reply.prompt)) {
      text.append(el("span", `prompt-${line.kind}`, `${line.text}\n`));
    }
    const links = el("div", "prompt-links");
    links.append(el("span", "prompt-links-label", "Linked files"));
    for (const path of files) links.append(el("span", "prompt-link", path));
    const warning = uncommittedWarning(reply);
    previewEl.replaceChildren(
      ...(warning ? [warning] : []),
      text,
      ...(files.length ? [links] : []),
    );
  }

  // What the send step says when the working tree has changes since the last commit: the
  // thread starts from that commit and does not see them, and a noted file among them may not
  // have the note's lines in the agent's copy (docs/design.md, "Threads"). Null when clean.
  function uncommittedWarning(reply) {
    if (reply.uncommitted === 0) return null;
    const warning = el("div", "send-warning");
    // `git status` failed: the tree is not known to be clean, so the step does not say it is.
    if (reply.uncommitted === null) {
      const unknown = `Could not check your working tree for uncommitted changes (${reply.uncommittedError}). The thread starts from the last commit.`;
      warning.append(el("div", "", unknown));
      return warning;
    }
    const changes = reply.uncommitted === 1 ? "1 change" : `${reply.uncommitted} changes`;
    warning.append(
      el(
        "div",
        "",
        `Your working tree has ${changes} since the last commit. The thread starts from that commit and does not see them.`,
      ),
    );
    if (reply.uncommittedNoted.length) {
      const noted = reply.uncommittedNoted.join(", ");
      warning.append(
        el(
          "div",
          "",
          `Changed among the noted files: ${noted}. The agent's copy may not have their notes' lines, or the file at all.`,
        ),
      );
    }
    return warning;
  }

  async function sendNotes() {
    const agent = chosenAgent();
    if (!state.send || state.send.sending || tickedIds().length === 0 || !agent) return;
    // This send step: the engineer may close it, and open another, while the request runs.
    // What follows touches only this one, never a later step with a request of its own.
    const send = state.send;
    send.sending = true;
    renderSendBar();
    // Send is usable again whatever happened, a request that never got an answer included:
    // its error reaches the page's toast (app.js, `unhandledrejection`).
    let started = false;
    try {
      started = await startRun(tickedIds(), agent);
    } finally {
      send.sending = false;
      if (state.send === send) renderSendBar();
    }
    // A newer step is the engineer's next send: left as it is.
    if (!started || (state.send && state.send !== send)) return;
    closeSend();
    showTab("run");
  }

  // Sends `note` alone to the default agent, and stays on the notes: its card shows the run.
  // With `review`, or with no agent chosen yet, opens the send step with only it ticked.
  async function sendAlone(note, { review }) {
    const agent = state.settings?.agent?.name;
    if (review || !agent) {
      openSend([note.id]);
      return;
    }
    if (await startRun([note.id], agent)) showTab("notes");
  }

  // Starts a run of `ids` with `agent`. Returns whether it started; a refusal is shown.
  async function startRun(ids, agent) {
    const body = JSON.stringify({ notes: ids, agent });
    const response = await fetch("api/run", { method: "POST", body });
    if (!response.ok) {
      showToast(await response.text());
      return false;
    }
    await runs.sync();
    return true;
  }

  $("send-button").addEventListener("click", () => {
    if (!state.send) openSend();
    else if (chosenAgent()) sendNotes();
    else $("agent-picker").click();
  });

  // The agents turned on in the settings, to send this one run to; the settings' agent is the
  // default.
  $("agent-picker").addEventListener("click", (event) => {
    const { left, top } = event.currentTarget.getBoundingClientRect();
    const agents = state.settings?.enabledAgents ?? [];
    const actions = agents.map((name) => ({
      label: name,
      icon: agentLogoIcon(name),
      detail: name === state.settings.agent?.name ? "default" : "",
      key: name === chosenAgent() ? "✓" : "",
      run: () => {
        if (!state.send) return;
        state.send.agent = name;
        renderSendBar();
      },
    }));
    actions.push({
      label: agents.length === 0 ? "Turn on an agent in Settings…" : "Agents in Settings…",
      separated: agents.length > 0,
      run: () => openSettings(),
    });
    menu.open(left, top, actions);
  });

  $("focus-pill").addEventListener("click", () => openSend());

  // ---- The tabs ---------------------------------------------------------------------------

  function showTab(tab) {
    state.tab = tab;
    const run = tab === "run";
    if (run) closeSend();
    $("tab-notes").setAttribute("aria-selected", String(!run));
    $("tab-run").setAttribute("aria-selected", String(run));
    notesEl.hidden = run;
    $("run").hidden = !run;
    $("note-count").hidden = run;
    $("run-header").hidden = !run;
    renderSendBar();
  }

  $("tab-notes").addEventListener("click", () => showTab("notes"));
  $("tab-run").addEventListener("click", () => showTab("run"));
  $("agent-status").addEventListener("click", () => showTab("run"));

  return {
    render: renderNotes,
    renderSendBar,
    runFor,
    cardOf,
    noteCard,
    canSendAlone,
    inLiveRun,
    openSend,
    closeSend,
    sendNotes,
    sendAlone,
    showTab,
  };
}
