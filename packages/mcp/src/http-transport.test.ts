import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { afterEach, describe, expect, it } from "vitest";
import { z } from "zod";
import { runHttpTransport, type RunningHttpTransport } from "./http-transport";

// Regression cover for the ChatGPT tunnel outage of 2026-08-23: the restricted
// instance shared one McpServer across sessions, so the second `initialize`
// threw "Already connected to a transport" → HTTP 500. tunnel-client holds a
// probe session of its own, which made ChatGPT permanently the second client.

let running: RunningHttpTransport | undefined;
const clients: Client[] = [];

afterEach(async () => {
  for (const client of clients.splice(0)) await client.close();
  await running?.close();
  running = undefined;
});

function buildEndpoint(): McpServer {
  const server = new McpServer({ name: "test-mcp", version: "0.0.0" });
  server.registerTool(
    "ping",
    { title: "ping", description: "returns pong", inputSchema: { value: z.string().optional() } },
    async () => ({ content: [{ type: "text" as const, text: "pong" }] }),
  );
  return server;
}

async function connectClient(port: number, name: string): Promise<Client> {
  const client = new Client({ name, version: "1.0.0" });
  clients.push(client);
  await client.connect(new StreamableHTTPClientTransport(new URL(`http://127.0.0.1:${port}/mcp`)));
  return client;
}

describe("runHttpTransport", () => {
  it("serves two concurrent clients when multiSession is on", async () => {
    let built = 0;
    running = await runHttpTransport({
      port: 0,
      multiSession: true,
      createEndpoint: async () => {
        built++;
        return buildEndpoint();
      },
    });

    // Sequential connects, both kept open — this is the tunnel's shape:
    // tunnel-client probes first and stays connected, ChatGPT arrives later.
    const first = await connectClient(running.port, "probe");
    const second = await connectClient(running.port, "chatgpt");

    // Both sessions must be independently usable, not merely established.
    await expect(first.listTools()).resolves.toMatchObject({ tools: [{ name: "ping" }] });
    await expect(second.listTools()).resolves.toMatchObject({ tools: [{ name: "ping" }] });

    expect(built).toBe(2);
  });

  it("builds the endpoint once when multiSession is off", async () => {
    let built = 0;
    running = await runHttpTransport({
      port: 0,
      multiSession: false,
      createEndpoint: async () => {
        built++;
        return buildEndpoint();
      },
    });

    // Built eagerly, before any client — the pre-existing behaviour the full
    // instance still relies on.
    expect(built).toBe(1);

    const client = await connectClient(running.port, "agent");
    await expect(client.listTools()).resolves.toMatchObject({ tools: [{ name: "ping" }] });
    expect(built).toBe(1);
  });

  // The production crash-loop (2026-08-23, 78 agent restarts): the supervisor
  // dies without closing its transport, and every reconnect gets "Already
  // connected" → 500 → exit 1 → restart → 500. Newest wins breaks it.
  it("evicts the previous session when multiSession is off", async () => {
    running = await runHttpTransport({
      port: 0,
      multiSession: false,
      createEndpoint: async () => buildEndpoint(),
    });

    const first = await connectClient(running.port, "agent-before-crash");
    await expect(first.listTools()).resolves.toMatchObject({ tools: [{ name: "ping" }] });

    // The reconnecting agent must get in, and must be able to work — an
    // accepted-but-unusable session would be the same outage wearing a hat.
    const second = await connectClient(running.port, "agent-after-restart");
    await expect(second.listTools()).resolves.toMatchObject({ tools: [{ name: "ping" }] });

    // And a third, because the loop repeated on every restart.
    const third = await connectClient(running.port, "agent-after-restart-2");
    await expect(third.listTools()).resolves.toMatchObject({ tools: [{ name: "ping" }] });
  });

  it("keeps serving the newest session after eviction, not the evicted one", async () => {
    running = await runHttpTransport({
      port: 0,
      multiSession: false,
      createEndpoint: async () => buildEndpoint(),
    });

    const evicted = await connectClient(running.port, "old");
    const current = await connectClient(running.port, "new");

    // The evicted client's session id is gone, so its calls must fail rather
    // than silently share the newcomer's session.
    await expect(evicted.listTools()).rejects.toThrow();
    await expect(current.listTools()).resolves.toMatchObject({ tools: [{ name: "ping" }] });
  });
});
