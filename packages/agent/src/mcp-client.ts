import path from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
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

function buildTransport(): Transport {
  const mode = (process.env.MCP_TRANSPORT ?? "stdio").toLowerCase();
  if (mode === "http") {
    const url = process.env.MCP_URL ?? "http://localhost:3000/mcp";
    return new StreamableHTTPClientTransport(new URL(url));
  }
  return new StdioClientTransport({
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

async function openClient(): Promise<Client> {
  const client = new Client(
    { name: "agent-loop", version: "0.1.0" },
    { capabilities: {} },
  );
  await client.connect(buildTransport());
  return client;
}

export async function connectMcp(): Promise<McpHandle> {
  let client = await openClient();
  // Single in-flight reconnect, shared by concurrent callers. The AgentLoop
  // dispatches tool calls in parallel — when the MCP session dies, several
  // calls fail at once, and without this each would open its own client
  // (last write wins, the rest leak unclosed).
  let reconnecting: Promise<Client> | null = null;

  function reconnect(failed: Client): Promise<Client> {
    // Someone already swapped the client out — just use the fresh one.
    if (failed !== client) return Promise.resolve(client);
    reconnecting ??= (async () => {
      try {
        await failed.close();
      } catch {
        /* dead session, close may itself fail — ignore */
      }
      client = await openClient();
      return client;
    })().finally(() => {
      reconnecting = null;
    });
    return reconnecting;
  }

  // Tool schemas are stable across MCP restarts (we deploy mcp+agent in
  // lockstep), so we list once at boot and reuse. If we ever change that,
  // re-list inside the reconnect path.
  const { tools: mcpTools } = await client.listTools();

  const tools: ChatCompletionTool[] = mcpTools.map((t) => ({
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
