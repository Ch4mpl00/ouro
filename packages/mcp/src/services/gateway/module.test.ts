import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { describe, expect, it } from "vitest";
import { z } from "zod";
import { createGatewayModule } from "./module";

// Builds own-MCP with a couple of tools, fronts it with the gateway (no remote
// upstreams — those are exercised by the live pilot, not in CI), and drives it
// through a real MCP Client over an in-memory transport.
async function connectGateway(register: (s: McpServer) => void) {
  const own = new McpServer({ name: "own", version: "0.0.0" });
  register(own);
  const gw = await createGatewayModule({ ownServer: own, upstreams: [] });

  const [clientT, serverT] = InMemoryTransport.createLinkedPair();
  await gw.server.connect(serverT);
  const client = new Client({ name: "test", version: "0.0.0" }, { capabilities: {} });
  await client.connect(clientT);
  return { client, close: () => gw.close().then(() => client.close()) };
}

describe("createGatewayModule (own-MCP passthrough)", () => {
  it("lists own tools un-prefixed with their schema verbatim", async () => {
    const { client, close } = await connectGateway((s) =>
      s.registerTool(
        "echo",
        { description: "echo back", inputSchema: { msg: z.string().describe("text") } },
        async ({ msg }) => ({ content: [{ type: "text", text: `echo:${msg}` }] }),
      ),
    );

    const { tools } = await client.listTools();
    const echo = tools.find((t) => t.name === "echo");
    expect(echo).toBeDefined();
    expect(echo!.description).toBe("echo back");
    expect(echo!.inputSchema.properties).toHaveProperty("msg");

    await close();
  });

  it("routes a tools/call to the owning source", async () => {
    const { client, close } = await connectGateway((s) =>
      s.registerTool(
        "echo",
        { description: "echo", inputSchema: { msg: z.string() } },
        async ({ msg }) => ({ content: [{ type: "text", text: `echo:${msg}` }] }),
      ),
    );

    const res = await client.callTool({ name: "echo", arguments: { msg: "hi" } });
    expect(res.content).toEqual([{ type: "text", text: "echo:hi" }]);

    await close();
  });

  it("returns an isError result for an unknown tool", async () => {
    const { client, close } = await connectGateway((s) =>
      s.registerTool("echo", { description: "echo", inputSchema: {} }, async () => ({
        content: [{ type: "text", text: "ok" }],
      })),
    );

    const res = await client.callTool({ name: "nope", arguments: {} });
    expect(res.isError).toBe(true);

    await close();
  });
});
