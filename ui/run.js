// The rules of the send step and the run panel, apart from the document so `node --test`
// runs them. A run comes from `GET api/run` (src/run.rs, `RunView`): its entries are the
// lines of its log, append-only, and the panel folds them into what it shows.

// The notes ticked when the send step opens: the open ones whose lines are still in their
// file. A sent note can be sent again, and an outdated one is offered, but both are left
// for the engineer to tick.
export function sendable(notes) {
  return notes.filter((note) => isOpen(note) && note.anchor === "found").map((note) => note.id);
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

export function foldRun(entries) {
  let fold = folds.get(entries);
  if (!fold) {
    fold = foldEntries(entries);
    folds.set(entries, fold);
  }
  return fold;
}

function foldEntries(entries) {
  let plan = null;
  const items = [];
  const tools = new Map();
  const permissions = new Map();
  // A cancel can log a request's answer before the request itself (src/run.rs).
  const early = new Map();

  // A part joins the item before it when both are of one type and one message: the agent's
  // message id when it sends one, else parts in a row are one message.
  const append = (type, text, message) => {
    const last = items.at(-1);
    if (last?.type === type && last.message === message) last.text += text;
    else items.push({ type, text, message });
  };

  for (const entry of entries) {
    switch (entry.kind) {
      case "message":
        append("message", entry.text, entry.message);
        break;
      case "thought":
        append("thought", entry.text);
        break;
      case "tool": {
        const known = tools.get(entry.id);
        const tool = {
          type: "tool",
          id: entry.id,
          title: entry.title,
          tool: entry.tool,
          status: entry.status,
          places: entry.places,
          output: entry.output,
        };
        // An agent may report the same call twice; the second report replaces the first.
        if (known) Object.assign(known, tool);
        else {
          tools.set(entry.id, tool);
          items.push(tool);
        }
        break;
      }
      case "toolUpdate": {
        let tool = tools.get(entry.id);
        if (!tool) {
          // A change to a call the log never started: shown, under its id.
          tool = { type: "tool", id: entry.id, title: entry.id, tool: "other", status: "pending", places: [], output: [] };
          tools.set(entry.id, tool);
          items.push(tool);
        }
        for (const field of ["title", "tool", "status", "places", "output"]) {
          if (entry[field] !== null && entry[field] !== undefined) tool[field] = entry[field];
        }
        break;
      }
      case "plan":
        plan = entry.steps;
        break;
      case "permission": {
        const request = {
          type: "permission",
          request: entry.request,
          title: entry.title ?? tools.get(entry.toolCall)?.title ?? "A tool call",
          options: entry.options,
          // `undefined` while open; then the chosen option's id, or `null` when cancelled.
          answer: early.get(entry.request),
        };
        permissions.set(entry.request, request);
        items.push(request);
        break;
      }
      case "question": {
        // Numbered with the permission requests (src/acp.rs, "Permission requests").
        const question = {
          type: "question",
          request: entry.request,
          message: entry.message,
          fields: entry.fields,
          // `undefined` while open; then `{ content }`, the form's values or `null` for a
          // skip, or `null` when closed unanswered (the run stopped, or the agent withdrew it).
          answer: early.get(entry.request),
        };
        permissions.set(entry.request, question);
        items.push(question);
        break;
      }
      case "answer": {
        const request = permissions.get(entry.request);
        if (request) request.answer = entry.option;
        else early.set(entry.request, entry.option);
        break;
      }
      case "reply": {
        const question = permissions.get(entry.request);
        if (question) question.answer = { content: entry.content };
        break;
      }
      case "unreadable":
        items.push({ type: "notice", text: `The agent sent something Postil could not read: ${entry.problem}` });
        break;
      case "failed":
        items.push({ type: "error", text: entry.error });
        break;
      // `started`, `cancelRequested` and `ended` show in the status line.
      default:
        break;
    }
  }
  return { plan, items };
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
  if (tool.status === "failed") state = "failed";
  else if (tool.status === "completed") {
    const stats = diffStats(tool.output);
    state = stats ? `+${stats.added} -${stats.removed}` : "done";
  } else state = live ? "" : "unfinished";
  const running = live && underWay(tool);
  return { verb, target, state, running };
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
      return { text: "Unfinished: Postil closed during it, or another window runs it", tone: "idle" };
  }
}

// Whether the run is still going: it can be stopped, and no other can start.
export function isLive(run) {
  return ["starting", "running", "waiting", "cancelling"].includes(run?.status);
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
// src/run.rs, `prompt_text`.
export function promptLines(text) {
  return text.replace(/\n$/, "").split("\n").map((line) => {
    if (/^\[N\d+\] /.test(line)) return { kind: "heading", text: line };
    if (line.startsWith("> ")) return { kind: "quote", text: line };
    if (line === "(These lines are no longer in the file; they were:)") return { kind: "lost", text: line };
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
    if (paragraph.length > 0) blocks.push({ type: "paragraph", inline: inline(paragraph.join("\n")) });
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
  const pattern = /(`+)([\s\S]+?)\1|\*\*([\s\S]+?)\*\*|(?<![\w*])[*_]([^*_\s][^*_]*?)[*_](?![\w*])/g;
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
