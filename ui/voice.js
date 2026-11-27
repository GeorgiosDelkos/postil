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

// Preserves original casing/punctuation inside the free-form tail after a prefix like "note "
// or "ask ", trimming only leading separator punctuation/spaces.
function extractTail(raw, wordCount) {
  const tokens = raw.trim().split(/\s+/);
  const rest = tokens.slice(wordCount).join(" ").trim();
  return rest.replace(/^[,.:;-]+\s*/, "").trim();
}

// Matches a full utterance against the fixed voice command grammar and checks `context` so a
// command with nothing to act on returns `{ type: "noop", reason }` instead of failing silently.
// Returns `null` when the utterance does not match any command.
export function command(transcript, context = {}) {
  const norm = normalize(transcript);
  if (!norm) return null;

  if (norm === "next") {
    if (!context.changesOpen) return { type: "noop", reason: "Nothing to step through." };
    return { type: "next", label: "Next" };
  }

  if (norm === "previous" || norm === "prev") {
    if (!context.changesOpen) return { type: "noop", reason: "Nothing to step through." };
    return { type: "previous", label: "Previous" };
  }

  if (norm === "save") {
    if (!context.composerOpen) return { type: "noop", reason: "Nothing to save." };
    return { type: "save", label: "Save" };
  }

  if (norm === "show map") {
    if (!context.projectOpen) return { type: "noop", reason: "No project open." };
    return { type: "show-map", label: "Show map" };
  }

  if (norm === "show code") {
    if (!context.projectOpen) return { type: "noop", reason: "No project open." };
    return { type: "show-code", label: "Show code" };
  }

  if (norm === "show changes") {
    if (!context.hasChanges) return { type: "noop", reason: "No changes to show." };
    return { type: "show-changes", label: "Show changes" };
  }

  if (norm === "resolve") {
    if (!context.noteDialogOpen) return { type: "noop", reason: "Nothing to resolve." };
    return { type: "resolve", label: "Resolve" };
  }

  if (norm === "send") {
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
    if (!context.liveRun) return { type: "noop", reason: "No run to stop." };
    return { type: "stop", label: "Stop run" };
  }

  if (norm.startsWith("note ")) {
    const text = extractTail(transcript, 1);
    if (!text) return null;
    if (!context.hasTarget) return { type: "noop", reason: "Nothing selected for a note." };
    return { type: "note", text, label: "Note" };
  }

  if (norm.startsWith("ask ")) {
    const text = extractTail(transcript, 1);
    if (!text) return null;
    if (!context.hasTarget) return { type: "noop", reason: "Nothing selected to ask about." };
    return { type: "ask", text, label: "Ask" };
  }

  if (norm.startsWith("open ")) {
    const query = norm.slice("open ".length).trim();
    if (!query) return null;
    if (!context.projectOpen) return { type: "noop", reason: "No project open." };
    return { type: "open", query, label: "Open" };
  }

  if (norm.startsWith("go to ")) {
    const name = extractTail(transcript, 2);
    if (!name) return null;
    if (!context.projectOpen) return { type: "noop", reason: "No project open." };
    return { type: "goto", name, label: "Go to" };
  }

  if (norm.startsWith("goto ")) {
    const name = extractTail(transcript, 1);
    if (!name) return null;
    if (!context.projectOpen) return { type: "noop", reason: "No project open." };
    return { type: "goto", name, label: "Go to" };
  }

  return null;
}
