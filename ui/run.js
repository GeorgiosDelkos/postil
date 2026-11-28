// The rules of the send step and the run panel, apart from the document so `node --test`
// runs them. A run comes from `GET api/run` (src/run/log.rs, `RunView`): its entries are the
// lines of its log, append-only, and the panel folds them into what it shows.

// The notes ticked when the send step opens: the open change notes whose lines are still in
// their file. A sent note can be sent again, and an outdated one is offered, but both are left
// for the engineer to tick.
export function sendable(notes) {
  return notes
    .filter((note) => waitsToSend(note) && note.anchor === "found")
    .map((note) => note.id);
}

// Whether a note is a question, answered by an agent that only reads, in a thread of its own
// (src/notes.rs, "Asks"). An ask is never sent with change notes: the server refuses it.
export function isAsk(note) {
  return note.intent === "ask";
}

// Whether a note is a change still to be sent: what the send step's counts count.
export function waitsToSend(note) {
  return isOpen(note) && !isAsk(note);
}

// Whether a note is still to be sent. `state` is `{ kind: "open" }`,
// `{ kind: "sent", run: "R1" }` or `{ kind: "resolved", run: "R1" }`, a resolved note's run
// left out when it was never sent (src/notes.rs).
export function isOpen(note) {
  return note.state.kind === "open";
}

// Whether the engineer marked the note done.
export function isResolved(note) {
  return note.state.kind === "resolved";
}

// The run a note was sent in, or null. A resolved note keeps the run it was sent in last.
export function sentIn(note) {
  return note.state.run ?? null;
}

// Folds a run's entries into what the panel shows: the latest plan, and the items in the
// order they happened. Consecutive message parts form one message, and so do thought parts;
// a tool call's changes are applied to the call; an answer is set on its request.
// Folds are kept per entries list: the page folds a run on every draw, for every note sent
// in it, and a run's entries list is replaced, not changed, when it grows. The result must not
// be changed by its callers.
const folds = new WeakMap();

// Maps an extended entries array back to the prefix array it was built from via `appendEntries`,
// so `startsWith` can verify incremental extensions in O(1) instead of scanning all elements.
const entryParents = new WeakMap();

export function appendEntries(base, added) {
  if (added.length === 0) return base;
  const next = base.concat(added);
  // Truncate the parent chain at depth 8 (matching `startsWith`'s hop bound) so older
  // intermediate arrays are not pinned in memory for the lifetime of the run.
  let node = base;
  for (let hops = 0; node && hops < 7; hops += 1) node = entryParents.get(node);
  if (node) entryParents.delete(node);
  entryParents.set(next, base);
  return next;
}

// Recent fold states kept for incremental extension: up to 3 live runs can stream side by
// side, so a single slot would evict one run on every chunk of another and fall back to O(N^2)
// full re-folds.
const MAX_RECENT_FOLDS = 8;
const recentFolds = [];

export function foldRun(entries) {
  let fold = folds.get(entries);
  if (!fold) {
    let base = null;
    for (let i = recentFolds.length - 1; i >= 0; i -= 1) {
      if (startsWith(entries, recentFolds[i].entries)) {
        base = recentFolds[i];
        recentFolds.splice(i, 1);
        break;
      }
    }
    const next = foldMore(base ?? emptyFold(), entries);
    recentFolds.push(next);
    if (recentFolds.length > MAX_RECENT_FOLDS) recentFolds.shift();
    fold = { plan: next.plan, items: next.items, completedEdits: next.completedEdits };
    folds.set(entries, fold);
  }
  return fold;
}

// Whether `entries` begins with every entry of `prefix`, the same objects in the same order.
function startsWith(entries, prefix) {
  const len = prefix.length;
  if (len === 0 || len > entries.length) return false;
  // O(1) fast path when `entries` was extended from `prefix` via `appendEntries`.
  let ancestor = entryParents.get(entries);
  for (let hops = 0; ancestor && hops < 8; hops += 1) {
    if (ancestor === prefix) return true;
    if (ancestor.length < len) break;
    ancestor = entryParents.get(ancestor);
  }
  // Fast reject on endpoints before scanning the full prefix.
  if (entries[0] !== prefix[0] || entries[len - 1] !== prefix[len - 1]) return false;
  for (let i = 1; i < len - 1; i += 1) {
    if (entries[i] !== prefix[i]) return false;
  }
  return true;
}

