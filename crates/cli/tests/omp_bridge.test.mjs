// node --test harness for the vendored omp bridge (crates/cli/src/setup/omp_extension.js).
// Loads a fresh module instance per "session binding" (omp rebinds the factory
// into every session; module globals are shared, which the globalThis queue
// models), drives a fake event bus, and records every `zj-radar notify omp`
// spawn through the shared-state spawn seam.
import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { chmodSync, copyFileSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const SRC = join(dirname(fileURLToPath(import.meta.url)), "../src/setup/omp_extension.js");
const KEY = Symbol.for("zj-radar.omp");
const DIR = mkdtempSync(join(tmpdir(), "zjr-omp-"));
let loads = 0;

// A fresh ESM instance each call (copied to a unique .mjs so node treats it
// as ESM and never serves it from the module cache).
async function loadRuntime(agent = { kind: "main", id: "Main", name: "main", depth: 0 }) {
  const file = join(DIR, `ext-${loads++}.mjs`);
  copyFileSync(SRC, file);
  const mod = await import(pathToFileURL(file).href);
  const handlers = new Map();
  const pi = { on: (name, fn) => handlers.set(name, fn) };
  mod.default(pi);
  let idle = true;
  const ctx = { mode: "tui", cwd: "/repo", isIdle: () => idle, agent };
  return {
    ctx,
    setIdle: (v) => { idle = v; },
    emit: (name, event = {}) => handlers.get(name)?.({ type: name, ...event }, ctx),
  };
}

// Fake spawn: records {status, payload}; `hang` keeps the child alive.
let sent;
let behavior;
function fakeSpawn(cmd, args) {
  const child = new EventEmitter();
  let buf = "";
  if (behavior === "epipe") {
    // A real dead child's stdin fires `error` asynchronously — never as a
    // synchronous throw from write()/end() — after the write is attempted.
    const stdin = new EventEmitter();
    stdin.write = (d) => { buf += d; };
    stdin.end = () => {
      queueMicrotask(() => stdin.emit("error", Object.assign(new Error("write EPIPE"), { code: "EPIPE" })));
    };
    child.stdin = stdin;
    child.kill = () => child.emit("exit", null, "SIGTERM");
    queueMicrotask(() => child.emit("exit", 1, null));
    return child;
  }
  child.stdin = { write: (d) => { buf += d; }, end: () => {} };
  child.kill = () => child.emit("exit", null, "SIGTERM");
  if (behavior === "enoent") {
    queueMicrotask(() => child.emit("error", Object.assign(new Error("spawn zj-radar ENOENT"), { code: "ENOENT" })));
    return child;
  }
  assert.equal(cmd, "zj-radar");
  assert.deepEqual(args.slice(0, 3), ["notify", "omp", "--status"]);
  const mode = behavior;
  queueMicrotask(() => {
    sent.push({ status: args[3], payload: JSON.parse(buf) });
    // "slow" keeps each child alive 5 ms so a backlog forms (coalescing,
    // cross-runtime ordering); "hang" never exits (bounded-drain test).
    if (mode !== "hang") setTimeout(() => child.emit("exit", 0, null), mode === "slow" ? 5 : 0);
  });
  return child;
}

// Poll `pred` until true or `ms` elapses (then fail loudly): no fixed sleeps,
// so a loaded CI box only makes a test slower, never flaky.
async function until(pred, ms = 5000) {
  const t0 = Date.now();
  while (!pred()) {
    if (Date.now() - t0 > ms) throw new Error(`condition not met within ${ms}ms`);
    await new Promise((r) => setTimeout(r, 2));
  }
}
// Every handler enqueues synchronously, so "settled" is exactly "the shared
// queue has drained" (processQueue nulls `processing` after the last exit;
// it is unset if nothing was ever sent).
const settle = () => until(() => !globalThis[KEY].processing);
const events = () => sent.map((s) => `${s.status}:${s.payload.event}`);

beforeEach(() => {
  sent = [];
  behavior = "ok";
  process.env.ZELLIJ = "0";
  globalThis[KEY] = { spawn: fakeSpawn };
});

const assistant = (text, extra = {}) => ({ role: "assistant", content: [{ type: "text", text }], stopReason: "stop", ...extra });
const user = (text) => ({ role: "user", content: [{ type: "text", text }] });
const end = (...messages) => ({ messages });
const sub = { kind: "sub", id: "0-Explore", name: "explore", depth: 1, parentId: "Main" };

test("prompt → tool → settled", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("input", { text: "fix the flaky test", source: "interactive" });
  rt.setIdle(false);
  rt.emit("agent_start");
  rt.emit("tool_execution_start", { toolCallId: "1", toolName: "read", args: { path: "/repo/a.rs", sel: "1-20" } });
  rt.setIdle(true);
  rt.emit("agent_end", end(user("fix the flaky test"), assistant("All green.")));
  await settle();
  assert.deepEqual(events(), ["running:prompt", "running:tool", "done:settled"]);
  assert.equal(sent[0].payload.prompt, "fix the flaky test");
  assert.equal(sent[0].payload.cwd, "/repo");
  assert.deepEqual(sent[1].payload.tool_input, { path: "/repo/a.rs" });
  assert.equal(sent[2].payload.message, "All green.");
});

