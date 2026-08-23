import { createServer as createHttpServer, type IncomingMessage, type ServerResponse } from "node:http";
import { randomUUID } from "node:crypto";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import { isInitializeRequest } from "@modelcontextprotocol/sdk/types.js";

// The client-facing endpoint is either an McpServer directly (no upstreams) or
// the gateway's low-level Server (own-MCP + third-party MCPs merged). Both
// expose connect(transport); that's all the transport plumbing needs.
export interface ConnectableServer {
  connect(transport: Transport): Promise<void>;
  close?(): Promise<void>;
}

export interface HttpTransportOptions {
  port: number;
  // Builds the endpoint a session binds to: once up front in single-session
  // mode, once per `initialize` in multi-session mode.
  createEndpoint: () => Promise<ConnectableServer>;
  // Whether concurrent clients are allowed.
  //
  // The SDK forbids connecting one Server to two transports, so a shared
  // instance means the second client's `initialize` throws "Already connected"
  // → HTTP 500. That is deliberate for the full instance: the signals queue has
  // no delivery coordination, so two agents would race for the same signal
  // (.claude/tasks/mcp-connection-lifecycle.md).
  //
  // A restricted instance has no signals tools and no gateway, so nothing is
  // raced and each session can cheaply own its server. The ChatGPT tunnel needs
  // this: tunnel-client holds a probe session of its own, so an arriving
  // ChatGPT client is *always* the second connection.
  multiSession: boolean;
}

export interface RunningHttpTransport {
  // Bound port — meaningful when 0 was passed to get an ephemeral one.
  port: number;
  close(): Promise<void>;
}

async function readBody(req: IncomingMessage): Promise<unknown> {
  if (req.method !== "POST") return undefined;
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);
  const body = Buffer.concat(chunks).toString("utf-8");
  if (!body) return undefined;
  try {
    return JSON.parse(body);
  } catch {
    return undefined;
  }
}

// One session per connecting client, created lazily on `initialize` and tracked
// by the mcp-session-id header for subsequent calls (per the Streamable HTTP
// spec).
export async function runHttpTransport(options: HttpTransportOptions): Promise<RunningHttpTransport> {
  const { createEndpoint, multiSession } = options;
  const transports = new Map<string, StreamableHTTPServerTransport>();
  // Single-session mode keeps the historical behaviour exactly: build the
  // endpoint once, before listening, and hand every session the same one.
  const sharedEndpoint = multiSession ? undefined : await createEndpoint();

  const http = createHttpServer(async (req: IncomingMessage, res: ServerResponse) => {
    if (!req.url || !req.url.startsWith("/mcp")) {
      res.writeHead(404).end();
      return;
    }
    try {
      const parsedBody = await readBody(req);
      const sessionId = (req.headers["mcp-session-id"] as string | undefined) ?? undefined;
      let transport = sessionId ? transports.get(sessionId) : undefined;

      if (!transport) {
        if (req.method === "POST" && isInitializeRequest(parsedBody)) {
          transport = new StreamableHTTPServerTransport({
            sessionIdGenerator: () => randomUUID(),
            onsessioninitialized: (sid) => {
              transports.set(sid, transport!);
            },
          });
          const endpoint = sharedEndpoint ?? (await createEndpoint());
          // Latch: closing the endpoint closes its transport, which fires
          // onclose again — without this the two bounce until the stack blows.
          let teardownStarted = false;
          transport.onclose = () => {
            if (teardownStarted) return;
            teardownStarted = true;
            if (transport!.sessionId) transports.delete(transport!.sessionId);
            // A per-session endpoint dies with its session; the shared one
            // outlives every session and must never be closed here.
            if (!sharedEndpoint) {
              void endpoint.close?.().catch((err: unknown) => {
                console.error("[mcp-http] endpoint close failed:", err);
              });
            }
          };
          await endpoint.connect(transport);
        } else {
          res.writeHead(400, { "content-type": "application/json" });
          res.end(
            JSON.stringify({
              jsonrpc: "2.0",
              error: { code: -32000, message: "No valid session id; send an initialize request first." },
              id: null,
            }),
          );
          return;
        }
      }

      await transport.handleRequest(req, res, parsedBody);
    } catch (err) {
      console.error("[mcp-http] request failed:", err);
      if (!res.headersSent) {
        res.writeHead(500, { "content-type": "application/json" });
        res.end(JSON.stringify({ jsonrpc: "2.0", error: { code: -32603, message: "internal" }, id: null }));
      }
    }
  });

  await new Promise<void>((resolve) => http.listen(options.port, "0.0.0.0", resolve));
  const address = http.address();
  const port = typeof address === "object" && address !== null ? address.port : options.port;
  console.log(`[mcp-http] listening on 0.0.0.0:${port}${multiSession ? " (multi-session)" : ""}`);

  return {
    port,
    close: async () => {
      for (const transport of transports.values()) await transport.close();
      transports.clear();
      await sharedEndpoint?.close?.();
      await new Promise<void>((resolve, reject) =>
        http.close((err) => (err ? reject(err) : resolve())),
      );
    },
  };
}