// The tool kinds that change files: a check that passed before one of these ended is stale,
// and `completedEdits` counts how many have finished.
// An execute call can change files too (a formatter); Postil cannot see into it, so the
// stale state says "an edit after it" and claims no more.
const EDITS = new Set(["edit", "delete", "move"]);

function isCompletedEdit(tool) {
  return tool.status === "completed" && EDITS.has(tool.tool);
}

function emptyFold() {
  return {
    entries: [],
    plan: null,
    items: [],
    completedEdits: 0,
    // Item indices: of each tool call by id, and of each permission request and question by
    // request number.
    tools: new Map(),
    permissions: new Map(),
    // A cancel can log a request's answer before the request itself (src/run/log.rs).
    early: new Map(),
  };
}

// Folds the entries of `entries` after those of `base` into a copy of `base`'s items. The
// maps of `base` are taken over and changed, so `base` must not be extended again; its items
// are not: one is copied before it changes, as a fold already returned must stay as it was
// (`followRun` in app.js compares the fold before an update with the one after).
function foldMore(base, entries) {
  const { tools, permissions, early } = base;
  const shared = base.items.length;
  const items = base.items.slice();
  const copied = new Set();
  const own = (index) => {
    if (index < shared && !copied.has(index)) {
      items[index] = { ...items[index] };
      copied.add(index);
    }
    return items[index];
  };
  let plan = base.plan;
  let completedEdits = base.completedEdits;

  // A part joins the item before it when both are of one type and one message: the agent's
  // message id when it sends one, else parts in a row are one message.
  const append = (type, text, message) => {
    const last = items.at(-1);
    if (last?.type === type && last.message === message) own(items.length - 1).text += text;
    else items.push({ type, text, message });
  };

  // A tool call's final status ends it; `endedAt` is the index of the entry that said so. The
  // items keep the order the calls started in, so this is the only record of the order they
  // ended in: an edit started before a check and finished after it makes the check stale.
  // A call that ended keeps the entry that first said so: one reported again, or completed a
  // second time, after an edit must still read as stale. A status that is not final clears it.
  const ended = (previous, status, at) =>
    status === "completed" || status === "failed" ? (previous ?? at) : null;
  for (let at = base.entries.length; at < entries.length; at += 1) {
    const entry = entries[at];
    switch (entry.kind) {
      case "message":
        append("message", entry.text, entry.message);
        break;
      case "thought":
        append("thought", entry.text);
        break;
      case "tool": {
        // An agent may report the same call twice; the second report replaces the first.
        const known = tools.get(entry.id);
        const tool = {
          type: "tool",
          id: entry.id,
          title: entry.title,
          tool: entry.tool,
          status: entry.status,
          places: entry.places,
          output: entry.output,
          // An execute call's command line and exit code, when the agent reports them; logs
          // before version 5 have neither (src/run/log.rs).
          command: entry.command ?? null,
          exit: entry.exit ?? null,
          endedAt: ended(known === undefined ? null : items[known].endedAt, entry.status, at),
        };
        if (known === undefined) {
          tools.set(entry.id, items.length);
          items.push(tool);
        } else {
          if (isCompletedEdit(items[known])) completedEdits -= 1;
          items[known] = tool;
        }
        if (isCompletedEdit(tool)) completedEdits += 1;
        break;
      }
      case "toolUpdate": {
        if (!tools.has(entry.id)) {
          // A change to a call the log never started: shown, under its id.
          tools.set(entry.id, items.length);
          items.push({
            type: "tool",
            id: entry.id,
            title: entry.id,
            tool: "other",
            status: "pending",
            places: [],
            output: [],
            command: null,
            exit: null,
            endedAt: null,
          });
        }
        const tool = own(tools.get(entry.id));
        const wasEdit = isCompletedEdit(tool);
        for (const field of ["title", "tool", "status", "places", "output", "command", "exit"]) {
          if (entry[field] !== null && entry[field] !== undefined) tool[field] = entry[field];
        }
        if (entry.status !== null && entry.status !== undefined) {
          tool.endedAt = ended(tool.endedAt, entry.status, at);
        }
        const isEdit = isCompletedEdit(tool);
        if (wasEdit !== isEdit) completedEdits += isEdit ? 1 : -1;
        break;
      }
      case "plan":
        plan = entry.steps;
        break;
      case "permission": {
        const call = tools.get(entry.toolCall);
        permissions.set(entry.request, items.length);
        items.push({
          type: "permission",
          request: entry.request,
          title: entry.title ?? (call === undefined ? null : items[call].title) ?? "A tool call",
          options: entry.options,
          // `undefined` while open; then the chosen option's id, or `null` when cancelled.
          answer: early.get(entry.request),
        });
        break;
      }
      case "question":
        // Numbered with the permission requests (src/acp.rs, "Permission requests").
        permissions.set(entry.request, items.length);
        items.push({
          type: "question",
          request: entry.request,
          message: entry.message,
          fields: entry.fields,
          // `undefined` while open; then `{ content }`, the form's values or `null` for a
          // skip, or `null` when closed unanswered (the run stopped, or the agent withdrew it).
          answer: early.get(entry.request),
        });
        break;
      case "answer": {
        const request = permissions.get(entry.request);
        if (request === undefined) early.set(entry.request, entry.option);
        else own(request).answer = entry.option;
        break;
      }
      case "reply": {
        const question = permissions.get(entry.request);
        if (question !== undefined) own(question).answer = { content: entry.content };
        break;
      }
      case "unreadable":
        items.push({
          type: "notice",
          text: `The agent sent something Postil could not read: ${entry.problem}`,
        });
        break;
      case "failed":
        items.push({ type: "error", text: entry.error });
        break;
      // `started`, `cancelRequested` and `ended` show in the status line.
      default:
        break;
    }
  }
  return { entries, plan, items, completedEdits, tools, permissions, early };
}

