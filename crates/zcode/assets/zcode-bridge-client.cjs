// GateForge zcode bridge client — one JSON-line request on stdin, one JSON-line
// result on stdout. Drives the locally installed ZCode runtime (server dist
// preferred, desktop CLI app-server fallback) using the same wire clients as
// the 9router executor.
//
// Why node: the server dist closes its stdio mid-handshake when its client is
// a non-node process (verified against node clients with byte-identical
// frames succeeding where the Rust client got EOF). Running the protocol
// client in node keeps the pipe topology identical to the proven 9router
// path; GateForge keeps gateway/store/translation native.
//
// Request:  { mode, command, args, env, jwt, model, providerId,
//             requireStartPlan, allowTools, body, timeouts }
// Result:   { ok: true, text } | { ok: false, error }
const { spawn } = require("node:child_process");
const { existsSync, readFileSync } = require("node:fs");
const { homedir } = require("node:os");
const { join } = require("node:path");
const { randomUUID } = require("node:crypto");

// --- server-dist binary framing (zcode-server-protocol.js) ------------------

const HEADER_SIZE = 13;
const REGULAR_MESSAGE = 1;
const INITIALIZE_MESSAGE = 200;
const RESPONSE_MESSAGE = 201;
const ERROR_MESSAGE = 202;
const CANCELED_MESSAGE = 203;
const MAX_FRAME_BYTES = 32 * 1024 * 1024;

function encodeVql(value, out) {
  let remaining = value;
  do {
    let next = remaining % 128;
    remaining = Math.floor(remaining / 128);
    if (remaining > 0) next |= 0x80;
    out.push(next);
  } while (remaining > 0);
}

function encodeZcodeValue(value) {
  const out = [];
  if (value === undefined) out.push(0);
  else if (typeof value === "string") {
    const bytes = Buffer.from(value, "utf8");
    out.push(1);
    encodeVql(bytes.length, out);
    out.push(...bytes);
  } else if (Array.isArray(value)) {
    out.push(4);
    encodeVql(value.length, out);
    value.forEach((item) => out.push(...encodeZcodeValue(item)));
  } else if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) {
    out.push(6);
    encodeVql(value, out);
  } else {
    const bytes = Buffer.from(JSON.stringify(value), "utf8");
    out.push(5);
    encodeVql(bytes.length, out);
    out.push(...bytes);
  }
  return out;
}

function decodeVql(data, offset) {
  let value = 0;
  let multiplier = 1;
  for (let i = 0; i < 8; i += 1) {
    if (offset.cursor >= data.length) throw new Error("Truncated ZCode vql");
    const next = data[offset.cursor++];
    value += (next & 0x7f) * multiplier;
    if ((next & 0x80) === 0) return value;
    multiplier *= 128;
  }
  throw new Error("Invalid ZCode vql");
}

function decodeZcodeValue(data, offset) {
  if (offset.cursor >= data.length) throw new Error("Truncated ZCode value");
  const type = data[offset.cursor++];
  if (type === 0) return undefined;
  if (type === 1 || type === 2) {
    const length = decodeVql(data, offset);
    const end = offset.cursor + length;
    if (end > data.length) throw new Error("Truncated ZCode bytes");
    const value = Buffer.from(data.subarray(offset.cursor, end)).toString("utf8");
    offset.cursor = end;
    return value;
  }
  if (type === 4) {
    const length = decodeVql(data, offset);
    const values = [];
    for (let i = 0; i < length; i += 1) values.push(decodeZcodeValue(data, offset));
    return values;
  }
  if (type === 5) {
    const length = decodeVql(data, offset);
    const end = offset.cursor + length;
    if (end > data.length) throw new Error("Truncated ZCode JSON");
    const value = JSON.parse(Buffer.from(data.subarray(offset.cursor, end)).toString("utf8"));
    offset.cursor = end;
    return value;
  }
  if (type === 6) return decodeVql(data, offset);
  throw new Error(`Unknown ZCode value type ${type}`);
}

