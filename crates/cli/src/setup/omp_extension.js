// ZJ_RADAR_OMP_EXTENSION=v1
//
// zj-radar bridge extension for omp (oh-my-pi, github.com/can1357/oh-my-pi).
// Vendored by `zj-radar setup omp` into omp's auto-loaded user extensions dir
// (~/.omp/agent/extensions/zj-radar-omp.js; see crates/cli/src/setup/omp.rs
// for the $PI_CONFIG_DIR / profile / $PI_CODING_AGENT_DIR resolution), so no
// config edit is needed and a clean uninstall is one file delete.
//
// omp is a fork of pi, and this is a sibling of the pi bridge
// (pi_extension.js) — same spawn discipline, different event names:
// omp settles on `agent_end` (no `agent_settled`), surfaces dialogs as the
// `ask` tool and `tool_approval_*` (no `ui_prompt_*`), reports /new as
// `session_switch`, and rebinds every extension factory into each subagent
// session (`ctx.agent.kind === "sub"`).
//
// Spawn discipline (load-bearing, see CONTEXT.md → Status contract / Bounded
// sends): this bridge spawns `zj-radar notify omp --status <s>` per event with
// the payload JSON on stdin. The `zellij pipe` send is bounded by the CLI path,
// so the bridge adds no second pipe client. Spawns are never concurrent:
// status edges go out strictly FIFO, while tool-activity `running` refreshes
// coalesce to the latest unsent one (latest-wins is the project's ordering
// rule). Each child has a hard 10 s kill timer; a failed child never breaks
// the queue.
// ASYNC SPAWN ONLY — never a synchronous spawn: the extension runs in omp's
// process, and omp awaits every handler in turn, so handlers enqueue and
// return at once. The one exception is `session_shutdown`, which drains the
// queue (bounded below omp's 2 s shutdown-handler budget).
//
// Module globals are shared by every session omp binds this factory into
// (main, subagents, the next session after /new), so the queue lives on
// globalThis: ordering holds across all of them.
//
// The bridge picks the status class; the Rust adapter (crates/cli/src/agents/
// omp.rs) owns the refinements keyed off the payload's `event` field, which is
// zj-radar's own vocabulary so an omp API change lands here only.
import { spawn as nodeSpawn } from "node:child_process";

const KILL_AFTER_MS = 10_000;
const DRAIN_BOUND_MS = 1_500;

// The only tool_input keys the Rust side ever reads: `tool_activity` and
// `bash_activity` (crates/cli/src/agents.rs) read `file_path`/`notebook_path`/
// `command`; the omp adapter renames omp's `path` to `file_path` before that
// lookup. Keep this list in sync with those readers — a `write` tool's
// `content` arg alone can run hundreds of KB.
const TOOL_INPUT_KEYS = ["path", "file_path", "notebook_path", "command"];

// Free-text cap for `prompt`/`message` (and each kept tool arg). The Rust side
// bounds every wire field at MAX_WIRE_FIELD_CHARS (512) anyway; this only stops
// a 20 MB paste from being stringified and written to a child on every event.
const TEXT_CAP = 4096;

function trimToolInput(args) {
  if (!args || typeof args !== "object" || Array.isArray(args)) return undefined;
  const out = {};
  for (const key of TOOL_INPUT_KEYS) {
    if (typeof args[key] === "string") out[key] = capHead(args[key]);
  }
  return out;
}

// A string slice that never leaves a lone UTF-16 surrogate at either cut:
// JSON.stringify would escape it as `\udXXX`, which serde_json rejects.
function sliceWhole(s, start, end) {
  let a = start;
  let b = end;
  if (a > 0 && a < s.length && isLow(s.charCodeAt(a))) a++;
  if (b > a && b < s.length && isLow(s.charCodeAt(b))) b--;
  return s.slice(a, b);
}
function isLow(c) { return c >= 0xdc00 && c <= 0xdfff; }

// The prompt's reader (`task_from_prompt`) takes its first non-empty line.
function capHead(s) {
  return s.length <= TEXT_CAP ? s : sliceWhole(s, 0, TEXT_CAP);
}

// A message's readers are the rail's msg (the head) and the trailing-question
// remap (the last non-empty line): keep both ends.
function capHeadTail(s) {
  if (s.length <= TEXT_CAP) return s;
  const half = TEXT_CAP / 2;
  return `${sliceWhole(s, 0, half)}\n…\n${sliceWhole(s, s.length - half, s.length)}`;
}