// The open question, if any: the one the engineer is asked to answer.
export function openQuestion(items) {
  return items.findLast((item) => item.type === "question" && item.answer === undefined) ?? null;
}

// The open permission request, if any: the one the keys answer.
export function openPermission(items) {
  return items.findLast((item) => item.type === "permission" && item.answer === undefined) ?? null;
}

// Whether the agent has not yet reported a tool call finished.
export function underWay(tool) {
  return tool.status === "pending" || tool.status === "in_progress";
}

// The permission key a keydown means: "a", "A" (Shift-a) or "r", or null. The character
// decides, so every keyboard layout answers with the keys the buttons show, but its case does
// not: with Caps Lock on, `a` types "A", which must not allow always; only Shift does. A held
// key answers once.
export function permissionKey(event) {
  if (event.metaKey || event.ctrlKey || event.altKey || event.repeat) return null;
  const key = event.key.toLowerCase();
  if (key === "a") return event.shiftKey ? "A" : "a";
  if (key === "r" && !event.shiftKey) return "r";
  return null;
}

// Which key answers with which option: `a` allows once, `A` (Shift-a) always, `r` rejects.
// "Always" is offered only when the agent offers it (docs/design.md, "ACP integration"); a
// reject falls back to "reject always" when the agent offers no plain reject.
export function permissionKeys(options) {
  const first = (kind) => options.find((option) => option.kind === kind)?.id;
  const keys = new Map();
  const allow = first("allow_once");
  const always = first("allow_always");
  const reject = first("reject_once") ?? first("reject_always");
  if (allow !== undefined) keys.set("a", allow);
  if (always !== undefined) keys.set("A", always);
  if (reject !== undefined) keys.set("r", reject);
  return keys;
}

// The key shown on an option's button, or "" for one no key answers.
export function keyFor(keys, optionId) {
  for (const [key, id] of keys) {
    if (id === optionId) return key === "A" ? "⇧a" : key;
  }
  return "";
}

const VERBS = {
  read: "read",
  edit: "edit",
  delete: "delete",
  move: "move",
  search: "search",
  execute: "run",
  think: "think",
  fetch: "fetch",
  switch_mode: "mode",
  other: "tool",
};

// A tool call as one row: a verb, what it acts on, and its state on the right. A file tool
// names its first file; a command or a search names its title, which the agent writes. A
// call the agent never finished is running only while the run is `live`.
export function toolRow(tool, live) {
  const verb = VERBS[tool.tool] ?? "tool";
  const onFile = ["read", "edit", "delete", "move"].includes(tool.tool);
  const place = tool.places[0];
  const target = onFile && place ? place.path : tool.title;
  let state;
  // Grok ends a command that exits non-zero as completed, with the code alone saying it
  // failed (src/run/log.rs, `exit_of`).
  if (tool.status === "failed" || (tool.exit ?? 0) !== 0) state = "failed";
  else if (tool.status === "completed") {
    const stats = diffStats(tool.output);
    state = stats ? `+${stats.added} -${stats.removed}` : "done";
  } else state = live ? "" : "unfinished";
  const running = live && underWay(tool);
  return { verb, target, state, running };
}

