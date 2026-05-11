import "dotenv/config";
import path from "node:path";
import { spawn } from "node:child_process";

// Heartbeat: every INTERVAL_MS, spawn one `claude -p` invocation pointed at
// HEARTBEAT.md (at the repo root). This process has no business logic — it
// is a pure scheduler. Claude reads HEARTBEAT.md fresh on every tick and
// decides what to do.
//
// To extend the agent, edit HEARTBEAT.md. No code change required here.

const INTERVAL_MS = Number(process.env.HEARTBEAT_INTERVAL_MS ?? 60_000);
const TICK_TIMEOUT_MS = Number(process.env.HEARTBEAT_TIMEOUT_MS ?? 5 * 60 * 1000);
const HEARTBEAT_MODEL = process.env.HEARTBEAT_MODEL ?? "claude-haiku-4-5-20251001";

// Spawned `claude` runs from the repo root so it picks up .mcp.json + skills.
const PROJECT_ROOT = path.resolve(import.meta.dirname, "../../../..");

const TICK_PROMPT = [
  "Heartbeat tick.",
  "",
  "Read HEARTBEAT.md (at the repo root) and follow it. Re-read every tick — the file is the source of truth and may change between ticks.",
  "",
  "Reply with a short, factual summary of what you did (or 'nothing to do' if all preconditions were false).",
].join("\n");

let tickCounter = 0;

function ts(): string {
  return new Date().toISOString();
}

function fmtMs(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function log(...parts: unknown[]): void {
  console.log(`[${ts()}]`, "[heartbeat]", ...parts);
}

function logErr(...parts: unknown[]): void {
  console.error(`[${ts()}]`, "[heartbeat]", ...parts);
}

function logWarn(...parts: unknown[]): void {
  console.warn(`[${ts()}]`, "[heartbeat]", ...parts);
}

function indent(text: string, prefix = "  | "): string {
  return text
    .split("\n")
    .map((line) => `${prefix}${line}`)
    .join("\n");
}

async function spawnClaude(model: string, prompt: string, timeoutMs: number): Promise<string> {
  log(`spawning claude (model=${model}, timeout=${fmtMs(timeoutMs)}, cwd=${PROJECT_ROOT})`);
  const startedAt = Date.now();

  return new Promise<string>((resolve, reject) => {
    // stdio[0]='ignore' attaches /dev/null to claude's stdin — without it,
    // `claude -p` waits 3s for possible piped data before proceeding.
    //
    // --permission-mode bypassPermissions: the heartbeat is headless, so the
    // user can never approve a permission prompt. The agent's blast radius is
    // narrow (project MCP tools + project sqlite + storage/ cleanup), so we
    // bypass and rely on the .mcp.json + .claude/settings.json allowlists as
    // the operative boundary.
    const args = [
      "-p",
      prompt,
      "--model",
      model,
      "--permission-mode",
      "bypassPermissions",
    ];
    const child = spawn("claude", args, {
      cwd: PROJECT_ROOT,
      stdio: ["ignore", "pipe", "pipe"],
    });

    const stdoutChunks: Buffer[] = [];
    const stderrChunks: Buffer[] = [];
    child.stdout.on("data", (c: Buffer) => stdoutChunks.push(c));
    child.stderr.on("data", (c: Buffer) => stderrChunks.push(c));

    const timer = setTimeout(() => {
      logWarn(`timing out after ${fmtMs(timeoutMs)}, sending SIGTERM`);
      child.kill("SIGTERM");
      reject(new Error(`claude(${model}) timed out after ${timeoutMs}ms`));
    }, timeoutMs);

    child.on("error", (err) => {
      clearTimeout(timer);
      reject(err);
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      const elapsed = Date.now() - startedAt;
      const stdout = Buffer.concat(stdoutChunks).toString("utf-8");
      const stderr = Buffer.concat(stderrChunks).toString("utf-8");
      if (stderr.trim()) {
        logWarn(`stderr:\n${indent(stderr.trim())}`);
      }
      if (code !== 0) {
        logErr(`claude exited code=${code} after ${fmtMs(elapsed)}`);
        reject(new Error(`claude(${model}) exited ${code}: ${stderr || stdout}`));
        return;
      }
      log(`claude completed in ${fmtMs(elapsed)} (${stdout.length} bytes stdout)`);
      resolve(stdout);
    });
  });
}

async function tick(): Promise<void> {
  tickCounter++;
  const tickStart = Date.now();
  log(`tick #${tickCounter} starting`);

  try {
    const reply = await spawnClaude(HEARTBEAT_MODEL, TICK_PROMPT, TICK_TIMEOUT_MS);
    const trimmed = reply.trim();
    log(`tick #${tickCounter} reply (${trimmed.length} chars):\n${indent(trimmed || "(empty)")}`);
  } catch (err) {
    logErr(`tick #${tickCounter} failed:`, err);
  }
  log(`tick #${tickCounter} done in ${fmtMs(Date.now() - tickStart)}`);
}

async function main(): Promise<void> {
  log("=".repeat(60));
  log(`heartbeat starting`);
  log(`  interval = ${INTERVAL_MS / 1000}s`);
  log(`  model    = ${HEARTBEAT_MODEL} (per-tick timeout ${fmtMs(TICK_TIMEOUT_MS)})`);
  log(`  cwd      = ${PROJECT_ROOT}`);
  log(`  reads    = HEARTBEAT.md (every tick, fresh)`);
  log("=".repeat(60));

  let stopping = false;
  const stop = (sig: string): void => {
    if (stopping) return;
    stopping = true;
    log(`received ${sig} — stopping after current tick`);
  };
  process.on("SIGINT", () => stop("SIGINT"));
  process.on("SIGTERM", () => stop("SIGTERM"));

  while (!stopping) {
    const startedAt = Date.now();
    try {
      await tick();
    } catch (err) {
      logErr("tick crashed:", err);
    }
    if (stopping) break;
    const wait = Math.max(0, INTERVAL_MS - (Date.now() - startedAt));
    log(`next tick in ${fmtMs(wait)}`);
    await new Promise((resolve) => setTimeout(resolve, wait));
  }
  log("heartbeat stopped");
}

main().catch((err: unknown) => {
  logErr("fatal:", err);
  process.exit(1);
});
