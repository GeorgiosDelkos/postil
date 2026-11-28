// Pure logic for the multi-repo Agentic Developer Hub (filtering, priority grouping, lane
// summaries, and hippius-mem backend labels), tested directly with `node --test`.

export const PRIORITY_META = {
  P0: { id: "P0", label: "P0 · Critical", tone: "attention" },
  P1: { id: "P1", label: "P1 · High", tone: "note" },
  P2: { id: "P2", label: "P2 · Medium", tone: "accent" },
  P3: { id: "P3", label: "P3 · Low", tone: "muted" },
};

export const LANES = ["easy", "middle", "hard"];

// Filters Hub items by kind ("all" | "issue" | "pullRequest"), priority ("all" | "P0".."P3"),
// and a case-insensitive search query over title, repo name, number, and labels.
export function filterHubItems(items, { kind = "all", priority = "all", query = "" } = {}) {
  const needle = query.trim().toLowerCase();
  return items.filter((item) => {
    if (kind !== "all" && item.kind !== kind) return false;
    if (priority !== "all" && item.priority !== priority) return false;
    if (!needle) return true;
    const hay = [
      item.repoName,
      `#${item.number}`,
      item.title,
      item.priorityReason,
      ...(item.labels ?? []),
    ]
      .join(" ")
      .toLowerCase();
    return hay.includes(needle);
  });
}

// Groups already-sorted items into ordered priority sections, omitting empty buckets.
export function groupByPriority(items) {
  const order = ["P0", "P1", "P2", "P3"];
  const map = new Map(order.map((p) => [p, []]));
  for (const item of items) {
    const bucket = map.get(item.priority) ?? map.get("P2");
    bucket.push(item);
  }
  return order
    .map((priority) => ({
      priority,
      ...PRIORITY_META[priority],
      items: map.get(priority),
    }))
    .filter((group) => group.items.length > 0);
}

// Concise description of what each lane runs for an issue or pull request.
export function laneSummary(lane, kind) {
  if (kind === "pullRequest") {
    if (lane === "easy") return "1 Fast/Standard reviewer · no extra verification pass";
    if (lane === "middle") return "Primary reviewer + independent second-opinion verifier";
    return "Architecture context + 2 Frontier audits + synthesis judge";
  }
  if (lane === "easy") return "1 agent implements directly · no review stage";
  if (lane === "middle") return "Implementer + independent peer reviewer + judge";
  return "2 Frontier models plan & judge + impl + 2 reviews + final judge";
}

// Human-readable label for a lane chip.
export function laneLabel(lane) {
  if (lane === "easy") return "Easy";
  if (lane === "hard") return "Hard";
  return "Middle";
}

// Summarizes hippius-mem storage backend status for the Hub sidebar and Settings.
export function memoryBackendSummary(memoryView) {
  if (!memoryView) {
    return { kind: "unchecked", storage: "unknown", title: "hippius-mem", detail: "Checking…" };
  }
  const state = memoryView.state;
  const backend = memoryView.backend;
  if (state === null) {
    return { kind: "unchecked", storage: "unknown", title: "hippius-mem", detail: "Checking…" };
  }
  if (state.kind === "missing") {
    return {
      kind: "absent",
      storage: "unknown",
      title: "hippius-mem",
      detail: "Not installed on PATH",
    };
  }
  if (state.kind === "unconfigured") {
    const failed = Boolean(state.problem);
    return {
      kind: failed ? "missing" : "unchecked",
      storage: "unknown",
      title: "hippius-mem",
      detail: failed ? "Doctor check failed" : "Not configured yet",
    };
  }
  if (backend?.kind === "s3") {
    const where = backend.bucket ? `Remote S3 · ${backend.bucket}` : "Remote S3 (paid)";
    return { kind: "installed", storage: "s3", title: "hippius-mem", detail: where };
  }
  if (backend?.kind === "local") {
    return {
      kind: "installed",
      storage: "local",
      title: "hippius-mem",
      detail: "Local trial vault",
    };
  }
  return { kind: "installed", storage: "unknown", title: "hippius-mem", detail: "Ready" };
}

// Formats a Unix timestamp (seconds) or ISO date string as a compact relative age.
export function formatAge(value, nowMs = Date.now()) {
  if (!value) return "";
  const ms = typeof value === "number" ? value * 1000 : Date.parse(value);
  if (Number.isNaN(ms)) return "";
  const secs = Math.max(0, Math.floor((nowMs - ms) / 1000));
  if (secs < 60) return "just now";
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 48) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

// Stable unique key for an item in the Hub priority queue.
export function itemKey(item) {
  return `${item.repoPath}::${item.kind}::${item.number}`;
}