test("session_start alone sends nothing", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  await settle();
  assert.deepEqual(events(), []);
});

test("slash commands and extension input are never a task", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("input", { text: "/model", source: "interactive" });
  rt.emit("input", { text: "injected", source: "extension" });
  rt.emit("agent_start");
  await settle();
  assert.deepEqual(events(), ["running:prompt"]);
  assert.equal(sent[0].payload.prompt, undefined);
});

test("input with no run following sends nothing, and never leaks into a later run", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("input", { text: "hello", source: "interactive" }); // rejected pre-run
  rt.emit("session_switch", { reason: "resume" });
  rt.emit("agent_start"); // an auto-continuation, no new input
  await settle();
  assert.deepEqual(events(), ["running:prompt"]);
  assert.equal(sent[0].payload.prompt, undefined);
});

test("steer mid-run sends prompt immediately", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.setIdle(false);
  rt.emit("agent_start");
  rt.emit("input", { text: "also update the docs", source: "interactive" });
  await settle();
  assert.deepEqual(events(), ["running:prompt", "running:prompt"]);
  assert.equal(sent[1].payload.prompt, "also update the docs");
});

test("a scheduled continuation is not a settle", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("agent_start");
  rt.emit("agent_end", { messages: [assistant("", { stopReason: "error", errorMessage: "overloaded" })], willContinue: true });
  rt.emit("agent_end", { messages: [assistant("waiting on the job")], willContinue: true, awaitingAsyncWork: true });
  await settle();
  assert.deepEqual(events(), ["running:prompt"]);
  rt.emit("agent_end", end(assistant("Recovered and done.")));
  await settle();
  assert.deepEqual(events(), ["running:prompt", "done:settled"]);
});

test("Esc abort is done with a blank message", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("agent_start");
  rt.emit("agent_end", end(assistant("Should I continue?", { stopReason: "aborted" })));
  await settle();
  assert.deepEqual(events(), ["running:prompt", "done:settled"]);
  assert.equal(sent[1].payload.message, "");
});

test("provider error is error with its message", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("agent_start");
  rt.emit("agent_end", end(assistant("", { stopReason: "error", errorMessage: "429 rate limited" })));
  await settle();
  assert.deepEqual(events(), ["running:prompt", "error:error"]);
  assert.equal(sent[1].payload.message, "429 rate limited");
});

test("agent_end without messages falls back to the last message_end", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("agent_start");
  rt.emit("message_end", { message: assistant("From message_end.") });
  rt.emit("agent_end", {});
  await settle();
  assert.equal(sent.at(-1).payload.message, "From message_end.");
});

test("ask tool is pending with its first question, then running", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.setIdle(false);
  rt.emit("agent_start");
  const args = { questions: [{ question: "  " }, { question: "Which database?", options: [{ label: "pg" }] }] };
  rt.emit("tool_execution_start", { toolCallId: "a", toolName: "ask", args });
  rt.emit("tool_execution_end", { toolCallId: "a", toolName: "ask", result: {}, isError: false });
  await settle();
  assert.deepEqual(events(), ["running:prompt", "pending:ui_prompt.start", "running:ui_prompt.end"]);
  assert.equal(sent[1].payload.message, "Which database?");
});

test("ask with no readable question gets a generic label", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("tool_execution_start", { toolName: "ask", args: null });
  await settle();
  assert.equal(sent[0].payload.message, "needs input");
});

