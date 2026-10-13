import http from "node:http";
import { runCodex, type CodexRunRequest } from "./runner";

// The Codex sandbox service: POST /run executes one `codex exec` and returns
// its result; GET /health. Reachable only on the compose network (`expose`).

export interface CodexServerOptions {
  maxBodyBytes?: number;
}

class BodyTooLarge extends Error {}

// Over the limit: stop keeping the body, let the rest drain, and answer 413 —
// destroying the request instead cut the caller off with no answer at all.
function readBody(req: http.IncomingMessage, maxBodyBytes: number): Promise<string> {
  return new Promise((resolve, reject) => {
    let size = 0;
    let body = "";
    let tooLarge = false;
    req.setEncoding("utf8");
    req.on("data", (chunk: string) => {
      if (tooLarge) return;
      size += Buffer.byteLength(chunk);
      if (size > maxBodyBytes) {
        tooLarge = true;
        body = "";
        reject(new BodyTooLarge(`request body exceeds ${maxBodyBytes} bytes`));
        return;
      }
      body += chunk;
    });
    req.on("end", () => resolve(body));
    req.on("error", reject);
  });
}

function send(res: http.ServerResponse, status: number, body: unknown): void {
  const text = JSON.stringify(body);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(text),
  });
  res.end(text);
}

// The sandboxes a caller may ask for. `danger-full-access` (no sandbox) is
// not on the list: no client needs it — code_agent runs workspace-write, the
// judge read-only — and anything else on the compose network that can reach
// this port must not be able to buy an unsandboxed shell with one field.
const SANDBOXES = ["read-only", "workspace-write"] as const;
const APPROVAL_POLICIES = ["untrusted", "on-request", "never"] as const;

function oneOf<T extends string>(allowed: readonly T[], value: unknown, field: string): T | undefined {
  if (value === undefined) return undefined;
  const hit = allowed.find((a) => a === value);
  if (hit === undefined) throw new Error(`${field} must be one of ${allowed.join(", ")}`);
  return hit;
}

function optionalString(value: unknown, field: string): string | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "string") throw new Error(`${field} must be a string`);
  return value;
}

export function validateRunRequest(input: unknown): CodexRunRequest {
  if (typeof input !== "object" || input === null || Array.isArray(input)) throw new Error("body must be an object");
  const field = (name: string): unknown => Reflect.get(input, name);
  const prompt = field("prompt");
  if (typeof prompt !== "string" || prompt.length === 0) throw new Error("prompt must be a non-empty string");
  const timeoutMs = field("timeoutMs");
  if (timeoutMs !== undefined && (typeof timeoutMs !== "number" || !Number.isFinite(timeoutMs) || timeoutMs <= 0)) {
    throw new Error("timeoutMs must be a positive number");
  }
  const includeEvents = field("includeEvents");
  if (includeEvents !== undefined && typeof includeEvents !== "boolean") throw new Error("includeEvents must be a boolean");
  const config = field("config");
  if (config !== undefined && (typeof config !== "object" || config === null || Array.isArray(config))) {
    throw new Error("config must be an object");
  }
  return {
    prompt,
    input: field("input"),
    schema: field("schema"),
    sandbox: oneOf(SANDBOXES, field("sandbox"), "sandbox"),
    approvalPolicy: oneOf(APPROVAL_POLICIES, field("approvalPolicy"), "approvalPolicy"),
    cwd: optionalString(field("cwd"), "cwd"),
    model: optionalString(field("model"), "model"),
    timeoutMs,
    includeEvents,
    config: config === undefined ? undefined : Object.fromEntries(Object.entries(config)),
  };
}

export function createCodexServer(options: CodexServerOptions = {}): http.Server {
  const maxBodyBytes = options.maxBodyBytes ?? 10 * 1024 * 1024;
  return http.createServer(async (req, res) => {
    try {
      if (req.method === "GET" && req.url === "/health") {
        send(res, 200, { ok: true });
        return;
      }

      if (req.method !== "POST" || req.url !== "/run") {
        send(res, 404, { ok: false, error: "not found" });
        return;
      }

      const raw = await readBody(req, maxBodyBytes);
      const parsed: unknown = raw ? JSON.parse(raw) : {};
      const runReq = validateRunRequest(parsed);
      const result = await runCodex(runReq);
      send(res, result.ok ? 200 : 502, result);
    } catch (err) {
      if (!res.headersSent && !res.destroyed) {
        send(res, err instanceof BodyTooLarge ? 413 : 400, { ok: false, error: err instanceof Error ? err.message : String(err) });
      }
    }
  });
}
