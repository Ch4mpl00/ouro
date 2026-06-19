import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
  type Tool,
} from "@modelcontextprotocol/sdk/types.js";
import {
  connectOwnClient,
  connectRemoteClient,
  type GatewayClient,
} from "./client";
import type { ResolvedUpstream } from "./config";

// The gateway is a low-level MCP Server that fronts the agent and is itself an
// MCP client to N sources: own-MCP (in-process) plus each third-party upstream
// (StreamableHTTP). It merges every source's tools into one tools/list — own
// tools verbatim, upstream tools namespaced as `${prefix}__${tool}` — and routes
// tools/call to the owning source by name. Tool schemas pass through untouched
// (no Zod round-trip), which is why it's a low-level Server, not an McpServer.
//
// The agent connects to this Server instead of own-MCP directly and sees one
// flat, namespaced tool list. Tool curation stays agent-side (skill frontmatter).

const TOOL_NS_SEPARATOR = "__";
// OpenAI function-calling name constraint — the agent feeds these straight to
// the model, so an over-long or illegal namespaced name must be dropped, not
// silently surfaced and then rejected mid-session.
const OPENAI_TOOL_NAME = /^[a-zA-Z0-9_-]{1,64}$/;

const DEFAULT_CALL_TIMEOUT_MS = 30_000;
const DEFAULT_CONNECT_TIMEOUT_MS = 15_000;

export interface GatewayModule {
  server: Server;
  close(): Promise<void>;
}

export interface GatewayModuleDeps {
  ownServer: McpServer;
  upstreams: ResolvedUpstream[];
  callTimeoutMs?: number;
  connectTimeoutMs?: number;
}

interface Route {
  client: GatewayClient;
  // The tool's original name on the source (un-prefixed).
  originalName: string;
}

function withTimeout<T>(p: Promise<T>, ms: number, label: string): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
    p.then(
      (v) => {
        clearTimeout(timer);
        resolve(v);
      },
      (e) => {
        clearTimeout(timer);
        reject(e);
      },
    );
  });
}

export async function createGatewayModule(deps: GatewayModuleDeps): Promise<GatewayModule> {
  const callTimeoutMs = deps.callTimeoutMs ?? DEFAULT_CALL_TIMEOUT_MS;
  const connectTimeoutMs = deps.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS;

  const clients: GatewayClient[] = [];

  // own-MCP first — its tools are authoritative and win any name collision.
  clients.push(await connectOwnClient(deps.ownServer));

  // Connect upstreams concurrently; a failure (unreachable / handshake error /
  // timeout) is logged and the upstream skipped, never fatal.
  const connected = await Promise.all(
    deps.upstreams.map(async (u) => {
      try {
        const client = await withTimeout(
          connectRemoteClient({
            id: u.name,
            prefix: u.prefix,
            url: u.url,
            headers: u.headers,
            callTimeoutMs,
          }),
          connectTimeoutMs,
          `upstream '${u.name}' connect`,
        );
        return client;
      } catch (err) {
        console.warn(
          `[gateway] upstream '${u.name}' unavailable, skipping: ${err instanceof Error ? err.message : String(err)}`,
        );
        return null;
      }
    }),
  );
  for (const c of connected) if (c) clients.push(c);

  // Build the merged tool list + routing map once at boot (the agent lists tools
  // once and we deploy in lockstep). own tools un-prefixed; upstream tools
  // namespaced. Skip — with a log — any tool whose exposed name is illegal or
  // collides with an already-registered one.
  const tools: Tool[] = [];
  const routes = new Map<string, Route>();

  for (const client of clients) {
    let upstreamTools: Tool[];
    try {
      upstreamTools = await withTimeout(client.listTools(), callTimeoutMs, `'${client.id}' listTools`);
    } catch (err) {
      console.warn(
        `[gateway] failed to list tools from '${client.id}', skipping its tools: ${err instanceof Error ? err.message : String(err)}`,
      );
      continue;
    }

    for (const tool of upstreamTools) {
      const exposedName = client.prefix
        ? `${client.prefix}${TOOL_NS_SEPARATOR}${tool.name}`
        : tool.name;

      if (!OPENAI_TOOL_NAME.test(exposedName)) {
        console.warn(`[gateway] dropping '${client.id}' tool '${tool.name}': invalid exposed name '${exposedName}'`);
        continue;
      }
      if (routes.has(exposedName)) {
        console.warn(`[gateway] dropping '${client.id}' tool '${exposedName}': name already taken`);
        continue;
      }

      routes.set(exposedName, { client, originalName: tool.name });
      tools.push({ ...tool, name: exposedName });
    }
  }

  console.log(
    `[gateway] aggregated ${tools.length} tools from ${clients.length} source(s): ${clients.map((c) => c.id).join(", ")}`,
  );

  const server = new Server(
    { name: "mcp-tools-gateway", version: "0.1.0" },
    { capabilities: { tools: {} } },
  );

  server.setRequestHandler(ListToolsRequestSchema, () => ({ tools }));

  server.setRequestHandler(CallToolRequestSchema, async (request) => {
    const route = routes.get(request.params.name);
    if (!route) {
      return { content: [{ type: "text", text: `[gateway] unknown tool: ${request.params.name}` }], isError: true };
    }
    try {
      return await route.client.call(route.originalName, request.params.arguments ?? {});
    } catch (err) {
      // Isolate upstream failures: report as a tool error, keep the gateway up.
      return {
        content: [
          {
            type: "text",
            text: `[gateway] upstream '${route.client.id}' failed: ${err instanceof Error ? err.message : String(err)}`,
          },
        ],
        isError: true,
      };
    }
  });

  return {
    server,
    async close() {
      await Promise.all(clients.map((c) => c.close().catch(() => undefined)));
    },
  };
}