test("tool approval is pending, resolved returns to running", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.setIdle(false);
  rt.emit("agent_start");
  rt.emit("tool_approval_requested", { toolCallId: "1", toolName: "bash", approvalMode: "always-ask" });
  rt.emit("tool_approval_resolved", { toolCallId: "1", toolName: "bash", approved: true });
  await settle();
  assert.deepEqual(events(), ["running:prompt", "pending:ui_prompt.start", "running:ui_prompt.end"]);
  assert.equal(sent[1].payload.message, "Allow bash?");
});

test("subagent sessions are silent except for dialogs, once main armed", async () => {
  const headless = await loadRuntime(sub);
  headless.ctx.mode = "print"; // omp's real subagent binding: no UI
  headless.ctx.hasUI = false;
  const child = await loadRuntime(sub);
  child.ctx.hasUI = true;
  child.emit("session_start");
  child.emit("tool_approval_requested", { toolName: "write" }); // main not armed yet
  await settle();
  assert.deepEqual(events(), []);

  const main = await loadRuntime();
  main.emit("session_start");
  main.setIdle(false);
  main.emit("agent_start");
  headless.emit("session_start");
  headless.emit("tool_approval_requested", { toolName: "bash" }); // auto-denied: never surfaces
  headless.emit("tool_approval_resolved", { toolName: "bash", approved: false });
  child.emit("agent_start");
  child.emit("tool_execution_start", { toolName: "read", args: { path: "/x" } });
  child.emit("tool_approval_requested", { toolName: "write" });
  child.setIdle(true); // a sub's own idleness never restores a settled state
  child.emit("tool_approval_resolved", { toolName: "write", approved: false });
  child.emit("agent_end", end(assistant("sub finished")));
  child.emit("session_shutdown");
  await settle();
  assert.deepEqual(events(), ["running:prompt", "pending:ui_prompt.start", "running:ui_prompt.end"]);
  assert.equal(sent[1].payload.message, "Allow write?");
});

test("dialog while idle after an error restores the error", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("agent_start");
  rt.emit("agent_end", end(assistant("", { stopReason: "error", errorMessage: "boom" })));
  rt.setIdle(true);
  rt.emit("tool_execution_start", { toolName: "ask", args: { questions: [{ question: "Retry?" }] } });
  rt.emit("tool_execution_end", { toolName: "ask" });
  await settle();
  assert.deepEqual(events().slice(-2), ["pending:ui_prompt.start", "error:ui_prompt.end"]);
  assert.equal(sent.at(-1).payload.message, "boom");
});

test("/new sends session.new and forgets the settled state; resume only forgets", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("agent_start");
  rt.emit("agent_end", end(assistant("", { stopReason: "error", errorMessage: "boom" })));
  rt.emit("session_switch", { reason: "resume", previousSessionFile: "/s/a.jsonl" });
  rt.emit("tool_execution_start", { toolName: "ask", args: { questions: [{ question: "Q?" }] } });
  rt.emit("tool_execution_end", { toolName: "ask" });
  rt.emit("session_switch", { reason: "new" });
  await settle();
  assert.deepEqual(events(), [
    "running:prompt",
    "error:error",
    "pending:ui_prompt.start",
    "idle:ui_prompt.end",
    "idle:session.new",
  ]);
  assert.equal(sent[3].payload.message, "");
});

test("queue order holds across session bindings", async () => {
  behavior = "slow";
  const a = await loadRuntime();
  const b = await loadRuntime();
  a.emit("session_start");
  a.emit("agent_start");
  a.emit("agent_end", end(assistant("partial", { stopReason: "aborted" })));
  // b's edge is pushed while a's slow spawns are still queued: FIFO regardless.
  b.emit("session_start");
  b.emit("session_switch", { reason: "new" });
  await settle();
  assert.deepEqual(events(), ["running:prompt", "done:settled", "idle:session.new"]);
});

test("shutdown sends session.end and drains before resolving", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("agent_start");
  await rt.emit("session_shutdown");
  assert.deepEqual(events(), ["running:prompt", "idle:session.end"]);
});

test("shutdown drain is bounded under omp's 2 s handler budget", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  behavior = "hang";
  rt.emit("agent_start");
  const t0 = Date.now();
  await rt.emit("session_shutdown");
  const took = Date.now() - t0;
  assert.ok(took >= 1400 && took < 2000, `drain took ${took}ms`);
});

