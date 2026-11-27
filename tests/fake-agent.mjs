// A fake ACP agent for the tests of `src/acp/`: it speaks JSON-RPC over real pipes, one line
// per message, and plays the scenario named by its first argument. Messages follow the
// version 1 schema (see `src/acp/wire.rs` for the pinned commit); extra fields and kinds are
// the ones real agents were seen to send on 2026-11-23 (Grok's `_x.ai/...` notifications,
// Claude's `_auth/status_update`).
//
// What the agent received is echoed back as `agent_message_chunk` text, so a test can check
// what the app sent without reading the agent's input itself.

import { execFileSync, spawn } from "node:child_process";
import {
  closeSync,
  mkdirSync,
  readSync,
  renameSync,
  rmSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import { createInterface } from "node:readline";

const [scenario, ...extra] = process.argv.slice(2);

const send = (message) => process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
const update = (update) => send({ method: "session/update", params: { sessionId: "S1", update } });
// Each call is a message of its own, with an id as Claude's adapter sends.
let messages = 0;
const say = (text) => {
  messages += 1;
  update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text }, messageId: `m${messages}` });
};

// Replies from the app to the agent's own requests, by id.
const answers = new Map();
const answered = (id) => new Promise((resolve) => answers.set(id, resolve));
let nextId = 100;
const ask = (method, params) => {
  const id = nextId++;
  send({ id, method, params });
  return answered(id);
};

const permission = (toolCallId) =>
  ask("session/request_permission", {
    sessionId: "S1",
    toolCall: { toolCallId, title: "Edit src/a.rs", kind: "edit" },
    options: [
      { optionId: "once", name: "Allow", kind: "allow_once" },
      { optionId: "always", name: "Always allow", kind: "allow_always" },
      { optionId: "no", name: "Reject", kind: "reject_once" },
    ],
  });

// A form question, as Claude's adapter asks one (AskUserQuestion): one single-select field
// and its free-text companion. `method` is the name ACP's unstable schema gives at the pinned
// commit, or the one Claude's adapter sends.
const question = (method = "elicitation/create") =>
  ask(method, {
    mode: "form",
    sessionId: "S1",
    message: "Should divide(i32::MIN, -1) also return None?",
    requestedSchema: {
      type: "object",
      properties: {
        question_0: {
          type: "string",
          title: "Overflow",
          oneOf: [
            { const: "Yes", title: "Return None", description: "Use checked_div." },
            { const: "No", title: "Keep the panic" },
          ],
        },
        question_0_custom: { type: "string", title: "Other" },
      },
    },
  });

// A question the agent asked and left open when its turn ended ("leave-open").
let leftOpen = null;

// The client's capabilities, as `initialize` gave them.
let capabilities = null;

const initialize = (id) =>
  send({
    id,
    result: {
      protocolVersion: scenario === "version-2" ? 2 : 1,
      agentCapabilities: { loadSession: true, _meta: { "x.ai/fs_notify": true } },
      authMethods: [],
    },
  });

let cancelled;
const onCancel = new Promise((resolve) => (cancelled = resolve));

// The session's settings, as `session/new` reports them: a model select in the shape every
// agent probed on 2026-11-26 sent (flat options, plus a group, which the schema allows), after
// a boolean that also says "model", which is no select and must be passed over. Scenario
// "no-config" reports none, as an agent that offers no model choice. "long" has a name as long
// as real ones (OpenCode's, 2026-11-26), for the page's test that a model select shows it whole.
const MODELS = [
  { value: "m", name: "Model M" },
  { value: "pro", name: "Pro", description: "The careful one" },
  { value: "fast", name: "Fast" },
  {
    group: "more",
    name: "More",
    options: [
      { value: "deep", name: "Deep" },
      { value: "long", name: "OpenCode/Nemotron 3.5 Super VL Free" },
    ],
  },
];
const configOptions = (current) => [
  { id: "model", name: "Use the model", category: "model", type: "boolean", currentValue: true },
  { id: "model", name: "Model", category: "model", type: "select", currentValue: current, options: MODELS },
];
const offered = (value) =>
  MODELS.some((option) => option.value === value || option.options?.some((each) => each.value === value));
// The model the app set with `session/set_config_option`; the first message of each turn
// names it, so a test sees that it was set before the prompt.
let model = null;

// Prompts this process received, so a test can tell a kept agent from a new one.
let prompts = 0;
// A file in the session's working directory, as agents report paths: absolute.
const file = `${process.cwd()}/src/a.rs`;