// What a turn's checks show (docs/review-assist.md, "Evidence on the run"): each command the
// author's report (src/report.rs) says it ran, against the execute calls the log holds, then
// every other command the turn ran. Pure: `report` is the turn's report or null, `items` its
// folded items (`foldRun`).
//
// A row is `{command, claim, state, item}`: `claim` is what the author says came of it,
// "passed" or "failed", or null for a command it did not name; `item` is the execute call that
// is the evidence, or null. `state` is the log's, never the author's:
// - `failed`: the call failed, or exited non-zero whatever its status (Grok ends such a
//   command as completed). A failure an edit followed is still failed (owner, 2026-11-25).
// - `unfinished`: the call had not ended when the turn did.
// - `stale`: it passed, and an edit, delete or move ended after it.
// - `passed`: it passed, and no such edit ended after it.
// - `not-seen`: no call ran the command the author names.
// A claim is matched by the last execute call whose command equals it once trimmed and with
// its spaces collapsed, and without a command kept (a log before version 5), by its title
// alone or in backticks. Nothing looser: a claim shown as seen must have been run as said.
export function evidence(report, items) {
  const calls = items.filter((item) => item.type === "tool");
  const runs = calls.filter((call) => call.tool === "execute");
  const lastEdit = Math.max(
    -1,
    ...calls
      .filter((call) => EDITS.has(call.tool) && call.status === "completed")
      .map((edit) => edit.endedAt ?? -1),
  );
  const stateOf = (run) => {
    if (run.status === "failed" || (run.exit ?? 0) !== 0) return "failed";
    if (run.status !== "completed" || run.endedAt === null) return "unfinished";
    return lastEdit > run.endedAt ? "stale" : "passed";
  };
  // The forms of each run's command a claim may match, made once: `evidence` runs on every
  // redraw of the dialog and the card.
  const forms = runs.map((run) =>
    run.command === null || run.command === undefined
      ? [spaced(run.title), spaced(run.title).replace(/^`(.*)`$/, "$1")]
      : [spaced(run.command)],
  );
  const claimed = new Set();
  const rows = (report?.checks ?? []).map((check) => {
    const wanted = spaced(check.command);
    const same = runs.filter((_, i) => forms[i].includes(wanted));
    for (const run of same) claimed.add(run);
    const item = same.at(-1) ?? null;
    const state = item ? stateOf(item) : "not-seen";
    return { command: check.command, claim: check.result, state, item };
  });
  for (const run of runs) {
    if (claimed.has(run)) continue;
    rows.push({ command: run.command ?? run.title, claim: null, state: stateOf(run), item: run });
  }
  return rows;
}

const spaced = (text) => text.trim().replace(/\s+/g, " ");

// The evidence states from worst to best, as the run's card ranks them (owner, 2026-11-25).
const WORST_FIRST = ["failed", "not-seen", "stale", "unfinished", "passed"];

// The worst state among the checks the author claimed, or null when it claimed none: the
// card shows the evidence for what the report says, not for every command the agent tried.
export function worstEvidence(rows) {
  const claimed = rows.filter((row) => row.claim !== null).map((row) => row.state);
  return WORST_FIRST.find((state) => claimed.includes(state)) ?? null;
}

// Lines added and removed by a tool call's diffs, or `null` when it has none.
export function diffStats(output) {
  const diffs = output.filter((part) => part.type === "diff");
  if (diffs.length === 0) return null;
  let added = 0;
  let removed = 0;
  for (const diff of diffs) {
    for (const line of partLines(diff)) {
      if (line.kind === "add") added += 1;
      else if (line.kind === "del") removed += 1;
    }
  }
  return { added, removed };
}

// A diff part's lines, computed once per part: a run's entries keep their objects from fetch
// to fetch, and the panel asks again on every update.
const computed = new WeakMap();
export function partLines(part) {
  let lines = computed.get(part);
  if (!lines) {
    lines = diffLines(part.oldText ?? "", part.newText);
    computed.set(part, lines);
  }
  return lines;
}

// The most edits diffLines looks for before it gives up and counts every line as changed.
// Its time grows with the lines times the edits, so this bounds it to a few million steps
// for files of thousands of lines; an agent's edit changes far fewer lines. Arbitrary.
export const MAX_EDITS = 1000;

