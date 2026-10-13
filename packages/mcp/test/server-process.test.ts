// The real server process — server.ts main(), the composition root — booted
// over HTTP the way the droplet runs `mcp` and `mcp-tunnel`, with a gateway
// config naming one paid upstream. Whatever MCP_TOOLSETS says: a restricted
// instance never reaches the upstream (no session, no call, nothing of it in
// tools/list), so a memory or tunnel client cannot spend third-party
// credits; an out-of-scope tool can't be called even by its exact name; and
// the full instance does serve the upstream through the gateway.
//
// The process runs from an empty directory with a minimal environment, so no
// .env with real credentials is ever read, and with MCP_NO_POLLERS=1.

import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import fc from "fast-check";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { z } from "zod";
import { runHttpTransport, type RunningHttpTransport } from "../src/http-transport";
import { parseToolsets } from "../src/toolsets";
import { call } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";

const REPO = path.resolve(import.meta.dirname, "../../..");
const TSX = path.join(REPO, "node_modules/.bin/tsx");
const SERVER = path.join(REPO, "packages/mcp/src/server.ts");
const GROUPS = ["memory", "news-read", "telegram-send", "skills", "signals", "scheduler", "fs"];

async function freePort(): Promise<number> {
  const server = net.createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  await new Promise((resolve) => server.close(resolve));
  if (address === null || typeof address === "string") throw new Error("no TCP address");
  return address.port;
}

// Boots server.ts and resolves once it listens; rejects if it exits first.
async function boot(env: Record<string, string>, cwd: string): Promise<{ url: string; child: ChildProcess; output: () => string }> {
  const port = await freePort();
  const child = spawn(TSX, [SERVER], {
    cwd,
    env: { PATH: process.env.PATH ?? "", HOME: process.env.HOME ?? "", MCP_TRANSPORT: "http", MCP_PORT: String(port), MCP_NO_POLLERS: "1", ...env },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let output = "";
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`server did not start:\n${output}`)), 60_000);
    const onData = (chunk: Buffer) => {
      output += chunk.toString();
      if (output.includes("[mcp-http] listening")) {
        clearTimeout(timer);
        resolve();
      }
    };
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.once("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`server exited (${code}) before listening:\n${output}`));
    });
  });
  return { url: `http://127.0.0.1:${port}/mcp`, child, output: () => output };
}

async function stop(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null) return;
  const exited = new Promise((resolve) => child.once("exit", resolve));
  child.kill("SIGTERM");
  const timer = setTimeout(() => child.kill("SIGKILL"), 5_000);
  await exited;
  clearTimeout(timer);
}

const available = await databaseAvailable();

describe.skipIf(!available)("the server process and its gateway", () => {
  let db: TestDatabase;
  let dir: string;
  let upstream: RunningHttpTransport;
  // What the paid upstream has seen.
  const seen = { sessions: 0, charges: 0 };

  beforeAll(async () => {
    db = await testDatabase();
    dir = mkdtempSync(path.join(tmpdir(), "server-process-"));
    upstream = await runHttpTransport({
      port: 0,
      multiSession: true,
      createEndpoint: async () => {
        seen.sessions++;
        const server = new McpServer({ name: "paid", version: "0" });
        server.registerTool("charge", { description: "Costs money per call.", inputSchema: { q: z.string().optional() } }, async () => {
          seen.charges++;
          return { content: [{ type: "text" as const, text: JSON.stringify({ charged: true }) }] };
        });
        return server;
      },
    });
    writeFileSync(
      path.join(dir, "gateway.config.json"),
      JSON.stringify({ upstreams: [{ name: "paid", prefix: "paid", url: `http://127.0.0.1:${upstream.port}/mcp` }] }),
    );
  }, 180_000);

  afterAll(async () => {
    await upstream?.close();
    await db?.drop();
    if (dir) rmSync(dir, { recursive: true, force: true });
  });

  it("only the full instance reaches the paid upstream, and out-of-scope tools stay uncallable", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(
        fc.oneof(
          { weight: 1, arbitrary: fc.constant<string | undefined>(undefined) },
          { weight: 1, arbitrary: fc.constant<string | undefined>("") },
          { weight: 4, arbitrary: fc.subarray(GROUPS, { minLength: 1 }).map((g): string | undefined => g.join(",")) },
        ),
        async (toolsets) => {
          const restricted = parseToolsets(toolsets).restricted;
          seen.sessions = 0;
          seen.charges = 0;
          const env: Record<string, string> = {
            DATABASE_URL: db.url,
            // Required at boot; nothing here embeds, so it is never used.
            OPENAI_API_KEY: "sk-test-unused",
            GATEWAY_CONFIG: path.join(dir, "gateway.config.json"),
            ...(process.env.STATE_DATABASE_URL ? { STATE_DATABASE_URL: process.env.STATE_DATABASE_URL } : {}),
            ...(toolsets === undefined ? {} : { MCP_TOOLSETS: toolsets }),
          };
          const server = await boot(env, dir);
          const client = new Client({ name: "t", version: "0" });
          const where = `MCP_TOOLSETS=${JSON.stringify(toolsets)}`;
          try {
            await client.connect(new StreamableHTTPClientTransport(new URL(server.url)));
            const names = (await client.listTools()).tools.map((t) => t.name);
            const charge = await call(client, "paid__charge", { q: "x" });
            if (restricted) {
              witnessed.add("restricted");
              expect(names.filter((n) => n.startsWith("paid__")), `${where}: no upstream tool listed`).toEqual([]);
              expect(charge.kind, `${where}: the upstream tool can't be called by name`).not.toBe("ok");
              expect(seen, `${where}: the upstream is never contacted`).toEqual({ sessions: 0, charges: 0 });
              if (!names.includes("get_next_signal")) {
                witnessed.add("signals out of scope");
                const next = await call(client, "get_next_signal", {});
                expect(next.kind, `${where}: get_next_signal can't be called by name`).not.toBe("ok");
              }
            } else {
              witnessed.add("full");
              expect(names, `${where}: the full instance serves the upstream`).toContain("paid__charge");
              expect(charge, `${where}: and the call reaches it`).toMatchObject({ kind: "ok", value: { charged: true } });
              expect(seen.charges, where).toBe(1);
            }
          } finally {
            await client.close().catch(() => {});
            await stop(server.child);
          }
        },
      ),
      {
        numRuns: Number(process.env.PROPERTY_RUNS ?? 8),
        // Every boot costs seconds, so the instances prod runs are always
        // among them: `mcp` (everything) and `mcp-tunnel`.
        examples: [[undefined], ["news-read,telegram-send,skills,memory"]],
      },
    );
    expect([...witnessed].sort()).toEqual(["full", "restricted", "signals out of scope"]);
  }, 600_000);
});
