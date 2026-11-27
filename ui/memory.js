// Team memory on the page (docs/review-assist.md, "Postil shows memory"): the rules the
// Memory tab (memory-tab.js), the run dialog's Memory section and a memory's dialog
// (memory-dialog.js) share, and how they ask src/memory.rs. A route answers `{state:
// "pending"}` until the answer is in, and the page asks again every 250 ms while it is: the
// owner chose that over a new nudge on 2026-11-26.

// How often a pending answer is asked for again, in ms: the settings dialog's rescan poll's.
export const MEMORY_POLL_MS = 250;

// The most of a memory's body the dialog shows, in UTF-8 bytes (docs/review-assist-plan.md,
// step 9.2): a teammate's text can be any length.
export const MEMORY_BODY_LIMIT = 64 * 1024;

// The longest summary and body a Save sends, in UTF-8 bytes (src/memory.rs,
// `SUMMARY_WRITE_LIMIT` and `BODY_WRITE_LIMIT`): checked here too, so the dialog says so before
// the server refuses.
export const MEMORY_SUMMARY_LIMIT = 1024;
export const MEMORY_WRITE_LIMIT = 16 * 1024;

// The kinds a memory can be, as hippius-mem's `remember` names them, the likeliest first.
export const NOTE_TYPES = ["gotcha", "convention", "decision", "context", "reference"];

// How many UTF-8 bytes `text` is.
export function utf8Length(text) {
  return new TextEncoder().encode(text).length;
}

// Why the Remember dialog's text cannot be saved as it is, or null when it can: a blank
// summary or body, one over its limit, or the repository's scope when the repository has no
// name in team memory (`memoryRepo` null: no origin remote).
export function checkMemoryDraft({ summary, body, scope, memoryRepo }) {
  if (!summary.trim()) return "Write a summary.";
  if (!body.trim()) return "Write the memory.";
  const summaryBytes = utf8Length(summary);
  if (summaryBytes > MEMORY_SUMMARY_LIMIT) {
    return `The summary is ${summaryBytes} bytes; at most ${MEMORY_SUMMARY_LIMIT} are sent.`;
  }
  const bodyBytes = utf8Length(body);
  if (bodyBytes > MEMORY_WRITE_LIMIT) {
    return `The memory is ${bodyBytes} bytes; at most ${MEMORY_WRITE_LIMIT} (16 KiB) are sent.`;
  }
  if (scope === "repo" && !memoryRepo) {
    return "This repository has no origin remote, which names it in team memory: save it for the whole team.";
  }
  return null;
}

// Whether a failed Save is hippius-mem refusing a summary close to one it holds, which
// "Save anyway" writes all the same (src/memory.rs, "Only the engineer's Save writes").
export function isNearDuplicateError(error) {
  return typeof error === "string" && error.includes("near-duplicate");
}

// A memory id as hippius-mem writes them; nothing else is sent (src/memory.rs, `MemoryId`).
export function isMemoryId(id) {
  return typeof id === "string" && /^mem_[0-9A-Z]+$/.test(id);
}

// The pointers a recall found, split: `strong` are likely matches and `weak` guesses, which
// show dimmed under "Possibly related". `empty` when nothing strong was found, weak guesses or
// not: hippius-mem says that when every pointer is weak, the team has probably recorded nothing.
export function partitionPointers(pointers) {
  const strong = pointers.filter((pointer) => !pointer.weak);
  const weak = pointers.filter((pointer) => pointer.weak);
  return { strong, weak, empty: strong.length === 0 };
}

// `text` cut to at most `limit` UTF-8 bytes, on a character boundary: `{ text, cut }`.
export function cutBody(text, limit = MEMORY_BODY_LIMIT) {
  const bytes = new TextEncoder().encode(text);
  if (bytes.length <= limit) return { text, cut: false };
  // A UTF-8 continuation byte is 10xxxxxx: back up to the start of the character it is in.
  let end = limit;
  while (end > 0 && (bytes[end] & 0xc0) === 0x80) end -= 1;
  return { text: new TextDecoder().decode(bytes.subarray(0, end)), cut: true };
}

// How long ago `updatedMs` was, at `nowMs`, both in milliseconds since the epoch, as
// hippius-mem gives them: "now", "5m", "3h", "4d".
export function memoryAge(updatedMs, nowMs) {
  const minutes = Math.floor(Math.max(0, nowMs - updatedMs) / 60_000);
  if (minutes < 1) return "now";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

// An author's SS58 address, shortened for a card: its first and last four characters.
export function shortAuthor(address) {
  return address.length > 12 ? `${address.slice(0, 4)}…${address.slice(-4)}` : address;
}

// Where an op of a memory's history was anchored (src/memory/client.rs, `Anchor`).
export function anchorWords(anchor) {
  return {
    none: "not anchored yet",
    local: "anchored in the vault",
    chain: "anchored on chain",
  }[anchor];
}

// What a reply that is not ready says, or null for a ready or pending one.
export function replyProblem(reply) {
  if (reply.state === "failed") return reply.error;
  if (reply.state === "unavailable") return reply.message;
  if (reply.state === "incompatible") {
    const missing = reply.missing.join(", ");
    return `This hippius-mem (${reply.serverVersion}) does not match Postil ${reply.postilVersion}: it lacks ${missing}.`;
  }
  return null;
}

// Asks `url` with `method`, and `body` when given, and again every MEMORY_POLL_MS while the
// answer is pending, calling `onReply(reply)` with each answer, pending ones included. A failed
// request is a failed answer. Returns a function that stops asking; no `onReply` comes after it.
export function pollMemory(url, { method = "GET", body, onReply }) {
  let stopped = false;
  let timer = null;
  async function ask() {
    let reply;
    try {
      const response = await fetch(url, body === undefined ? { method } : { method, body });
      reply = response.ok
        ? await response.json()
        : { state: "failed", error: await response.text() };
    } catch (error) {
      reply = { state: "failed", error: String(error) };
    }
    if (stopped) return;
    onReply(reply);
    if (reply.state === "pending") timer = setTimeout(ask, MEMORY_POLL_MS);
  }
  void ask();
  return () => {
    stopped = true;
    clearTimeout(timer);
  };
}
