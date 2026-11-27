// Voice command grammar (docs/review-assist.md, "Voice commands").
//
// `command(transcript, context) -> Command | null` is pure so `ui/voice.test.js` tests every
// command, normalization rule, rejected utterance, and empty-context guard without a browser.
//
// Destructive actions (Apply, Discard, deleting a note, saving a memory, opening a pull
// request) intentionally have no voice command.

const NUMBER_WORDS = new Map([
  ["zero", "0"],
  ["one", "1"],
  ["two", "2"],
  ["three", "3"],
  ["four", "4"],
  ["five", "5"],
  ["six", "6"],
  ["seven", "7"],
  ["eight", "8"],
  ["nine", "9"],
  ["ten", "10"],
  ["eleven", "11"],
  ["twelve", "12"],
  ["thirteen", "13"],
  ["fourteen", "14"],
  ["fifteen", "15"],
  ["sixteen", "16"],
  ["seventeen", "17"],
  ["eighteen", "18"],
  ["nineteen", "19"],
  ["twenty", "20"],
]);

const TRAILING_PUNCT = /[.,!?;:"'()[\]{}]+$/;

// Normalizes an utterance for command matching: lowercases, strips surrounding/sentence
// punctuation, converts standalone number words to digits, and collapses whitespace.
export function normalize(transcript) {
  if (typeof transcript !== "string") return "";
  const cleaned = transcript
    .trim()
    .replace(/[.,!?;:"'()[\]{}]+/g, " ")
    .toLowerCase();
  if (!cleaned) return "";
  return cleaned
    .split(/\s+/)
    .filter(Boolean)
    .map((word) => NUMBER_WORDS.get(word) ?? word)
    .join(" ");
}

// Strips a matched command prefix from `raw` while preserving original casing and punctuation
// in the remaining tail (handling punctuation attached directly to the keyword, e.g. "Note:x").
function extractAfterPrefix(raw, prefixRe) {
  return raw.trim().replace(prefixRe, "").trim();
}

// Matches a full utterance against the fixed voice command grammar and checks `context` so a
// command with nothing to act on returns `{ type: "noop", reason }` instead of failing silently.
// Returns `null` when the utterance does not match any command.
export function command(transcript, context = {}) {
  const norm = normalize(transcript);
  if (!norm) return null;

  // Resolve is the one command allowed while a modal dialog (specifically the note dialog) is open.
  if (norm === "resolve") {
    if (!context.noteDialogOpen) return { type: "noop", reason: "Nothing to resolve." };
    return { type: "resolve", label: "Resolve" };
  }

  const blockedByDialog = () =>
    context.dialogOpen ? { type: "noop", reason: "Close the dialog first." } : null;

  if (norm === "next") {
    return (
      blockedByDialog() ??
      (context.changesOpen
        ? { type: "next", label: "Next" }
        : { type: "noop", reason: "Nothing to step through." })
    );
  }

  if (norm === "previous" || norm === "prev") {
    return (
      blockedByDialog() ??
      (context.changesOpen
        ? { type: "previous", label: "Previous" }
        : { type: "noop", reason: "Nothing to step through." })
    );
  }

  if (norm === "save") {
    return (
      blockedByDialog() ??
      (context.composerOpen
        ? { type: "save", label: "Save" }
        : { type: "noop", reason: "Nothing to save." })
    );
  }

  if (norm === "show map") {
    return (
      blockedByDialog() ??
      (context.projectOpen
        ? { type: "show-map", label: "Show map" }
        : { type: "noop", reason: "No project open." })
    );
  }

  if (norm === "show code") {
    return (
      blockedByDialog() ??
      (context.projectOpen
        ? { type: "show-code", label: "Show code" }
        : { type: "noop", reason: "No project open." })
    );
  }

  if (norm === "show changes") {
    return (
      blockedByDialog() ??
      (context.hasChanges
        ? { type: "show-changes", label: "Show changes" }
        : { type: "noop", reason: "No changes to show." })
    );
  }

  if (norm === "send") {
    const blocked = blockedByDialog();
    if (blocked) return blocked;
    if (!context.sendOpen && !context.canSend) {
      return { type: "noop", reason: "Nothing to send." };
    }
    return {
      type: "send",
      confirm: Boolean(context.sendOpen),
      label: context.sendOpen ? "Send notes" : "Open send",
    };
  }

  if (norm === "stop") {
    return (
      blockedByDialog() ??
      (context.liveRun
        ? { type: "stop", label: "Stop run" }
        : { type: "noop", reason: "No run to stop." })
    );
  }

  if (norm.startsWith("note ")) {
    const text = extractAfterPrefix(transcript, /^note\b[\s,.:;-]*/i);
    if (!text) return null;
    return (
      blockedByDialog() ??
      (context.hasTarget
        ? { type: "note", text, label: "Note" }
        : { type: "noop", reason: "Nothing selected for a note." })
    );
  }

  if (norm.startsWith("ask ")) {
    const text = extractAfterPrefix(transcript, /^ask\b[\s,.:;-]*/i);
    if (!text) return null;
    return (
      blockedByDialog() ??
      (context.hasTarget
        ? { type: "ask", text, label: "Ask" }
        : { type: "noop", reason: "Nothing selected to ask about." })
    );
  }

  if (norm.startsWith("open ")) {
    const query = norm.slice("open ".length).trim();
    if (!query) return null;
    return (
      blockedByDialog() ??
      (context.projectOpen
        ? { type: "open", query, label: "Open" }
        : { type: "noop", reason: "No project open." })
    );
  }

  if (norm.startsWith("go to ")) {
    const name = extractAfterPrefix(transcript, /^go\s+to\b[\s,.:;-]*/i)
      .replace(TRAILING_PUNCT, "")
      .trim();
    if (!name) return null;
    return (
      blockedByDialog() ??
      (context.projectOpen
        ? { type: "goto", name, label: "Go to" }
        : { type: "noop", reason: "No project open." })
    );
  }

  if (norm.startsWith("goto ")) {
    const name = extractAfterPrefix(transcript, /^goto\b[\s,.:;-]*/i)
      .replace(TRAILING_PUNCT, "")
      .trim();
    if (!name) return null;
    return (
      blockedByDialog() ??
      (context.projectOpen
        ? { type: "goto", name, label: "Go to" }
        : { type: "noop", reason: "No project open." })
    );
  }

  return null;
}