function shared() {
  const key = Symbol.for("zj-radar.omp");
  const s = (globalThis[key] ??= {});
  s.spawn ??= nodeSpawn;
  s.queue ??= [];
  if (s.pendingRunning === undefined) s.pendingRunning = null;
  if (s.processing === undefined) s.processing = null;
  // Set by the main session once it arms (TUI inside Zellij); subagent
  // sessions forward their blocking dialogs only when it is.
  s.armed ??= false;
  // The last settled status (done/error/idle), for a dialog closed while
  // omp is idle: the row returns to where it was, error included.
  s.lastSettled ??= { status: "idle", message: "" };
  return s;
}

function enqueue(status, payload) {
  const s = shared();
  // Only tool-activity refreshes are droppable; the prompt carries the task.
  if (status === "running" && payload.event === "tool") {
    s.pendingRunning = payload;
  } else {
    if (s.pendingRunning !== null) {
      s.queue.push({ status: "running", payload: s.pendingRunning });
      s.pendingRunning = null;
    }
    s.queue.push({ status, payload });
  }
  if (s.processing === null) s.processing = processQueue(s);
}

async function processQueue(s) {
  while (s.queue.length > 0 || s.pendingRunning !== null) {
    let item = s.queue.shift();
    if (item === undefined) {
      item = { status: "running", payload: s.pendingRunning };
      s.pendingRunning = null;
    }
    try {
      await notify(s, item.status, item.payload);
    } catch {}
  }
  s.processing = null;
}

// Bounded async spawn: write the JSON payload to stdin, close it; a hard kill
// timer reaps a wedged child. Resolves when the child exits, errors, or is
// killed — never rejects.
function notify(s, status, payload) {
  return new Promise((resolve) => {
    let child;
    try {
      child = s.spawn("zj-radar", ["notify", "omp", "--status", status], {
        stdio: ["pipe", "ignore", "ignore"],
      });
    } catch {
      resolve();
      return;
    }
    let timer = null;
    const done = () => {
      if (timer !== null) clearTimeout(timer);
      resolve();
    };
    // ENOENT (zj-radar not on PATH) arrives here asynchronously; an
    // unhandled `error` event would crash omp.
    child.on("error", done);
    child.on("exit", done);
    timer = setTimeout(() => {
      try { child.kill(); } catch {}
      done();
    }, KILL_AFTER_MS);
    // A reaper must never keep omp's process alive on its own.
    timer.unref?.();
    // EPIPE (writing to a child that already exited without reading stdin)
    // arrives asynchronously as an `error` event on child.stdin — unhandled,
    // it's an uncaught exception that tears down the TUI. Attach before writing.
    if (child.stdin && typeof child.stdin.on === "function") child.stdin.on("error", () => {});
    try {
      child.stdin.write(JSON.stringify(payload));
      child.stdin.end();
    } catch {}
  });
}

// Wait for the queue to empty, at most DRAIN_BOUND_MS.
function drain() {
  const s = shared();
  if (s.processing === null) return Promise.resolve();
  // Deliberately NOT unref'd: omp awaits this, and a sole unref'd timer would
  // let the event loop exit mid-await. It is bounded, so it can't hold omp.
  return Promise.race([s.processing, new Promise((r) => setTimeout(r, DRAIN_BOUND_MS))]);
}

// The joined text parts of an assistant message.
function textOf(message) {
  if (!message || !Array.isArray(message.content)) return "";
  return message.content
    .filter((c) => c && c.type === "text" && typeof c.text === "string")
    .map((c) => c.text)
    .join("\n")
    .trim();
}

// The last assistant message of a finished run.
function lastAssistant(messages) {
  if (!Array.isArray(messages)) return null;
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (m && m.role === "assistant") return m;
  }
  return null;
}

// omp rebinds every extension factory into each subagent session; its
// handlers see `ctx.agent.kind === "sub"` there. An omp without the field
// predates subagent identity: treat it as the main session.
function isMain(ctx) {
  return !(ctx.agent && ctx.agent.kind === "sub");
}

// The first question of an `ask` tool call — what the user is looking at.
function askTitle(args) {
  const qs = args && Array.isArray(args.questions) ? args.questions : [];
  const q = qs.find((x) => x && typeof x.question === "string" && x.question.trim());
  return q ? q.question.trim() : "";
}