// A line diff of two texts: each line `same`, `del` or `add`, in order, with the removed
// lines of a change before the added ones. Myers' O((N+M)D) algorithm ("An O(ND) Difference
// Algorithm and Its Variations", 1986), keeping each round's frontier for the walk back, so
// a small change in a large file is cheap where a table of every line pair was not.
export function diffLines(oldText, newText) {
  const before = splitLines(oldText);
  const after = splitLines(newText);
  const n = before.length;
  const m = after.length;
  const offset = n + m + 1;
  const reach = new Int32Array(2 * offset + 1);
  // trace[d] is the frontier before round d, over diagonals -d-1..d+1.
  const trace = [];
  for (let d = 0; d <= Math.min(n + m, MAX_EDITS); d += 1) {
    trace.push(reach.slice(offset - d - 1, offset + d + 2));
    for (let k = -d; k <= d; k += 2) {
      const down = k === -d || (k !== d && reach[offset + k - 1] < reach[offset + k + 1]);
      let x = down ? reach[offset + k + 1] : reach[offset + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && before[x] === after[y]) {
        x += 1;
        y += 1;
      }
      reach[offset + k] = x;
      if (x >= n && y >= m) return walkBack(trace, before, after);
    }
  }
  return [
    ...before.map((text) => ({ kind: "del", text })),
    ...after.map((text) => ({ kind: "add", text })),
  ];
}

// The edit script, from the frontiers diffLines kept, read from the end back to the start.
function walkBack(trace, before, after) {
  const lines = [];
  let x = before.length;
  let y = after.length;
  for (let d = trace.length - 1; d >= 0; d -= 1) {
    const frontier = trace[d];
    const at = (k) => frontier[k + d + 1];
    const k = x - y;
    const down = k === -d || (k !== d && at(k - 1) < at(k + 1));
    const previousK = down ? k + 1 : k - 1;
    const previousX = at(previousK);
    const previousY = previousX - previousK;
    while (x > previousX && y > previousY) {
      x -= 1;
      y -= 1;
      lines.push({ kind: "same", text: before[x] });
    }
    if (d > 0) {
      if (down) {
        y -= 1;
        lines.push({ kind: "add", text: after[y] });
      } else {
        x -= 1;
        lines.push({ kind: "del", text: before[x] });
      }
    }
  }
  return lines.reverse();
}

function splitLines(text) {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines.at(-1) === "") lines.pop();
  return lines;
}

const REASONS = {
  end_turn: "Done",
  cancelled: "Stopped",
  max_tokens: "Stopped: out of tokens",
  max_turn_requests: "Stopped: too many steps",
  refusal: "The agent refused",
};

// The run's state in words, and its tone for the dot beside it: `busy`, `attention`,
// `done`, `failed`, or `idle`.
export function statusLine(run) {
  switch (run.status) {
    case "starting":
      return { text: `Starting ${run.agent}…`, tone: "busy" };
    case "running":
      return { text: "Working", tone: "busy" };
    case "waiting":
      return { text: "Needs you", tone: "attention" };
    case "cancelling":
      return { text: "Stopping…", tone: "busy" };
    case "ended": {
      const reason = run.entries.findLast((entry) => entry.kind === "ended")?.reason;
      return { text: REASONS[reason] ?? "Ended", tone: reason === "end_turn" ? "done" : "idle" };
    }
    case "failed":
      return { text: "Failed", tone: "failed" };
    default:
      return {
        text: "Unfinished: Postil crashed during it, or another window runs it",
        tone: "idle",
      };
  }
}

// A run's status as a status icon's kind (dom.js, `statusIcon`).
export function iconKind(run) {
  const { tone } = statusLine(run);
  const kinds = { busy: "working", attention: "needs", done: "addressed", failed: "failed" };
  return kinds[tone] ?? "stopped";
}

// A run's status in a word or two: statusLine's text, short enough for a card.
export function statusWord(run) {
  return run.status === "unfinished" ? "Unfinished" : statusLine(run).text;
}

// Whether the run is still going: it can be stopped, and no other can start.
export function isLive(run) {
  return ["starting", "running", "waiting", "cancelling"].includes(run?.status);
}

// Which Stop button the tray header shows: the live run's id, or null when there is none.
// The button is built again only when this changes, not on every update of the run: a press
// and a release on two different nodes is no click, so a Stop rebuilt as the agent writes
// would miss the engineer's click.
export function stopKey(run) {
  return isLive(run) ? run.id : null;
}

