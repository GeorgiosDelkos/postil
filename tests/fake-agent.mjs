// A fake ACP agent for the tests of `src/acp/`: it speaks JSON-RPC over real pipes, one line
// per message, and plays the scenario named by its first argument. Messages follow the
// version 1 schema (see `src/acp/wire.rs` for the pinned commit); extra fields and kinds are
// the ones real agents were seen to send on 2026-11-23 (Grok's `_x.ai/...` notifications,
// Claude's `_auth/status_update`).
//
// What the agent received is echoed back as `agent_message_chunk` text, so a test can check
// what the app sent without reading the agent's input itself.

import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
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

// Prompts this process received, so a test can tell a kept agent from a new one.
let prompts = 0;
// A file in the session's working directory, as agents report paths: absolute.
const file = `${process.cwd()}/src/a.rs`;

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
  say(`got ${blocks.join(" | ")}`);
  say(`prompt ${prompts}`);
  switch (scenario) {
    case "turn": {
      // Agents send notifications and update kinds the app does not use.
      send({ method: "_x.ai/session/setup", params: { phase: "auth" } });
      update({ sessionUpdate: "usage_update", used: 10, size: 100 });
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
      return send({ id, result: { stopReason: "end_turn" } });
    }
    case "cancel": {
      const answer = permission("t1");
      await onCancel;
      say(`permission ${(await answer).result.outcome.outcome}`);
      // A request that crosses the cancel on the wire is answered too.
      const late = await permission("t2");
      say(`late permission ${late.result.outcome.outcome}`);
      return send({ id, result: { stopReason: "cancelled" } });
    }
    case "hang":
      // Never ends the turn, and ignores the cancel.
      return;
    case "exit-after-turn":
      send({ id, result: { stopReason: "end_turn" } });
      return process.exit(0);
    case "garbage": {
      process.stdout.write("not json\n");
      send({ result: {} });
      update({ sessionUpdate: "tool_call", title: "no id" });
      send({ id: 7, method: "session/request_permission", params: { options: "none" } });
      const fs = await answered(7);
      say(`bad permission answered ${fs.error?.code}`);
      return send({ id, result: { stopReason: "end_turn" } });
    }
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
if (["silent", "exit-after-turn"].includes(scenario) && extra[0]) {
  writeFileSync(extra[0], String(process.pid));
}

createInterface({ input: process.stdin }).on("line", (line) => {
  const message = JSON.parse(line);
  if (message.method === undefined) {
    return answers.get(message.id)?.(message);
  }
  switch (message.method) {
    case "initialize":
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
      return send({ id: message.id, result: { sessionId: "S1", models: { currentModelId: "m" } } });
    case "session/prompt":
      return prompt(message.id, message.params);
    case "session/cancel":
      return cancelled();
    default:
      return send({ id: message.id, error: { code: -32601, message: "no" } });
  }
});
