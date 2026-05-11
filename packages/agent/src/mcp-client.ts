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

export async function connectMcp(): Promise<McpHandle> {
  const transport = buildTransport();

  const client = new Client(
    { name: "agent-loop", version: "0.1.0" },
    { capabilities: {} },
  );

  await client.connect(transport);

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
    const result = await client.callTool({ name, arguments: args });

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