function encodeRpcCall(id, channel, method, args) {
  const body = Buffer.from([
    ...encodeZcodeValue([100, id, channel, method]),
    ...encodeZcodeValue(args),
  ]);
  const frame = Buffer.alloc(HEADER_SIZE + body.length);
  frame.writeUInt8(REGULAR_MESSAGE, 0);
  frame.writeUInt32BE(body.length, 9);
  body.copy(frame, HEADER_SIZE);
  return frame;
}

// --- shared helpers ---------------------------------------------------------

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function readStderrHead(child, limit = 800) {
  let head = "";
  child.stderr.on("data", (chunk) => {
    if (head.length < limit) head += chunk.toString("utf8");
  });
  return () => head;
}

function extractSessionId(created) {
  return created?.session?.sessionId ?? created?.sessionId ?? created?.state?.session?.sessionId;
}

function extractAssistantText(state) {
  const messages = Array.isArray(state?.messages) ? state.messages : [];
  const found = [...messages]
    .reverse()
    .map((message) => ({
      role: message?.info?.role ?? message?.role,
      text: (Array.isArray(message?.parts) ? message.parts : [])
        .map((part) => (part?.type === "text" && typeof part.text === "string" ? part.text : ""))
        .join(""),
    }))
    .find((entry) => entry.text && (!entry.role || entry.role === "assistant"));
  return found ? found.text : null;
}

// Mirror what the ZCode desktop would answer so a turn completes unattended.
function answerReverseRequest(method, request) {
  const lowered = String(method || "").toLowerCase();
  if (method === "session/requestRuntimePreferences") {
    return {
      additionalDirectories: [],
      nativeSearchEnhancementsEnabled: false,
      toolSearchMode: "off",
      memoryEnabled: false,
      askUserQuestionAutoResolutionEnabled: true,
      modelContextBudgetStrategy: "preflight-v1",
    };
  }
  if (method.includes("providerRuntimeHeaders")) {
    return request.jwt
      ? { headersApplied: true, requestAuth: { apiKey: request.jwt, headers: {} } }
      : { headersApplied: false, errorMessage: "No ZCode JWT available on this connection" };
  }
  if (lowered.includes("permission")) {
    const decision = request.allowTools ? "allow" : "deny";
    return { decision, behavior: decision };
  }
  if (lowered.includes("officialmcpauth")) return { headers: {} };
  if (lowered.includes("userinput") || lowered.includes("interaction")) {
    return { action: "decline" };
  }
  return {};
}

// --- server-dist mode --------------------------------------------------------

