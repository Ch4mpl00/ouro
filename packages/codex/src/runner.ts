import { randomUUID } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";

export interface CodexRunRequest {
  prompt: string;
  input?: unknown;
  schema?: unknown;
  sandbox?: "read-only" | "workspace-write" | "danger-full-access";
  approvalPolicy?: "untrusted" | "on-request" | "never";
  cwd?: string;
  model?: string;
  timeoutMs?: number;
  includeEvents?: boolean;
  config?: Record<string, unknown>;
}

export interface CodexRunResult {
  ok: true;
  content: string;
  parsed?: unknown;
  usage?: unknown;
  threadId?: string;
  events?: unknown[];
  stderr: string;
}

export interface CodexRunFailure {
  ok: false;
  error: string;
  code: number | null;
  signal: NodeJS.Signals | null;
  stdout: string;
  stderr: string;
  events?: unknown[];
}

function parseInput(input: unknown): string | undefined {
  if (input === undefined) return undefined;
  return typeof input === "string" ? input : JSON.stringify(input, null, 2);
}

function tomlValue(value: unknown): string {
  if (typeof value === "string") return JSON.stringify(value);
  if (typeof value === "boolean") return value ? "true" : "false";
  if (typeof value === "number" && Number.isFinite(value)) return String(value);
  if (Array.isArray(value)) return `[${value.map(tomlValue).join(", ")}]`;
  if (value && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>)
      .map(([k, v]) => `${JSON.stringify(k)} = ${tomlValue(v)}`);
    return `{ ${entries.join(", ")} }`;
  }
  if (value === null) return '""';
  return JSON.stringify(String(value));
}

function parseJsonl(stdout: string): {
  events: unknown[];
  content: string;
  usage?: unknown;
  threadId?: string;
  errorMessage?: string;
} {
  const events: unknown[] = [];
  let content = "";
  let usage: unknown;
  let threadId: string | undefined;
  let errorMessage: string | undefined;
  for (const line of stdout.split(/\r?\n/)) {
    if (!line.trim()) continue;
    try {
      const event = JSON.parse(line) as Record<string, unknown>;
      events.push(event);
      if (event.type === "thread.started" && typeof event.thread_id === "string") {
        threadId = event.thread_id;
      }
      if (event.type === "item.completed") {
        const item = event.item as Record<string, unknown> | undefined;
        if (item?.type === "agent_message" && typeof item.text === "string") {
          content = item.text;
        }
      }
      if (event.type === "turn.completed") usage = event.usage;
      // The actual failure reason (usage limit, auth, model error) arrives as
      // a JSONL error event on STDOUT — stderr carries only progress notices.
      if (event.type === "error" && typeof event.message === "string") {
        errorMessage = event.message;
      }
    } catch {
      // Codex --json should be JSONL. Keep non-JSON lines out of events and
      // fall back to stdout as content below if no agent_message arrives.
    }
  }
  return { events, content: content || stdout.trim(), usage, threadId, errorMessage };
}

function parseStructuredContent(content: string): unknown | undefined {
  const trimmed = content.trim();
  if (!trimmed) return undefined;
  try {
    return JSON.parse(trimmed);
  } catch {
    return undefined;
  }
}

export async function runCodex(req: CodexRunRequest): Promise<CodexRunResult | CodexRunFailure> {
  const workDir = req.cwd ?? process.cwd();
  const timeoutMs = req.timeoutMs ?? 10 * 60_000;
  const tmp = await mkdtemp(path.join(tmpdir(), "codex-run-"));
  let schemaPath: string | null = null;
  try {
    const args = [
      "exec",
      "--ephemeral",
      "--json",
      "--skip-git-repo-check",
      // Host-independent behavior: the host's personal config.toml (model,
      // reasoning effort, MCP servers) must not leak into service runs.
      // Auth still comes from CODEX_HOME; per-run knobs arrive via req.
      "--ignore-user-config",
      "--sandbox",
      req.sandbox ?? "read-only",
      // `codex exec` has no --ask-for-approval flag (it's interactive-only);
      // the equivalent knob is the approval_policy config key.
      "-c",
      `approval_policy=${JSON.stringify(req.approvalPolicy ?? "never")}`,
    ];
    if (req.model) args.push("--model", req.model);
    if (req.schema !== undefined) {
      schemaPath = path.join(tmp, `${randomUUID()}.schema.json`);
      await writeFile(schemaPath, JSON.stringify(req.schema, null, 2), "utf8");
      args.push("--output-schema", schemaPath);
    }
    for (const [key, value] of Object.entries(req.config ?? {})) {
      args.push("-c", `${key}=${tomlValue(value)}`);
    }
    args.push(req.prompt);

    const child = spawn("codex", args, {
      cwd: workDir,
      stdio: ["pipe", "pipe", "pipe"],
      env: process.env,
    });

    const input = parseInput(req.input);
    if (input !== undefined) child.stdin.end(input);
    else child.stdin.end();

    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });

    let timedOut = false;
    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill("SIGTERM");
    }, timeoutMs);
    const exit = await new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolve, reject) => {
      child.on("error", reject);
      child.on("close", (code, signal) => resolve({ code, signal }));
    });
    clearTimeout(timeout);

    const parsed = parseJsonl(stdout);
    if (exit.code !== 0) {
      const reason = timedOut
        ? `codex timed out after ${timeoutMs}ms (killed with ${exit.signal ?? "SIGTERM"})`
        : exit.signal
          ? `codex killed by ${exit.signal}`
          : `codex exited with code ${exit.code ?? "?"}`;
      const detail = parsed.errorMessage ?? (stderr.trim() || null);
      return {
        ok: false,
        error: detail ? `${reason}: ${detail}` : reason,
        code: exit.code,
        signal: exit.signal,
        stdout,
        stderr,
        events: req.includeEvents ? parsed.events : undefined,
      };
    }

    return {
      ok: true,
      content: parsed.content,
      parsed: parseStructuredContent(parsed.content),
      usage: parsed.usage,
      threadId: parsed.threadId,
      events: req.includeEvents ? parsed.events : undefined,
      stderr,
    };
  } finally {
    await rm(tmp, { recursive: true, force: true });
  }
}