// What Escape does in the window, when no dialog is open and the welcome screen is hidden:
// it closes or clears the nearest thing on screen, and stops the run only when nothing else
// is left. Focus mode gives way first, as what it hides cannot be seen. The changes view
// covers the file, so it closes before the file's selection clears. Its own handler takes
// Escape while it has the focus; this is for when the focus is elsewhere.
// `page` says what is open: `{ focusMode, composer, send, changes, selection, live }`, each a
// boolean. Returns the action's name, or null.
export function escapeAction({ focusMode, composer, send, changes, changeMap, selection, live }) {
  if (focusMode && !composer) return "leave-focus";
  if (composer) return "close-composer";
  if (send) return "close-send";
  if (changes) return "close-changes";
  // The change map's own esc: back to the repository's map, before a run would be stopped.
  if (changeMap) return "leave-change-map";
  if (selection) return "clear-selection";
  if (live) return "stop";
  return null;
}

// Seconds as `m:ss`, or `h:mm:ss` from an hour on.
export function elapsed(seconds) {
  const s = Math.max(0, Math.floor(seconds));
  const pad = (n) => String(n).padStart(2, "0");
  const hours = Math.floor(s / 3600);
  const minutes = Math.floor((s % 3600) / 60);
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(s % 60)}` : `${minutes}:${pad(s % 60)}`;
}

// The prompt's lines, each with its kind for coloring: a note's heading (`[N1] path:3`), a
// quoted line of code, the line that says the code moved, or text. The format is
// src/notes.rs, `prompt`.
export function promptLines(text) {
  return text
    .replace(/\n$/, "")
    .split("\n")
    .map((line) => {
      if (/^\[N\d+\] /.test(line)) return { kind: "heading", text: line };
      if (line.startsWith("> ")) return { kind: "quote", text: line };
      // The sentences src/notes.rs `prompt` writes under a note whose target is gone.
      if (line === "(These lines are no longer in the file; they were:)")
        return { kind: "lost", text: line };
      if (/^\(This (file|folder) is no longer in the repository\.\)$/.test(line))
        return { kind: "lost", text: line };
      return { kind: "text", text: line };
    });
}

// The agent's text as blocks of a small Markdown subset: fenced code, headings, lists, and
// paragraphs, with inline code, bold and italic inside them. Agents write Markdown; the
// panel builds elements from these blocks and never parses the text as HTML.
export function markdownBlocks(text) {
  const blocks = [];
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  let paragraph = [];
  const flush = () => {
    if (paragraph.length > 0)
      blocks.push({ type: "paragraph", inline: inline(paragraph.join("\n")) });
    paragraph = [];
  };
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    const fence = line.match(/^\s*```(\S*)/);
    if (fence) {
      flush();
      const code = [];
      for (index += 1; index < lines.length && !/^\s*```/.test(lines[index]); index += 1) {
        code.push(lines[index]);
      }
      blocks.push({ type: "code", text: code.join("\n") });
      continue;
    }
    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    const item = line.match(/^\s*(?:[-*+]|(\d+)[.)])\s+(.*)$/);
    if (heading) {
      flush();
      blocks.push({ type: "heading", inline: inline(heading[2]) });
    } else if (item) {
      flush();
      const ordered = item[1] !== undefined;
      const last = blocks.at(-1);
      const list = last?.type === "list" && last.ordered === ordered ? last : null;
      if (list) list.items.push(inline(item[2]));
      else blocks.push({ type: "list", ordered, items: [inline(item[2])] });
    } else if (line.trim() === "") {
      flush();
    } else {
      paragraph.push(line);
    }
  }
  flush();
  return blocks;
}

// Inline Markdown: `code`, **bold** and *italic* (or _italic_). Code is taken first, so
// stars inside it stay text; an unclosed mark stays text too.
export function inline(text) {
  const parts = [];
  const pattern =
    /(`+)([\s\S]+?)\1|\*\*([\s\S]+?)\*\*|(?<![\w*])[*_]([^*_\s][^*_]*?)[*_](?![\w*])/g;
  let at = 0;
  for (const match of text.matchAll(pattern)) {
    if (match.index > at) parts.push({ type: "text", text: text.slice(at, match.index) });
    if (match[2] !== undefined) parts.push({ type: "code", text: match[2] });
    else if (match[3] !== undefined) parts.push({ type: "strong", text: match[3] });
    else parts.push({ type: "em", text: match[4] });
    at = match.index + match[0].length;
  }
  if (at < text.length) parts.push({ type: "text", text: text.slice(at) });
  return parts;
}