test("stdin EPIPE and spawn errors do not crash omp or stall the queue", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  for (const bad of ["epipe", "enoent"]) {
    behavior = bad;
    rt.emit("agent_start");
    await settle();
  }
  behavior = "ok";
  rt.emit("agent_end", end(assistant("ok")));
  await settle();
  assert.deepEqual(events(), ["done:settled"]);
});

test("handlers tolerate malformed events", async () => {
  const rt = await loadRuntime();
  rt.ctx.agent = undefined; // an omp predating subagent identity reads as main
  rt.emit("session_start", {});
  for (const [name, ev] of [
    ["input", { text: 42 }],
    ["tool_execution_start", { toolName: null, args: null }],
    ["tool_execution_start", { toolName: "ask", args: { questions: "nope" } }],
    ["message_end", { message: { role: "assistant", content: "not-an-array" } }],
    ["agent_end", { messages: "nope" }],
    ["agent_end", undefined],
    ["tool_approval_requested", {}],
    ["session_switch", undefined],
  ]) {
    assert.doesNotThrow(() => rt.emit(name, ev), name);
  }
  await settle();
  assert.ok(sent.length > 0, "a missing ctx.agent still arms the main session");
});

test("non-tui mode, no ZELLIJ, and a pi process are silent", async () => {
  const rt = await loadRuntime();
  rt.ctx.mode = "rpc";
  rt.emit("session_start");
  rt.emit("agent_start");
  const saved = process.title;
  process.title = "pi";
  try {
    const inPi = await loadRuntime();
    inPi.emit("session_start");
    inPi.emit("agent_start");
  } finally {
    process.title = saved;
  }
  delete process.env.ZELLIJ;
  const rt2 = await loadRuntime();
  rt2.emit("session_start");
  rt2.emit("agent_start");
  await settle();
  assert.deepEqual(events(), []);
});

test("coalesces tool refreshes but never drops edges", async () => {
  behavior = "slow";
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("agent_start");
  for (let i = 0; i < 5; i++) rt.emit("tool_execution_start", { toolName: "read", args: { path: `/f${i}` } });
  rt.emit("agent_end", end(assistant("done")));
  await settle();
  const ev = events();
  assert.equal(ev[0], "running:prompt");
  assert.equal(ev.at(-1), "done:settled");
  assert.ok(ev.filter((e) => e === "running:tool").length <= 2, ev.join(","));
});

test("prompt keeps its head and message keeps both ends under the cap, surrogate-safe", async () => {
  const rt = await loadRuntime();
  rt.emit("session_start");
  rt.emit("input", { text: "a".repeat(4095) + "😀".repeat(10), source: "interactive" });
  rt.emit("agent_start");
  const body = "All green.\n" + "m".repeat(3_000_000) + "😀\nShall I push?";
  rt.emit("agent_end", end(assistant(body)));
  await settle();
  const p = sent[0].payload.prompt;
  const m = sent[1].payload.message;
  const lone = /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;
  assert.ok(p.length <= 4096 && !lone.test(p), `prompt ${p.length}`);
  assert.ok(m.length <= 4096 + 3 && m.startsWith("All green.\n") && m.endsWith("\nShall I push?"));
});

test("real child that exits without reading a large stdin: no crash", async () => {
  const bin = mkdtempSync(join(DIR, "bin-"));
  const log = join(bin, "log");
  writeFileSync(join(bin, "zj-radar"), `#!/bin/sh\necho "$2 $4" >> "${log}"\nexit 0\n`);
  chmodSync(join(bin, "zj-radar"), 0o755);
  const savedPath = process.env.PATH;
  const uncaught = [];
  const onUncaught = (e) => uncaught.push(e);
  process.on("uncaughtException", onUncaught);
  globalThis[KEY] = {}; // no seam: the bridge's own node:child_process spawn
  process.env.PATH = bin;
  try {
    const rt = await loadRuntime();
    rt.ctx.cwd = "/" + "x".repeat(1_000_000);
    rt.emit("session_start");
    rt.emit("agent_start");
    rt.emit("agent_end", end(assistant("ok")));
    await settle();
    await new Promise((r) => setTimeout(r, 20));
  } finally {
    process.env.PATH = savedPath;
    process.off("uncaughtException", onUncaught);
  }
  assert.deepEqual(uncaught, [], "no uncaught exception may escape into omp");
  assert.deepEqual(readFileSync(log, "utf8").split("\n").filter(Boolean), ["omp running", "omp done"]);
});