function runServerMode(request) {
  return new Promise((resolve, reject) => {
    const timeouts = request.timeouts;
    const child = spawn(request.command, request.args, {
      cwd: process.cwd(),
      env: { ...process.env, ...(request.env || {}) },
      stdio: ["pipe", "pipe", "pipe"],
      shell: false,
      windowsHide: true,
    });
    const stderrHead = readStderrHead(child);
    let buffer = Buffer.alloc(0);
    let handshaken = false;
    let turnStarted = false;
    let nextId = 1;
    let pending = null; // { id, resolve, reject, timer }
    let settled = false;

    const fail = (error) => {
      if (settled) return;
      settled = true;
      if (pending) clearTimeout(pending.timer);
      try { child.kill(); } catch {}
      reject(error instanceof Error ? error : new Error(String(error)));
    };
    const failWithStderr = (message) => {
      const detail = stderrHead().trim().split("\n").slice(-3).join(" | ").slice(0, 400);
      fail(new Error(detail ? `${message} — ${detail}` : message));
    };

    const startupTimer = setTimeout(
      () => failWithStderr("ZCode app-server handshake timed out"),
      timeouts.startupMs,
    );

    const respond = (payload) => {
      if (settled) return;
      settled = true;
      clearTimeout(startupTimer);
      if (pending) clearTimeout(pending.timer);
      try { child.kill(); } catch {}
      resolve(payload);
    };

    const handleFrame = (kind, body) => {
      if (!handshaken) return;
      if (kind !== REGULAR_MESSAGE) return;
      const offset = { cursor: 0 };
      let header;
      let payload;
      try {
        header = decodeZcodeValue(body, offset);
        payload = decodeZcodeValue(body, offset);
      } catch (error) {
        fail(error);
        return;
      }
      const type = Array.isArray(header) ? header[0] : undefined;
      if (type === INITIALIZE_MESSAGE) {
        // Server announces readiness — mirror the JS client's start() gate.
        clearTimeout(startupTimer);
        runTurn().catch(fail);
        return;
      }
      if (type === RESPONSE_MESSAGE || type === ERROR_MESSAGE || type === CANCELED_MESSAGE) {
        const requestId = header[1];
        if (pending && requestId === pending.id) {
          clearTimeout(pending.timer);
          const waiter = pending;
          pending = null;
          if (type === RESPONSE_MESSAGE) waiter.resolve(payload);
          else waiter.reject(new Error(payload?.message || "ZCode RPC request failed"));
        }
      }
    };

    child.stdout.on("data", (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      if (!handshaken) {
        const newline = buffer.indexOf(0x0a);
        if (newline < 0) return;
        const line = buffer.subarray(0, newline).toString("utf8").trim();
        buffer = buffer.subarray(newline + 1);
        let hello;
        try {
          hello = JSON.parse(line);
        } catch {
          fail(new Error("Invalid ZCode app-server hello"));
          return;
        }
        if (!hello || hello.type !== "zcode-hello") {
          fail(new Error("Unexpected ZCode app-server hello"));
          return;
        }
        child.stdin.write(
          JSON.stringify({
            type: "zcode-hello-ack",
            version: "gateforge",
            clientId: "gateforge-" + process.pid,
          }) + "\n",
        );
        handshaken = true;
        // runTurn starts on the INITIALIZE frame (see handleFrame) — the same
        // readiness signal the JS client waits for in start().
      }
      while (handshaken && buffer.length >= HEADER_SIZE) {
        const length = buffer.readUInt32BE(9);
        if (length > MAX_FRAME_BYTES) {
          fail(new Error("ZCode frame exceeds the safety limit"));
          return;
        }
        if (buffer.length < HEADER_SIZE + length) return;
        const frameBody = Buffer.from(buffer.subarray(HEADER_SIZE, HEADER_SIZE + length));
        buffer = buffer.subarray(HEADER_SIZE + length);
        handleFrame(REGULAR_MESSAGE, frameBody);
      }
    });

    child.stderr.on("data", () => {}); // head captured by readStderrHead
    child.on("error", (error) => fail(error));
    child.on("exit", (code, signal) => {
      const reason = code !== null && code !== undefined ? String(code) : String(signal ?? "unknown");
      failWithStderr("ZCode app-server exited: " + reason);
    });

    async function call(channel, method, args, timeoutMs) {
      const id = nextId++;
      return new Promise((res, rej) => {
        const timer = setTimeout(() => {
          if (pending?.id === id) pending = null;
          rej(new Error(`ZCode RPC request timed out: ${channel}.${method}`));
        }, timeoutMs ?? timeouts.rpcMs);
        pending = { id, resolve: res, reject: rej, timer };
        try {
          child.stdin.write(encodeRpcCall(id, channel, method, args));
        } catch (error) {
          clearTimeout(timer);
          pending = null;
          rej(error instanceof Error ? error : new Error(String(error)));
        }
      });
    }

    async function runTurn() {
      if (turnStarted) return;
      turnStarted = true;
      const cwd = request.workspacePath;
      const workspace = { workspacePath: cwd, workspaceIdentity: cwd, workspaceKey: cwd };
      const initialized = await call("zcode-agent", "initialize", [workspace]);
      if (initialized?.available !== true) {
        throw new Error("ZCode server dist is not available: " + JSON.stringify(initialized).slice(0, 200));
      }
      const created = await call("zcode-agent", "createSession", [{
        ...workspace,
        sessionTraceId: randomUUID(),
        mode: "build",
        persistence: "immediate",
      }]);
      const sessionId = extractSessionId(created);
      if (!sessionId) throw new Error("ZCode server createSession returned no sessionId");

      const upstream = request.modelId === "glm-5.3"
        ? { modelId: "GLM-5.3", reasoningLevel: "max" }
        : { modelId: "GLM-5.3-Flash", reasoningLevel: "high" };
      try {
        await call("zcode-agent", "setModel", [{
          ...workspace,
          sessionId,
          model: {
            providerId: request.providerId,
            modelId: upstream.modelId,
            options: { reasoningLevel: upstream.reasoningLevel },
          },
        }]);
      } catch {
        // the worker's configured default selection still resolves
      }

      await call("zcode-agent", "sendPrompt", [{
        ...workspace,
        sessionId,
        inputId: randomUUID(),
        content: request.prompt,
      }]);

      const deadline = Date.now() + timeouts.turnMs;
      while (Date.now() <= deadline) {
        await sleep(timeouts.pollMs);
        let state;
        try {
          state = await call("zcode-agent", "readSession", [{
            ...workspace,
            sessionId,
            messageLimit: 200,
          }]);
        } catch {
          continue; // transient read failures happen while the worker is busy
        }
        const text = extractAssistantText(state);
        if (text) {
          respond({ ok: true, text });
          return;
        }
        const status = state?.session?.status ?? state?.status;
        if (status === "error") {
          const detail = state?.projection?.lastError?.message ?? state?.error?.message ?? "";
          throw new Error("ZCode turn ended in an error state" + (detail ? `: ${detail}` : ""));
        }
      }
      throw new Error(`ZCode turn timed out after ${Math.round(timeouts.turnMs / 1000)}s`);
    }
  });
}

