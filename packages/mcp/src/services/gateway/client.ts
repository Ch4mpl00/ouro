import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import {
  CallToolResultSchema,
  type CallToolResult,
  type CompatibilityCallToolResult,
  type Tool,
} from "@modelcontextprotocol/sdk/types.js";

// What client.callTool() resolves to: the modern result or the legacy
// 2024-10-07 compat shape. The gateway passes whichever the upstream returns
// through verbatim rather than narrowing (and risking dropping fields).
type ToolResult = CallToolResult | CompatibilityCallToolResult;

// A single aggregated MCP source behind the gateway. Two flavours share one
// interface: the local own-MCP (in-process, in-memory transport) and each
// remote third-party server (StreamableHTTP). The gateway lists each source's
// tools once at boot and routes calls to them by name.

export interface GatewayClient {
  // Label for logs/errors (e.g. "own", "tavily").
  id: string;
  // Namespace prepended to this source's tool names (empty for own-MCP, so its
  // tools keep their original names and existing skills are unaffected).
  prefix: string;
  listTools(): Promise<Tool[]>;
  call(name: string, args: Record<string, unknown>): Promise<ToolResult>;
  close(): Promise<void>;
}

// MCP sessions are stateful on the server side; a remote restart invalidates our
// cached session id. Same detection the agent's mcp-client uses — drop the dead
// client and reconnect once before retrying the call.
function isConnectionLostError(err: unknown): boolean {
  const msg = err instanceof Error ? err.message : String(err);
  return (
    msg.includes("No valid session id") ||
    msg.includes("Session not found") ||
    msg.includes("Not connected") ||
    msg.includes("terminated") ||
    msg.includes("ECONNREFUSED") ||
    msg.includes("socket hang up")
  );
}

// own-MCP as an in-process upstream. The gateway is wired so that extracting it
// to its own container later means only swapping this for a remote client.
export async function connectOwnClient(ownServer: McpServer): Promise<GatewayClient> {
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await ownServer.connect(serverTransport);
  const client = new Client({ name: "gateway->own", version: "0.1.0" }, { capabilities: {} });
  await client.connect(clientTransport);

  return {
    id: "own",
    prefix: "",
    async listTools() {
      const { tools } = await client.listTools();
      return tools;
    },
    async call(name, args) {
      // No timeout/reconnect: same process, the link cannot drop independently.
      return client.callTool({ name, arguments: args }, CallToolResultSchema);
    },
    close: () => client.close(),
  };
}

export interface RemoteClientConfig {
  id: string;
  prefix: string;
  url: string;
  headers: Record<string, string>;
  callTimeoutMs: number;
}

// A remote StreamableHTTP upstream with single-flight reconnect. Throws from
// connect() if the initial handshake fails — the module catches it and skips the
// upstream so one unreachable server can't take down own-MCP / the pollers.
export async function connectRemoteClient(cfg: RemoteClientConfig): Promise<GatewayClient> {
  const open = async (): Promise<Client> => {
    const transport = new StreamableHTTPClientTransport(new URL(cfg.url), {
      requestInit: Object.keys(cfg.headers).length > 0 ? { headers: cfg.headers } : undefined,
    });
    const c = new Client({ name: `gateway->${cfg.id}`, version: "0.1.0" }, { capabilities: {} });
    await c.connect(transport);
    return c;
  };

  let client = await open();
  let reconnecting: Promise<Client> | null = null;

  function reconnect(failed: Client): Promise<Client> {
    if (failed !== client) return Promise.resolve(client);
    reconnecting ??= (async () => {
      try {
        await failed.close();
      } catch {
        /* dead session — ignore */
      }
      client = await open();
      return client;
    })().finally(() => {
      reconnecting = null;
    });
    return reconnecting;
  }

  return {
    id: cfg.id,
    prefix: cfg.prefix,
    async listTools() {
      const { tools } = await client.listTools(undefined, { timeout: cfg.callTimeoutMs });
      return tools;
    },
    async call(name, args) {
      const used = client;
      try {
        return await used.callTool({ name, arguments: args }, CallToolResultSchema, {
          timeout: cfg.callTimeoutMs,
        });
      } catch (err) {
        if (!isConnectionLostError(err)) throw err;
        console.warn(`[gateway] upstream '${cfg.id}' connection lost, reconnecting…`);
        const fresh = await reconnect(used);
        return await fresh.callTool({ name, arguments: args }, CallToolResultSchema, {
          timeout: cfg.callTimeoutMs,
        });
      }
    },
    close: () => client.close(),
  };
}
