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

  // Pins the deliberate single-connect semantics of the full instance — and
  // proves the multiSession test above is not vacuous: without it, this is
  // exactly what the ChatGPT tunnel hit ("Already connected" → 500).
  it("rejects a second concurrent client when multiSession is off", async () => {
    running = await runHttpTransport({
      port: 0,
      multiSession: false,
      createEndpoint: async () => buildEndpoint(),
    });

    await connectClient(running.port, "agent");
    await expect(connectClient(running.port, "intruder")).rejects.toThrow();
  });
});
