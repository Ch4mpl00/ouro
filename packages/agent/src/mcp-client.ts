import path from "node:path";
import { UnauthorizedError } from "@modelcontextprotocol/sdk/client/auth.js";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import {
  StreamableHTTPClientTransport,
  StreamableHTTPError,
} from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import type { ChatCompletionTool } from "openai/resources/chat/completions";

// Connects to the MCP server. Two transports:
//   - stdio (default, MCP_TRANSPORT=stdio): spawns packages/mcp as a child
//     process — used for local dev and Claude Code's .mcp.json
//   - http (MCP_TRANSPORT=http, MCP_URL=...): connects to a remote MCP server
//     over Streamable HTTP — used for containerized deployment where the
//     MCP server runs in its own container with its own filesystem isolation
//
// Either way the agent never touches the MCP server's filesystem directly.

const PROJECT_ROOT = path.resolve(import.meta.dirname, "../../..");

export interface McpHandle {
  tools: ChatCompletionTool[];
  callTool(name: string, args: Record<string, unknown>): Promise<string>;
  close(): Promise<void>;
}

// ---------------------------------------------------------------------------
// Connect retry
//
// The agent's startup used to be a single unprotected `connect()`: any failure
// there propagated to `main().catch` → `process.exit(1)` → Docker's
// `restart: unless-stopped` → another connect. That container-level retry is
// what turned a transient MCP hiccup into the production crash-loops of
// 2026-06-15 and 2026-08-23 (78 restarts). Retrying *inside* the process
// instead keeps the backoff bounded, capped and visible in the logs.
// ---------------------------------------------------------------------------

export interface ConnectRetryPolicy {
  // Retries AFTER the first attempt. `null` = never give up.
  maxRetries: number | null;
  baseDelayMs: number;
  // Ceiling for the exponential backoff, so an hours-long MCP outage settles
  // into a steady poll instead of drifting into multi-hour sleeps.
  maxDelayMs: number;
}

// Supervisor startup: never give up. Giving up after N attempts only moves the
// retry back into Docker — same loop, no backoff we control, no log line
// explaining the wait, and a restart counter that makes the agent look broken
// when it is merely waiting. A capped 30s poll is strictly better.
export const RETRY_UNTIL_UP: ConnectRetryPolicy = {
  maxRetries: null,
  baseDelayMs: 1_000,
  maxDelayMs: 30_000,
};

// Everything else: one-shot scripts, benchmarks, and the mid-flight reconnect
// inside callTool. Bounded, because a tool call that hangs forever is worse
// than one that fails — the workflow executor has its own recovery path and
// the poll loop comes back in 2s.
export const CONNECT_RETRY: ConnectRetryPolicy = {
  maxRetries: 4,
  baseDelayMs: 500,
  maxDelayMs: 8_000,
};

export interface ConnectRetryInfo {
  // 1-based number of the attempt that just failed.
  attempt: number;
  delayMs: number;
  error: unknown;
}

export interface ConnectRetryDeps {
  sleep?: (ms: number) => Promise<void>;
  onRetry?: (info: ConnectRetryInfo) => void;
}

// Retrying a deterministic misconfiguration forever is worse than crashing on
// it: the container stays "up" and nobody looks at the logs. A 4xx from the
// MCP endpoint means the server IS up and is rejecting us on the merits —
// wrong path, wrong auth, protocol mismatch — so it stays fatal. 408 and 429
// are the two 4xx that literally mean "come back later".
//
// Everything else is treated as transient: connection refused / DNS / socket
// reset (no HTTP status at all), and every 5xx. HTTP 500 in particular is the
// exact shape of the incident — "Already connected to a transport" from an MCP
// that still holds the dead session of the agent we just replaced.
export function isFatalConnectError(err: unknown): boolean {
  if (err instanceof UnauthorizedError) return true;
  if (err instanceof StreamableHTTPError) {
    const status = err.code;
    if (typeof status !== "number") return false;
    if (status === 408 || status === 429) return false;
    return status >= 400 && status < 500;
  }
  return false;
}

export async function connectWithRetry<T>(
  open: () => Promise<T>,
  policy: ConnectRetryPolicy,
  deps: ConnectRetryDeps = {},
): Promise<T> {
  const sleep = deps.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
  for (let attempt = 1; ; attempt++) {
    try {
      return await open();
    } catch (err) {
      const exhausted = policy.maxRetries !== null && attempt > policy.maxRetries;
      if (exhausted || isFatalConnectError(err)) throw err;
      const delayMs = Math.min(policy.baseDelayMs * 2 ** (attempt - 1), policy.maxDelayMs);
      deps.onRetry?.({ attempt, delayMs, error: err });
      await sleep(delayMs);
    }
  }
}

function describeError(err: unknown): string {
  if (err instanceof StreamableHTTPError) return `http ${err.code}: ${err.message}`;
  return err instanceof Error ? err.message : String(err);
}

async function closeQuietly(client: Client): Promise<void> {
  try {
    await client.close();
  } catch {
    /* already dead — nothing to salvage */
  }
}

// A fresh transport per attempt: StreamableHTTPClientTransport.start() throws
// "already started" if one is reused, and a stdio transport that failed to
// spawn is not restartable either.
export type TransportFactory = () => Transport;

