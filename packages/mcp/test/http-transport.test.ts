// The Streamable HTTP transport under generated session lifecycles: clients
// connect, call, end their session or just go away, in both modes. A probe
// endpoint counts how many server instances exist, so leaks show.

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import fc from "fast-check";
import { describe, expect, it } from "vitest";
import { runHttpTransport } from "../src/http-transport";

type Op = { kind: "open" } | { kind: "call"; pick: number } | { kind: "end"; pick: number } | { kind: "abandon"; pick: number };

const op: fc.Arbitrary<Op> = fc.oneof(
  { weight: 3, arbitrary: fc.constant<Op>({ kind: "open" }) },
  { weight: 4, arbitrary: fc.nat().map((pick): Op => ({ kind: "call", pick })) },
  { weight: 2, arbitrary: fc.nat().map((pick): Op => ({ kind: "end", pick })) },
  { weight: 1, arbitrary: fc.nat().map((pick): Op => ({ kind: "abandon", pick })) },
);

// An endpoint with one tool that answers which instance it is.
function probeFactory() {
  let created = 0;
  let closed = 0;
  return {
    stats: () => ({ created, closed, live: created - closed }),
    create: async () => {
      const id = ++created;
      const server = new McpServer({ name: "probe", version: "0.0.0" });
      server.registerTool("whoami", { description: "instance id", inputSchema: {} }, async () => ({ content: [{ type: "text", text: String(id) }] }));
      const close = server.close.bind(server);
      return {
        connect: (t: Parameters<McpServer["connect"]>[0]) => server.connect(t),
        close: async () => {
          closed++;
          await close();
        },
      };
    },
  };
}

interface Session {
  client: Client;
  transport: StreamableHTTPClientTransport;
  state: "open" | "ended" | "abandoned";
}

async function works(c: Client): Promise<boolean> {
  try {
    await c.callTool({ name: "whoami", arguments: {} });
    return true;
  } catch {
    return false;
  }
}

describe("HTTP transport sessions", () => {
  it("multi-session: every open session works, an ended one stops working and lets go of its server", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(op, { minLength: 1, maxLength: 15, size: "max" }), async (ops) => {
        const probe = probeFactory();
        const http = await runHttpTransport({ port: 0, createEndpoint: probe.create, multiSession: true });
        const url = new URL(`http://127.0.0.1:${http.port}/mcp`);
        const sessions: Session[] = [];
        try {
          for (const o of ops) {
            const live = sessions.filter((s) => s.state === "open");
            if (o.kind === "open") {
              const transport = new StreamableHTTPClientTransport(url);
              const client = new Client({ name: "c", version: "0" });
              await client.connect(transport);
              sessions.push({ client, transport, state: "open" });
            } else if (live.length > 0) {
              const s = live[o.pick % live.length]!;
              if (o.kind === "call") expect(await works(s.client), "an open session answers").toBe(true);
              if (o.kind === "end") {
                await s.transport.terminateSession();
                await s.client.close();
                s.state = "ended";
              }
              if (o.kind === "abandon") {
                await s.client.close();
                s.state = "abandoned";
              }
            }
            // Every session that is still open keeps working.
            for (const s of sessions.filter((x) => x.state === "open")) expect(await works(s.client), "other sessions are unaffected").toBe(true);
            // An ended session holds no server instance.
            const held = sessions.filter((x) => x.state !== "ended").length;
            expect(probe.stats().live, `live server instances after ${JSON.stringify(o)}`).toBe(held);
          }
        } finally {
          for (const s of sessions) if (s.state === "open") await s.client.close();
          await http.close();
        }
        expect(probe.stats().live, "closing the transport closes every server instance").toBe(0);
      }),
      { numRuns: 60 },
    );
  }, 300_000);

  it("single-session: the newest session works, older ones are evicted and their state released", async () => {
    await fc.assert(
      fc.asyncProperty(fc.integer({ min: 1, max: 5 }), async (n) => {
        const probe = probeFactory();
        const http = await runHttpTransport({ port: 0, createEndpoint: probe.create, multiSession: false });
        const url = new URL(`http://127.0.0.1:${http.port}/mcp`);
        const clients: Client[] = [];
        try {
          for (let i = 0; i < n; i++) {
            const client = new Client({ name: "c", version: "0" });
            await client.connect(new StreamableHTTPClientTransport(url));
            clients.push(client);
            expect(await works(client), "the newest session works").toBe(true);
            for (const old of clients.slice(0, -1)) expect(await works(old), "an older session is evicted").toBe(false);
            expect(probe.stats().created, "one shared server instance").toBe(1);
          }
        } finally {
          for (const c of clients) await c.close();
          await http.close();
        }
        expect(probe.stats().live).toBe(0);
      }),
      { numRuns: 15 },
    );
  }, 300_000);
});