// --- desktop CLI app-server mode (NDJSON fallback) ---------------------------

function runAppServerMode(request) {
  return new Promise((resolve, reject) => {
    const timeouts = request.timeouts;
    const child = spawn(request.command, request.args, {
      cwd: process.cwd(),
      env: { ...process.env, ...(request.env || {}) },
      stdio: ["pipe", "pipe", "pipe"],
      shell: false,
      windowsHide: true,
    });
    const stderrHead = readStderrHead(child);
    let lineBuffer = "";
    let nextId = 1;
    let pending = new Map();
    let settled = false;

    const fail = (error) => {
      if (settled) return;
      settled = true;
      try { child.kill(); } catch {}
      reject(error instanceof Error ? error : new Error(String(error)));
    };
    const failWithStderr = (message) => {
      const detail = stderrHead().trim().split("\n").slice(-3).join(" | ").slice(0, 400);
      fail(new Error(detail ? `${message} — ${detail}` : message));
    };
    const respond = (payload) => {
      if (settled) return;
      settled = true;
      try { child.kill(); } catch {}
      resolve(payload);
    };

    const startupTimer = setTimeout(
      () => failWithStderr("ZCode app-server handshake timed out"),
      timeouts.startupMs,
    );

    child.stdout.on("data", (chunk) => {
      lineBuffer += chunk.toString("utf8");
      let newline;
      while ((newline = lineBuffer.indexOf("\n")) >= 0) {
        const line = lineBuffer.slice(0, newline).trim();
        lineBuffer = lineBuffer.slice(newline + 1);
        if (!line) continue;
        let message;
        try {
          message = JSON.parse(line);
        } catch {
          continue; // not protocol traffic
        }
        handleMessage(message);
      }
    });
    child.on("error", (error) => fail(error));
    child.on("exit", (code, signal) => {
      const reason = code !== null && code !== undefined ? String(code) : String(signal ?? "unknown");
      failWithStderr("ZCode app-server exited: " + reason);
    });

    function write(obj) {
      try {
        child.stdin.write(JSON.stringify(obj) + "\n");
      } catch {
        // exit handler rejects pending calls
      }
    }

    function call(method, params, timeoutMs) {
      const id = nextId++;
      return new Promise((res, rej) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          rej(new Error(`ZCode RPC timed out: ${method}`));
        }, timeoutMs ?? timeouts.rpcMs);
        pending.set(id, { resolve: res, reject: rej, timer });
        write({ id, method, params });
      });
    }

    function handleMessage(message) {
      if (message.id !== undefined && !message.method && ("result" in message || "error" in message)) {
        const waiter = pending.get(message.id);
        if (!waiter) return;
        pending.delete(message.id);
        clearTimeout(waiter.timer);
        if (message.error) waiter.reject(new Error(message.error.message || "ZCode RPC failed"));
        else waiter.resolve(message.result);
        return;
      }
      if (message.id !== undefined && message.method) {
        // Reverse request from the agent — the host side must answer.
        Promise.resolve(answerReverseRequest(message.method, request))
          .then((result) => write({ id: message.id, result: result ?? {} }))
          .catch((error) =>
            write({ id: message.id, error: { code: -32000, message: error?.message || "host error" } }));
        return;
      }
      if (message.method === "startup/storageState") {
        if (message.params?.phase === "ready") {
          clearTimeout(startupTimer);
          runTurn().catch(fail);
        } else if (message.params?.phase === "failed") {
          fail(new Error("ZCode app-server storage startup failed"));
        }
      }
    }

    async function runTurn() {
      const cwd = request.workspacePath;
      const workspace = { workspacePath: cwd, workspaceIdentity: cwd, workspaceKey: cwd };
      const created = await call("session/create", {
        workspace,
        mode: "build",
        titleGenerationEnabled: false,
      });
      const sessionId = extractSessionId(created);
      if (!sessionId) throw new Error("ZCode session/create returned no sessionId");

      if (request.requireStartPlan) {
        const current = await call("session/read", { sessionId, messageLimit: 1 }, 30_000);
        const currentModel = current?.settings?.model?.current;
        if (
          currentModel?.providerId &&
          !String(currentModel.providerId).includes("start-plan")
        ) {
          throw new Error(
            "ZCode app is not on Start Plan (current: " +
              `${currentModel.providerId}/${currentModel.modelId}). ` +
              "Select GLM-5.3 or GLM-5.3-Flash in the ZCode app, then retry " +
              "(or set ZCODE_REQUIRE_START_PLAN=false).",
          );
        }
      }

      await call("session/send", {
        sessionId,
        content: request.prompt,
        inputId: `gateforge-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      });

      const deadline = Date.now() + timeouts.turnMs;
      while (Date.now() <= deadline) {
        await sleep(timeouts.pollMs);
        let state;
        try {
          state = await call("session/read", { sessionId, messageLimit: 200 }, 30_000);
        } catch {
          continue;
        }
        const text = extractAssistantText(state);
        if (text) {
          respond({ ok: true, text });
          return;
        }
        const status = state?.session?.status ?? state?.status;
        if (status === "error") {
          const detail =
            state?.projection?.lastError?.message ??
            state?.session?.error?.message ??
            state?.error?.message ?? "";
          throw new Error("ZCode turn ended in an error state" + (detail ? `: ${detail}` : ""));
        }
      }
      throw new Error(`ZCode turn timed out after ${Math.round(timeouts.turnMs / 1000)}s`);
    }
  });
}

// --- entry -------------------------------------------------------------------

function main() {
  let raw = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", (chunk) => {
    raw += chunk;
    if (raw.includes("\n")) {
      process.stdin.removeAllListeners("data");
      const request = JSON.parse(raw.slice(0, raw.indexOf("\n")));
      execute(request)
        .then((result) => {
          process.stdout.write(JSON.stringify(result) + "\n");
          process.exit(0);
        })
        .catch((error) => {
          process.stdout.write(
            JSON.stringify({ ok: false, error: error?.message || String(error) }) + "\n",
          );
          process.exit(0);
        });
    }
  });
}

async function execute(request) {
  if (request.mode === "server") return runServerMode(request);
  return runAppServerMode(request);
}

main();