// A citation as the analysis prompt asks for one: lines, and the text of the first.
const cite = (path, start, end, first) => ({ path, lines: [start, end], first });

// The diagrams an Analyzer answers the analysis prompt `text` with, by the kind it asks for.
// The tests' repository has src/a.rs ("fn a() {", "    one();", "}") and src/b.rs, whose
// "fn two() {" is on line 3.
function analysisReply(text) {
  const node = (key, kind, label, anchors, extra = {}) => ({
    key,
    kind,
    label,
    summary: `${label} in one line`,
    detail: `What ${label} does.`,
    anchors,
    ...extra,
  });
  if (text.startsWith("Analyze this repository")) {
    return {
      diagrams: [
        {
          kind: "architecture",
          title: "System",
          nodes: [
            // Right where it says.
            node("page", "component", "Page", [cite("src/a.rs", 2, 2, "one();")], { group: "proc" }),
            // Shifted: its first line is on line 3, named by the worktree's absolute path. Its
            // label is over the limit, and its group names no node.
            node("worker", "component", "W".repeat(70), [cite(`${process.cwd()}/src/b.rs`, 1, 2, "fn two() {")], {
              group: "nowhere",
            }),
            // Wrong: no line reads so.
            node("store", "store", "Store", [cite("src/a.rs", 1, 1, "no such line")]),
            // A key used twice.
            node("page", "component", "Again", []),
            node("proc", "boundary", "Process", [{ path: "src" }]),
          ],
          edges: [
            { from: "page", to: "worker", label: "calls", anchor: cite("src/a.rs", 2, 2, "one();") },
            { from: "page", to: "ghost", label: "to no node", anchor: null },
            { from: "worker", to: "worker", label: "to itself", anchor: null },
          ],
        },
        { kind: "data", title: "Types", nodes: [node("note", "store", "Note", [{ path: "src/a.rs" }])], edges: [] },
      ],
    };
  }
  if (text.startsWith("Analyze one part")) {
    return {
      diagrams: [
        {
          kind: "architecture",
          title: "Inside",
          nodes: [
            node("x", "component", "Inner", [cite("src/a.rs", 1, 1, "fn a() {")]),
            node("y", "component", "Other", [{ path: "src/b.rs" }]),
          ],
          edges: [{ from: "x", to: "y", label: "uses", anchor: null }],
        },
      ],
    };
  }
  if (text.startsWith("Trace one behavior")) {
    // In step order, which is neither the keys' order nor the edges': the last step loops back
    // to the first, as a retry does.
    const steps = ["send", "check", "apply"].map((key) => node(key, "component", key, [{ path: "src/a.rs" }]));
    return {
      diagrams: [
        {
          kind: "flow",
          title: "A flow",
          nodes: steps,
          edges: [
            { from: "send", to: "check", label: "then", anchor: null },
            { from: "check", to: "apply", label: "then", anchor: null },
            { from: "apply", to: "send", label: "again", anchor: null },
          ],
        },
      ],
    };
  }
  // An update: replaces the first outdated node and deletes the others, tries to replace and
  // to delete a healthy one, and adds a node with an edge to the replaced one. The added node's
  // summary is over the limit and its group names no node, and one more edge goes to no node.
  const outdated = [...text.matchAll(/^Outdated node (D\d+\.\d+)/gm)].map((match) => match[1]);
  const healthy = [...text.matchAll(/^- (D\d+\.\d+) "[^"\n]*"$/gm)].map((match) => match[1]);
  const diagram = text.match(/^Diagram (D\d+)/m)?.[1] ?? null;
  const [first, ...rest] = outdated;
  // The added node places the first file not on the map, when the prompt lists any, and
  // leaves the others where they are.
  const placed = text.match(/as a new node:\n- (\S+)\n/)?.[1] ?? "src/a.rs";
  const added = node("added", "component", "Added", [{ path: placed }], { group: "nowhere" });
  const nodes = [{ ...added, summary: "S".repeat(200) }];
  const edges = [{ from: "added", to: "ghost", label: "to no node", anchor: null }];
  if (first) {
    nodes.push({ ...node(null, "component", "Renewed", [cite("src/a.rs", 1, 1, "fn a() {")]), id: first });
    edges.push({ from: "added", to: first, label: "new", anchor: null });
  }
  if (healthy.length) nodes.push({ ...node(null, "component", "Hijacked", []), id: healthy[0] });
  return { diagrams: [{ id: diagram, nodes, edges, deleted: [...rest, ...healthy.slice(0, 1)] }] };
}

async function prompt(id, params) {
  prompts += 1;
  // Refuses its first prompt before doing anything, as an agent at its rate limit does, and
  // answers the ones after it.
  if (scenario === "refuse-first" && prompts === 1) {
    return send({ id, error: { code: -32603, message: "model overloaded" } });
  }
  // The model declines the work before doing any.
  if (scenario === "refusal") return send({ id, result: { stopReason: "refusal" } });
  const blocks = params.prompt.map((block) => (block.type === "text" ? block.text : block.uri));
  if (model !== null) say(`model ${model}`);
  say(`got ${blocks.join(" | ")}`);
  say(`prompt ${prompts}`);
  switch (scenario) {
    case "turn": {
      // Agents send notifications and update kinds the app does not use.
      send({ method: "_x.ai/session/setup", params: { phase: "auth" } });
      update({ sessionUpdate: "current_mode_update", currentModeId: "default" });
      update({
        sessionUpdate: "usage_update",
        used: 10,
        size: 100,
        cost: { amount: Number((0.02 * prompts).toFixed(4)), currency: "USD" },
      });
      const fs = await ask("fs/read_text_file", { sessionId: "S1", path: "/etc/hosts" });
      say(`fs answered ${fs.error?.code}`);
      update({ sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "hmm" } });
      update({
        sessionUpdate: "tool_call",
        toolCallId: "t1",
        title: "Edit src/a.rs",
        kind: "edit",
        status: "pending",
        locations: [{ path: file, line: 3 }],
      });
      // A kind newer than the schema, and null lists: the call still shows.
      update({
        sessionUpdate: "tool_call",
        toolCallId: "t2",
        title: "New tool",
        kind: "teleport",
        content: null,
        locations: null,
      });
      const answer = await permission("t1");
      say(`chose ${answer.result.outcome.optionId}`);
      update({
        sessionUpdate: "tool_call_update",
        toolCallId: "t1",
        status: "completed",
        content: [{ type: "diff", path: file, oldText: "a", newText: "b" }],
      });
      update({
        sessionUpdate: "plan",
        entries: [{ content: "Fix N1", priority: "high", status: "completed" }],
      });
      return send({
        id,
        result: {
          stopReason: "end_turn",
          usage: {
            inputTokens: 120,
            outputTokens: 30,
            cachedReadTokens: 40,
            thoughtTokens: 10,
            totalTokens: 160,
          },
        },
      });
    }
    case "cancel": {
      // A question waits for the engineer, so the run is waiting when the cancel comes.
      const answer = question();
      await onCancel;
      say(`question ${(await answer).result.action}`);
      // Requests that cross the cancel on the wire are answered too.
      const late = await permission("t2");
      say(`late permission ${late.result.outcome.outcome}`);
      const lateQuestion = await question();
      say(`late question ${lateQuestion.result.action}`);
      return send({ id, result: { stopReason: "cancelled" } });
    }
    case "ask": {
      // The first prompt asks by ACP's name and the second by Claude's adapter's.
      if (prompts === 1) say(`form ${JSON.stringify(capabilities.elicitation)}`);
      const method = prompts === 1 ? "elicitation/create" : "session/create_elicitation";
      const reply = await question(method);
      say(`answered ${JSON.stringify(reply.result)}`);
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "ask-withdraw": {
      const url = await ask("elicitation/create", { mode: "url", sessionId: "S1", message: "Sign in", url: "https://example.com", elicitationId: "e1" });
      say(`url answered ${url.error?.code}`);
      // A form with no fields has nothing to show.
      const empty = await ask("elicitation/create", { mode: "form", sessionId: "S1", message: "OK?", requestedSchema: { type: "object", properties: {} } });
      say(`empty form answered ${empty.error?.code}`);
      // Asked, then withdrawn before anyone answered: the agent no longer needs it.
      const withdrawn = nextId;
      const reply = question();
      send({ method: "$/cancel_request", params: { requestId: withdrawn } });
      // ACP asks for a reply to a withdrawn request all the same.
      say(`withdrawn answered ${(await reply).result.action}`);
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "always-only": {
      // No "once": Postil allows always, the option that allows.
      const reply = await ask("session/request_permission", {
        sessionId: "S1",
        toolCall: { toolCallId: "t1", title: "Edit src/a.rs", kind: "edit" },
        options: [
          { optionId: "no", name: "Reject", kind: "reject_once" },
          { optionId: "always", name: "Always allow", kind: "allow_always" },
        ],
      });
      say(`chose ${reply.result.outcome.optionId}`);
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "leave-open": {
      // The first turn ends with its question open; the second reports how it was answered.
      if (prompts === 1) {
        leftOpen = question();
        return send({ id, result: { stopReason: "end_turn" } });
      }
      say(`left open answered ${(await leftOpen).result.action}`);
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "reviewer": {
      // Plays both reviewers, told apart by the prompt (src/review.rs).
      if (blocks[0].includes("Do not change any file")) {
        const edit = await ask("session/request_permission", {
          sessionId: "S1",
          toolCall: { toolCallId: "e1", title: "Edit src/a.rs", kind: "edit" },
          options: [
            { optionId: "once", name: "Allow", kind: "allow_once" },
            { optionId: "no", name: "Reject", kind: "reject_once" },
          ],
        });
        say(`edit ${edit.result.outcome.optionId}`);
        const read = await ask("session/request_permission", {
          sessionId: "S1",
          toolCall: { toolCallId: "r1", title: "Read src/a.rs", kind: "read" },
          options: [
            { optionId: "once", name: "Allow", kind: "allow_once" },
            { optionId: "no", name: "Reject", kind: "reject_once" },
          ],
        });
        say(`read ${read.result.outcome.optionId}`);
        say(`saw the edit: ${blocks[0].includes("src/a.rs, before:\n```\na\n```\nafter:\n```\nb\n```")}`);
        // The fix quotes a fence, as a fix that shows a block does: the findings still read.
        say('Found one.\n\n```json\n[{"note": "N1", "path": "a.txt", "line": 1, "finding": "a is too short", "fix": "say more, as ```json [] ``` does"}]\n```');
      } else {
        say(`judged: ${blocks[0].includes("F1 [N1] a.txt:1: a is too short") ? "saw F1" : "no F1"}`);
        say("F1: accepted - said more");
      }
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "writes":
      // Changes the working tree for real, as an agent's edits do: one file changed, one new.
      writeFileSync(`${process.cwd()}/a.txt`, `a\nprompt ${prompts}\n`);
      writeFileSync(`${process.cwd()}/new.txt`, "new\n");
      return send({ id, result: { stopReason: "end_turn" } });
    case "writes-deep":
      // A new file in a folder of its own: outside the patterns of a sparse checkout.
      mkdirSync(`${process.cwd()}/outside`, { recursive: true });
      writeFileSync(`${process.cwd()}/outside/new.md`, `prompt ${prompts}\n`);
      return send({ id, result: { stopReason: "end_turn" } });
    case "asks-to-edit": {
      // An ask's agent (src/notes.rs, `ask_prompt`) that asks to edit, then to read, says how
      // each was answered, and ends its answer with a block shaped like an author's report.
      const options = [
        { optionId: "once", name: "Allow", kind: "allow_once" },
        { optionId: "no", name: "Reject", kind: "reject_once" },
      ];
      const edit = await ask("session/request_permission", {
        sessionId: "S1",
        toolCall: { toolCallId: "e1", title: "Edit a.txt", kind: "edit" },
        options,
      });
      say(`edit ${edit.result.outcome.optionId}`);
      const read = await ask("session/request_permission", {
        sessionId: "S1",
        toolCall: { toolCallId: "r1", title: "Read a.txt", kind: "read" },
        options,
      });
      say(`read ${read.result.outcome.optionId}`);
      say('It returns early.\n\n```json\n{"checks": [{"command": "cargo test", "result": "passed"}]}\n```');
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "reviewer-silent":
      // A proposer whose reply has no findings block.
      say("Looks fine to me.");
      return send({ id, result: { stopReason: "end_turn" } });
    case "reviewer-edits":
      // A proposer that edits without asking: no permission request reaches Postil.
      update({ sessionUpdate: "tool_call", toolCallId: "x1", title: "Edit a.txt", kind: "edit", status: "completed", content: [{ type: "diff", path: `${process.cwd()}/a.txt`, oldText: "a", newText: "z" }] });
      say("```json\n[]\n```");
      return send({ id, result: { stopReason: "end_turn" } });
    case "reviewer-writes":
      // A proposer that changes a file through a command: it reports no edit at all.
      writeFileSync(`${process.cwd()}/a.txt`, "rewritten\n");
      say("```json\n[]\n```");
      return send({ id, result: { stopReason: "end_turn" } });
    case "reviewer-writes-cut":
      // A proposer that changes a file and is cut short: its turn does not end well.
      writeFileSync(`${process.cwd()}/a.txt`, "rewritten\n");
      return send({ id, result: { stopReason: "max_tokens" } });
    case "reviewer-writes-hangs":
      // A proposer that changes a file, then never ends its turn and ignores the cancel.
      writeFileSync(`${process.cwd()}/a.txt`, "rewritten\n");
      return;
    case "reviewer-commits":
      // A proposer that edits and commits its edit, which leaves no change behind.
      writeFileSync(`${process.cwd()}/a.txt`, "rewritten\n");
      execFileSync("git", ["commit", "--quiet", "--all", "--message", "reviewed"]);
      say("```json\n[]\n```");
      return send({ id, result: { stopReason: "end_turn" } });
    case "reshapes":
      // Every kind of change a patch carries: an edit, a deletion, a rename, a binary file.
      writeFileSync(`${process.cwd()}/a.txt`, "a\nedited\n");
      rmSync(`${process.cwd()}/b.txt`);
      renameSync(`${process.cwd()}/c.txt`, `${process.cwd()}/d.txt`);
      writeFileSync(`${process.cwd()}/bin.dat`, Buffer.from([0, 1, 2, 255, 0, 7]));
      return send({ id, result: { stopReason: "end_turn" } });
    case "report-reviewed": {
      // Plays the author and a proposing reviewer, told apart by the prompt (src/review.rs).
      // The author edits a.txt and rates its hunk mechanical; the reviewer finds something on
      // a.txt's line 2, which must show the hunk unfolded as eyes, and something on no file.
      if (blocks[0].includes("Do not change any file")) {
        const findings = [
          { path: "a.txt", line: 2, finding: "reported is not mechanical" },
          { finding: "the change has no test" },
        ];
        say(`Found two.\n\n\`\`\`json\n${JSON.stringify(findings)}\n\`\`\``);
        return send({ id, result: { stopReason: "end_turn" } });
      }
      writeFileSync(`${process.cwd()}/a.txt`, "a\nreported\n");
      const steps = [{ path: "a.txt", lines: [1, 2], why: "the change", risk: "mechanical" }];
      say(`N1: done\n\n\`\`\`json\n${JSON.stringify({ steps, checks: [] })}\n\`\`\``);
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "report": {
      // Edits a.txt and deletes b.txt for real, says what it did, and ends with the author's
      // report (src/report.rs): steps on an edited file, on the deleted one (its lines name
      // nothing left), on a path outside the repository, on the edited file by its absolute
      // path with a rating Postil does not know, and on lines that are no range; a check, and
      // one whose result is no claim.
      writeFileSync(`${process.cwd()}/a.txt`, "a\nreported\n");
      rmSync(`${process.cwd()}/b.txt`);
      say("N1: done");
      const report = {
        steps: [
          { path: "a.txt", lines: [1, 2], why: "the change", risk: "routine" },
          { path: "b.txt", lines: [3, 4], why: "no longer used", risk: "mechanical" },
          { path: "../x", lines: [1, 1], why: "outside", risk: "eyes" },
          { path: `${process.cwd()}/a.txt`, lines: [2, 2], why: "absolute", risk: "weird" },
          { path: "a.txt", lines: [0, 1], why: "no range", risk: "eyes" },
        ],
        checks: [
          { command: "cargo test", result: "passed" },
          { command: "cargo build", result: "maybe" },
        ],
      };
      say(`\`\`\`json\n${JSON.stringify(report, null, 2)}\n\`\`\``);
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "evidence": {
      // Two commands and a report that claims three checks (ui/run.js, `evidence`): one
      // passed as claimed, one that exited non-zero as Grok ends it (completed) though the
      // report says passed, and one the log never shows.
      update({ sessionUpdate: "tool_call", toolCallId: "c1", title: "cargo test", kind: "execute", status: "completed", rawInput: { command: "cargo test" }, rawOutput: { exit_code: 0 } });
      update({ sessionUpdate: "tool_call", toolCallId: "c2", title: "cargo clippy", kind: "execute", status: "completed", rawInput: { command: "cargo clippy" }, rawOutput: { exit_code: 101 } });
      const checks = ["cargo test", "cargo clippy", "cargo doc"].map((command) => ({ command, result: "passed" }));
      say(`N1: done\n\n\`\`\`json\n${JSON.stringify({ steps: [], checks })}\n\`\`\``);
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "report-bad":
      // A json block that is an object, whose steps are not a list.
      say('N1: done\n\n```json\n{"steps": "first a.txt"}\n```');
      return send({ id, result: { stopReason: "end_turn" } });
    case "report-twice":
      // A first report, text, a second report, and text after it: the second is the report.
      say('```json\n{"checks": [{"command": "first", "result": "passed"}]}\n```\nThen I ran more.');
      say('```json\n{"checks": [{"command": "second", "result": "failed"}]}\n```\nThat is all.');
      return send({ id, result: { stopReason: "end_turn" } });
    case "report-cancel":
      // A report written, then a turn that ends stopped: a stopped run reads no report.
      say('```json\n{"checks": [{"command": "cargo test", "result": "passed"}]}\n```');
      await onCancel;
      return send({ id, result: { stopReason: "cancelled" } });
    case "checks": {
      // Execute calls in the shapes Claude's adapter 0.94.0 and Grok 1.0.92 sent on
      // 2026-11-25 through Postil's own connection. c1 as Claude's adapter: the kind only on
      // the call, whose input is empty; the command in updates, then a blank one, which keeps
      // it; output that happens to start like a failure.
      update({ sessionUpdate: "tool_call", toolCallId: "c1", title: "Terminal", kind: "execute", status: "pending", rawInput: {} });
      update({ sessionUpdate: "tool_call_update", toolCallId: "c1", title: "cargo test", rawInput: { command: "cargo test" } });
      update({ sessionUpdate: "tool_call_update", toolCallId: "c1", rawInput: { command: "cargo  test", description: "Run the tests" } });
      update({ sessionUpdate: "tool_call_update", toolCallId: "c1", rawInput: { command: "  " } });
      update({ sessionUpdate: "tool_call_update", toolCallId: "c1", status: "completed", rawOutput: "Exit code 3\nis what the test printed" });
      // c2 as Grok: no kind on the call, then the kind and input on an update; exit_code 0
      // while it runs; a failure that ends as completed.
      update({ sessionUpdate: "tool_call", toolCallId: "c2", title: "run_terminal_command", rawInput: { command: "cargo build" } });
      update({ sessionUpdate: "tool_call_update", toolCallId: "c2", kind: "execute", title: "Execute `cargo build`", rawInput: { variant: "Bash", command: "cargo build" } });
      update({ sessionUpdate: "tool_call_update", toolCallId: "c2", status: "in_progress", rawOutput: { exit_code: 0 } });
      update({ sessionUpdate: "tool_call_update", toolCallId: "c2", status: "completed", rawOutput: { exit_code: 101 } });
      // c3 as Claude's adapter when a command fails; a command too long to keep whole.
      update({ sessionUpdate: "tool_call", toolCallId: "c3", title: "Terminal", kind: "execute", status: "pending", rawInput: {} });
      // Two-byte characters, so byte 4096 falls inside one.
      update({ sessionUpdate: "tool_call_update", toolCallId: "c3", rawInput: { command: `echo ${"é".repeat(2600)}` } });
      update({ sessionUpdate: "tool_call_update", toolCallId: "c3", status: "failed", rawOutput: "Exit code 2\nno" });
      // c4 reported whole in one call: kind, command and exit code together, a success.
      update({ sessionUpdate: "tool_call", toolCallId: "c4", title: "make", kind: "execute", status: "completed", rawInput: { command: "make" }, rawOutput: { exit_code: 0 } });
      // A read whose input has a command field, before and after: not an execute call.
      update({ sessionUpdate: "tool_call", toolCallId: "r1", title: "Read a.txt", kind: "read", status: "pending", rawInput: { command: "read", path: "a.txt" } });
      update({ sessionUpdate: "tool_call_update", toolCallId: "r1", status: "completed", rawInput: { command: "read" }, rawOutput: { exit_code: 1 } });
      // An edit after the first check.
      update({ sessionUpdate: "tool_call", toolCallId: "e1", title: "Edit a.txt", kind: "edit", status: "completed", content: [{ type: "diff", path: `${process.cwd()}/a.txt`, oldText: "a", newText: "b" }] });
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "reject-only": {
      // No option allows, so Postil cannot answer it alone: the engineer does.
      const reply = await ask("session/request_permission", {
        sessionId: "S1",
        toolCall: { toolCallId: "t1", title: "Delete everything", kind: "delete" },
        options: [{ optionId: "no", name: "Reject", kind: "reject_once" }],
      });
      say(`chose ${reply.result.outcome.optionId}`);
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "stream": {
      // An edit, then one reply streamed in parts, slowly enough to act on the panel meanwhile.
      update({ sessionUpdate: "tool_call", toolCallId: "t1", title: "Edit src/a.rs", kind: "edit", status: "pending", locations: [{ path: file, line: 1 }] });
      update({ sessionUpdate: "tool_call_update", toolCallId: "t1", status: "completed", content: [{ type: "diff", path: file, oldText: "one\ntwo\nthree\n", newText: "one\n2\nthree\n" }] });
      for (let part = 0; part < 40; part += 1) {
        await new Promise((resolve) => setTimeout(resolve, 300));
        update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: `part ${part} ` }, messageId: "reply" });
      }
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "flood": {
      // An edit, then a reply streamed in a part every 20 ms until the engineer stops the
      // run, as a fast agent writes: the page's end-to-end tests (tests/e2e/) click while the
      // page redraws many times a second. The reply is one line about the first note sent,
      // as agents answer, so what the page says the agent said about it changes on every
      // part. 3000 parts (a minute) bound a run nobody stops.
      const note = blocks.join("\n").match(/\[(N\d+)\]/)?.[1];
      if (!note) throw new Error("flood: the prompt names no note");
      update({ sessionUpdate: "tool_call", toolCallId: "t1", title: "Edit src/a.rs", kind: "edit", status: "pending", locations: [{ path: file, line: 1 }] });
      // Made for real, so the run's changes have lines (`GET api/run/changes`).
      writeFileSync(file, "one\n2\nthree\n");
      update({ sessionUpdate: "tool_call_update", toolCallId: "t1", status: "completed", content: [{ type: "diff", path: file, oldText: "one\ntwo\nthree\n", newText: "one\n2\nthree\n" }] });
      update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: `[${note}]: ` }, messageId: "reply" });
      let stopped = false;
      onCancel.then(() => (stopped = true));
      for (let part = 0; part < 3000 && !stopped; part += 1) {
        await new Promise((resolve) => setTimeout(resolve, 20));
        update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: `part ${part} ` }, messageId: "reply" });
      }
      return send({ id, result: { stopReason: stopped ? "cancelled" : "end_turn" } });
    }
    case "analysis":
      // An Analyzer (src/analysis/prompt.rs) that answers each kind of analysis its prompt asks
      // for, as its last json block.
      say(`\`\`\`json\n${JSON.stringify(analysisReply(blocks[0]), null, 2)}\n\`\`\``);
      return send({ id, result: { stopReason: "end_turn" } });
    case "analysis-empty":
      say('Nothing to draw.\n\n```json\n{"diagrams": [{"kind": "architecture", "title": "None", "nodes": [], "edges": []}]}\n```');
      return send({ id, result: { stopReason: "end_turn" } });
    case "analysis-edits":
      // An Analyzer that edits without asking, then replies with diagrams that would read.
      writeFileSync(`${process.cwd()}/a.txt`, "edited\n");
      say(`\`\`\`json\n${JSON.stringify(analysisReply(blocks[0]), null, 2)}\n\`\`\``);
      return send({ id, result: { stopReason: "end_turn" } });
    case "hang":
      // Never ends the turn, and ignores the cancel.
      return;
    case "exit-after-turn":
      send({ id, result: { stopReason: "end_turn" } });
      return process.exit(0);
    case "exit-leaving-stderr": {
      // A grandchild keeps standard error open after the agent exits, as one an agent starts
      // can: the reader then waits out its grace for the agent's last words. Once the agent
      // has exited, the grandchild writes `<pid file>.exited`, which a test cannot learn from
      // the pid: an exited agent stays a zombie until it is reaped. The grandchild learns it
      // from its standard input, a pipe only the agent writes to: the pipe ends when the
      // agent exits. It then stays, holding standard error, until its group is killed.
      const exited = JSON.stringify(`${extra[0]}.exited`);
      const watch = `process.stdin.on("end", () => {
          require("node:fs").writeFileSync(${exited}, "x");
          setInterval(() => {}, 1000);
        }).resume();`;
      spawn(process.execPath, ["-e", watch], { stdio: ["pipe", "ignore", "inherit"] });
      send({ id, result: { stopReason: "end_turn" } });
      return process.exit(0);
    }
    case "garbage": {
      process.stdout.write("not json\n");
      send({ result: {} });
      update({ sessionUpdate: "tool_call", title: "no id" });
      send({ id: 7, method: "session/request_permission", params: { options: "none" } });
      const fs = await answered(7);
      say(`bad permission answered ${fs.error?.code}`);
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "long-line":
      // A line of exactly the line limit the tests build with (`LINE_LIMIT` in
      // src/acp/connection.rs, 1 MiB), newline included, which is read whole; then 1.5 MiB
      // with no newline until the end, which is over it; then a message.
      process.stdout.write(`${"y".repeat((1 << 20) - 1)}\n`);
      process.stdout.write(`${"x".repeat(3 << 19)}\n`);
      say("after the long line");
      return send({ id, result: { stopReason: "end_turn" } });
    case "odd-error":
      // An error object without a code, though JSON-RPC requires one.
      return send({ id, error: { message: "no code here" } });
    case "refuse-prompt":
      return send({ id, error: { code: -32603, message: "model overloaded" } });
    case "exit-mid-turn":
      process.stderr.write("panicked at the model\n");
      return process.exit(1);
    default:
      return send({ id, result: { stopReason: "end_turn" } });
  }
}

// These agents say who they are, so a test can check that they are gone.
const tellsPid = ["silent", "exit-after-turn", "exit-leaving-stderr", "reviewer-silent"];
if (tellsPid.includes(scenario) && extra[0]) {
  writeFileSync(extra[0], String(process.pid));
}

// "close-stdin": answers `initialize` and `session/new`, then closes its standard input and
// runs on, so the app's next write fails. It reads and writes the descriptors itself: Node's
// stdin stream never closes descriptor 0, and its stdout stream may still be writing while
// the reads here block.
const closeStdinAndRunOn = () => {
  let input = "";
  const readLine = () => {
    const buffer = Buffer.alloc(4096);
    while (!input.includes("\n")) {
      const read = readSync(0, buffer);
      if (read === 0) process.exit(1);
      input += buffer.toString("utf8", 0, read);
    }
    const end = input.indexOf("\n");
    const line = input.slice(0, end);
    input = input.slice(end + 1);
    return JSON.parse(line);
  };
  const reply = (id, result) => writeSync(1, `${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
  reply(readLine().id, { protocolVersion: 1 });
  const { id } = readLine();
  // Closed before the session is open, so no write of the app's can still get through.
  closeSync(0);
  reply(id, { sessionId: "S1" });
  setInterval(() => {}, 1000);
};

const handle = (line) => {
  const message = JSON.parse(line);
  if (message.method === undefined) {
    return answers.get(message.id)?.(message);
  }
  switch (message.method) {
    case "initialize":
      capabilities = message.params.clientCapabilities;
      if (scenario === "silent") return;
      if (scenario === "no-session-id") return initialize(message.id);
      if (scenario === "exit-on-start") {
        process.stderr.write("Please sign in: run `fake login`\n");
        return process.exit(3);
      }
      return initialize(message.id);
    case "session/new":
      if (scenario === "no-session-id") return send({ id: message.id, result: {} });
      if (scenario === "auth") {
        return send({ id: message.id, error: { code: -32000, message: "Authentication required" } });
      }
      if (scenario === "spawns-child") {
        // A grandchild in the agent's process group, as `npx` leaves `node` running. Its pid
        // goes to the file the test named, so the test can check it is gone after the drop.
        const child = spawn("sleep", ["1000"], { stdio: "ignore" });
        writeFileSync(extra[0], String(child.pid));
      }
      send({ method: "_auth/status_update", params: { authStatus: { kind: "account" } } });
      if (scenario === "no-config") {
        return send({ id: message.id, result: { sessionId: "S1", models: { currentModelId: "m" } } });
      }
      // No `models`, as Claude's adapter sends none: the model in effect is read from the
      // model select alone.
      return send({ id: message.id, result: { sessionId: "S1", configOptions: configOptions("m") } });
    case "session/set_config_option": {
      const { configId, value } = message.params;
      if (scenario === "no-config" || configId !== "model" || !offered(value)) {
        return send({ id: message.id, error: { code: -32602, message: `no option ${configId}=${value}` } });
      }
      model = value;
      return send({ id: message.id, result: { configOptions: configOptions(value) } });
    }
    case "session/prompt":
      return prompt(message.id, message.params);
    case "session/cancel":
      return cancelled();
    default:
      return send({ id: message.id, error: { code: -32601, message: "no" } });
  }
};

if (scenario === "close-stdin") closeStdinAndRunOn();
else createInterface({ input: process.stdin }).on("line", handle);
