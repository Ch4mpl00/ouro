import "dotenv/config";
import { createServer as createHttpServer, type IncomingMessage, type ServerResponse } from "node:http";
import { randomUUID } from "node:crypto";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { isInitializeRequest } from "@modelcontextprotocol/sdk/types.js";
import { registerGmailTools } from "./tools/gmail";
import { registerTelegramTools } from "./tools/telegram";
import { registerMonobankTools } from "./tools/monobank";
import { registerPdfTools } from "./tools/pdf";
import { registerFsTools } from "./tools/fs";
import { registerSignalsTools } from "./tools/signals";
import { registerNewsTools } from "./tools/news";
import { registerKnowledgeTools } from "./tools/knowledge";
import { registerDreamingTools } from "./tools/dreaming";
import { registerUserbotTools } from "./tools/userbot";
import { registerSchedulerTools } from "./tools/scheduler";
import { startTelegramPoller } from "./services/telegram";
import { startGmailPoller } from "./services/gmail";
import { startSchedulerPoller } from "./services/scheduler";
import { createPgClient } from "./db/pg/client";
import { createNewsModule, startNewsModule, type NewsRepository } from "./services/news";
import { createKnowledgeModule, type KnowledgeRepository } from "./services/knowledge";

export interface ServerDeps {
  news: NewsRepository;
  knowledge: KnowledgeRepository;
}

export function createServer(deps: ServerDeps): McpServer {
  const server = new McpServer({
    name: "mcp-tools",
    version: "0.1.0",
  });

  registerGmailTools(server);
  registerTelegramTools(server);
  registerMonobankTools(server);
  registerPdfTools(server);
  registerFsTools(server);
  registerSignalsTools(server);
  registerNewsTools(server, deps.news);
  registerKnowledgeTools(server, deps.knowledge);
  registerDreamingTools(server);
  registerUserbotTools(server);
  registerSchedulerTools(server);

  return server;
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

// HTTP mode: single MCP server instance, one session per connecting client.
// Sessions are created lazily on the initialize request and tracked by
// the mcp-session-id header for subsequent calls (per the Streamable HTTP
// spec). For our deployment (one agent), there'll be exactly one session.
async function runHttpTransport(server: McpServer, port: number): Promise<void> {
  const transports = new Map<string, StreamableHTTPServerTransport>();

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
          transport.onclose = () => {
            if (transport!.sessionId) transports.delete(transport!.sessionId);
          };
          await server.connect(transport);
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

  await new Promise<void>((resolve) => http.listen(port, "0.0.0.0", resolve));
  console.log(`[mcp-http] listening on 0.0.0.0:${port}`);
}

async function main(): Promise<void> {
  // Postgres must be up and migrated before any tool handler or poller
  // touches news_items.
  const pg = createPgClient();
  await pg.ensureReady();

  const newsModule = createNewsModule({ db: pg.db });
  const knowledgeModule = createKnowledgeModule({ db: pg.db });

  const server = createServer({
    news: newsModule.repository,
    knowledge: knowledgeModule.repository,
  });
  const transport = (process.env.MCP_TRANSPORT ?? "stdio").toLowerCase();

  if (transport === "http") {
    const port = Number(process.env.MCP_PORT ?? 3000);
    if (!Number.isFinite(port)) throw new Error(`MCP_PORT must be numeric, got ${process.env.MCP_PORT}`);
    await runHttpTransport(server, port);
  } else {
    const stdio = new StdioServerTransport();
    await server.connect(stdio);
  }

  startTelegramPoller();
  startGmailPoller();
  startSchedulerPoller();
  startNewsModule(newsModule);
}

main().catch((err) => {
  console.error("mcp-tools server crashed", err);
  process.exit(1);
});