export default function (pi) {
  // Per-session state: omp rebinds this factory into every session (main,
  // each subagent, the next one after /new), each with its own closure.
  let armed = false;
  let cwd = "";
  // Prompt text stashed by `input` until the run actually starts (`input`
  // fires at submission ingress, before command interpretation and model
  // validation — sending running there could strand the row).
  let stashedPrompt = null;
  // The run's last assistant message, from `message_end`: the fallback when
  // `agent_end` carries no messages.
  let last = null;

  const emit = (status, payload) => {
    const out = { ...payload, cwd };
    if (typeof out.prompt === "string") out.prompt = capHead(out.prompt);
    if (typeof out.message === "string") out.message = capHeadTail(out.message);
    enqueue(status, out);
  };
  // The main session's lifecycle; subagent sessions never send these (their
  // runs are tool calls inside the main run, not the pane's state).
  const send = (status, payload) => {
    if (armed) emit(status, payload);
  };
  // A dialog blocks the user wherever it was raised, so subagents forward
  // these too — but only once the main session armed the bridge, and only
  // from a session with a UI: a UI-less subagent's approval auto-denies at
  // once, and forwarding it would flash a false "needs you".
  const sendDialog = (ctx, status, payload) => {
    if (isMain(ctx) ? armed : shared().armed && ctx.hasUI === true) emit(status, payload);
  };
  const dialogEnd = (ctx) => {
    const idle = typeof ctx.isIdle === "function" ? ctx.isIdle() : false;
    if (!idle || !isMain(ctx)) {
      sendDialog(ctx, "running", { event: "ui_prompt.end" });
    } else {
      const { status, message } = shared().lastSettled;
      sendDialog(ctx, status, { event: "ui_prompt.end", message });
    }
  };
  // Every handler is wrapped: a bridge bug must never throw into omp.
  const on = (name, fn) => pi.on(name, (event, ctx) => {
    try {
      return fn(event || {}, ctx || {});
    } catch {}
  });

  on("session_start", (_event, ctx) => {
    cwd = typeof ctx.cwd === "string" && ctx.cwd ? ctx.cwd : process.cwd();
    // pi's bridge may share this dir (a `$PI_CODING_AGENT_DIR` both read);
    // never report a pi process as omp.
    armed = isMain(ctx) && ctx.mode === "tui" && Boolean(process.env.ZELLIJ) && process.title !== "pi";
    if (armed) shared().armed = true;
  });

  on("session_switch", (event) => {
    // A different session must not inherit the previous one's settled state.
    shared().lastSettled = { status: "idle", message: "" };
    stashedPrompt = null;
    // Only /new resets the row (Claude wires only SessionStart{clear}).
    if (event.reason === "new") send("idle", { event: "session.new" });
  });

  on("input", (event, ctx) => {
    if (event.source === "extension" || typeof event.text !== "string") return;
    // `input` precedes command interpretation: a slash command is not a task.
    if (event.text.trimStart().startsWith("/")) return;
    const idle = typeof ctx.isIdle === "function" ? ctx.isIdle() : true;
    if (!idle) {
      // A steer joins the current run: no new agent_start is coming.
      send("running", { event: "prompt", prompt: event.text });
    } else {
      stashedPrompt = event.text;
    }
  });

  on("agent_start", () => {
    last = null;
    const payload = { event: "prompt" };
    if (stashedPrompt !== null) payload.prompt = stashedPrompt;
    stashedPrompt = null;
    send("running", payload);
  });

  on("tool_execution_start", (event, ctx) => {
    if (event.toolName === "ask") {
      sendDialog(ctx, "pending", { event: "ui_prompt.start", message: askTitle(event.args) || "needs input" });
      return;
    }
    send("running", { event: "tool", tool: event.toolName, tool_input: trimToolInput(event.args) });
  });

  on("tool_execution_end", (event, ctx) => {
    if (event.toolName === "ask") dialogEnd(ctx);
  });

  on("tool_approval_requested", (event, ctx) => {
    const tool = typeof event.toolName === "string" && event.toolName ? event.toolName : "tool";
    sendDialog(ctx, "pending", { event: "ui_prompt.start", message: `Allow ${tool}?` });
  });

  on("tool_approval_resolved", (_event, ctx) => dialogEnd(ctx));

  on("message_end", (event) => {
    const m = event.message;
    if (m && m.role === "assistant") last = m;
  });

  on("agent_end", (event) => {
    // A scheduled continuation (auto-retry, a reminder, a background job the
    // turn waits on) is not a settle: the run is still going.
    if (event.willContinue === true) return;
    const final = lastAssistant(event.messages) || last;
    const stop = final && final.stopReason;
    let settled;
    if (stop === "error") {
      const message = typeof final.errorMessage === "string" ? final.errorMessage : "";
      settled = { status: "error", message };
      send("error", { event: "error", message });
    } else {
      // An abort (Esc) is the user's own action: done, and no
      // trailing-question remap (blank message).
      const message = stop === "aborted" ? "" : textOf(final);
      settled = { status: "done", message };
      send("done", { event: "settled", message });
    }
    if (armed) shared().lastSettled = { status: settled.status, message: capHeadTail(settled.message) };
    last = null;
    stashedPrompt = null;
  });

  on("session_shutdown", () => {
    // Fired on process exit only (session switches are `session_switch`).
    send("idle", { event: "session.end" });
    return drain();
  });
}