function transportFactoryFromEnv(): TransportFactory {
  const mode = (process.env.MCP_TRANSPORT ?? "stdio").toLowerCase();
  if (mode === "http") {
    // Parsed once, up front and OUTSIDE the retry loop: a malformed MCP_URL is
    // a misconfiguration, and retrying one forever would hide it behind a
    // container that looks alive.
    const url = new URL(process.env.MCP_URL ?? "http://localhost:3000/mcp");
    return () => new StreamableHTTPClientTransport(url);
  }
  return () =>
    new StdioClientTransport({
      command: "pnpm",
      args: ["mcp:serve"],
      cwd: PROJECT_ROOT,
    });
}

// MCP sessions live in-memory on the server side. Any time the MCP container
// restarts (rebuild, crash, healthcheck), the agent's cached session-id is
// dead and every call comes back with "No valid session id; send an
// initialize request first." We detect that, drop the dead client, and
// transparently reconnect once before retrying the call.
function isSessionLostError(err: unknown): boolean {
  const msg = err instanceof Error ? err.message : String(err);
  return msg.includes("No valid session id") || msg.includes("Session not found");
}

export interface ConnectMcpOptions {
  // Policy for the INITIAL connect. Defaults to the bounded `CONNECT_RETRY`;
  // the supervisor passes `RETRY_UNTIL_UP` because for a long-running process
  // "wait" beats "exit 1 and let Docker do it".
  startupRetry?: ConnectRetryPolicy;
  // Overridable so tests can drive the retry paths without a socket. Defaults
  // to the env-configured stdio / Streamable-HTTP transport.
  createTransport?: TransportFactory;
}

export async function connectMcp(options: ConnectMcpOptions = {}): Promise<McpHandle> {
  const startupRetry = options.startupRetry ?? CONNECT_RETRY;
  const createTransport = options.createTransport ?? transportFactoryFromEnv();

  const logRetry =
    (what: string) =>
    ({ attempt, delayMs, error }: ConnectRetryInfo): void => {
      console.warn(
        `[mcp-client] ${what} attempt ${attempt} failed (${describeError(error)}), retrying in ${delayMs}ms`,
      );
    };

  async function openClient(): Promise<Client> {
    const fresh = new Client({ name: "agent-loop", version: "0.1.0" }, { capabilities: {} });
    try {
      await fresh.connect(createTransport());
    } catch (err) {
      // connect() closes the client itself when `initialize` fails, but not
      // when the transport never started. Close defensively so a retried
      // attempt doesn't leave a socket or a child process behind.
      await closeQuietly(fresh);
      throw err;
    }
    return fresh;
  }

  // Tool schemas are stable across MCP restarts (we deploy mcp+agent in
  // lockstep), so we list once at boot and reuse. If we ever change that,
  // re-list inside the reconnect path.
  //
  // Listing is inside the retried unit because a session that can't list tools
  // is not a usable session — succeeding here and failing on listTools would
  // put us right back on the exit-1 path.
  const opened = await connectWithRetry(
    async () => {
      const fresh = await openClient();
      try {
        return { client: fresh, mcpTools: (await fresh.listTools()).tools };
      } catch (err) {
        await closeQuietly(fresh);
        throw err;
      }
    },
    startupRetry,
    { onRetry: logRetry("connect") },
  );

  let client = opened.client;
  // Single in-flight reconnect, shared by concurrent callers. The AgentLoop
  // dispatches tool calls in parallel — when the MCP session dies, several
  // calls fail at once, and without this each would open its own client
  // (last write wins, the rest leak unclosed).
  let reconnecting: Promise<Client> | null = null;

  function reconnect(failed: Client): Promise<Client> {
    // Someone already swapped the client out — just use the fresh one.
    if (failed !== client) return Promise.resolve(client);
    reconnecting ??= (async () => {
      await closeQuietly(failed);
      // Bounded, unlike startup: this runs inside a tool call the workflow
      // executor is waiting on. If MCP is down for longer than this, failing
      // the call is the honest answer — the poll loop retries in 2s and the
      // next reconnect gets a fresh budget.
      client = await connectWithRetry(openClient, CONNECT_RETRY, {
        onRetry: logRetry("reconnect"),
      });
      return client;
    })().finally(() => {
      reconnecting = null;
    });
    return reconnecting;
  }

  const tools: ChatCompletionTool[] = opened.mcpTools.map((t) => ({
    type: "function",
    function: {
      name: t.name,
      description: t.description ?? "",
      parameters: t.inputSchema,
    },
  }));

  async function callTool(
    name: string,
    args: Record<string, unknown>,
  ): Promise<string> {
    const used = client;
    let result;
    try {
      result = await used.callTool({ name, arguments: args });
    } catch (err) {
      if (!isSessionLostError(err)) throw err;
      console.warn("[mcp-client] session lost, reconnecting…");
      const fresh = await reconnect(used);
      result = await fresh.callTool({ name, arguments: args });
    }

    const parts = Array.isArray(result.content) ? result.content : [];
    const text = parts
      .filter((p): p is { type: "text"; text: string } => p.type === "text")
      .map((p) => p.text)
      .join("\n");

    if (result.isError) {
      return `[tool error] ${text || "(no error text)"}`;
    }
    return text || "(empty)";
  }

  async function close(): Promise<void> {
    await client.close();
  }

  return { tools, callTool, close };
}
