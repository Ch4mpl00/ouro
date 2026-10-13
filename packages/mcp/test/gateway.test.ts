// The gateway over real Streamable HTTP upstreams (in-process MCP servers on
// loopback) plus an own server: generated tool sets with clashing and
// invalid names, upstreams that are down at boot, tools that fail, and
// upstream sessions that get evicted between calls. The merged surface must
// be exactly what the namespacing rules promise, and every call must reach
// the tool it names, with its arguments, or fail as a tool error — never as
// a broken connection.

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import fc from "fast-check";
import { describe, expect, it } from "vitest";
import { z } from "zod";
import { runHttpTransport, type RunningHttpTransport } from "../src/http-transport";
import { createGatewayModule } from "../src/services/gateway";

type Behaviour = "echo" | "error" | "throw";
interface ToolSpec {
  name: string;
  behaviour: Behaviour;
}
interface UpstreamSpec {
  prefix: string;
  up: boolean;
  tools: ToolSpec[];
}

const VALID_EXPOSED = /^[a-zA-Z0-9_-]{1,64}$/;

// A server whose tools answer with who they are and what they were given.
function serverWith(source: string, tools: ToolSpec[]): McpServer {
  const server = new McpServer({ name: source, version: "0.0.0" });
  const seen = new Set<string>();
  for (const t of tools) {
    if (seen.has(t.name)) continue;
    seen.add(t.name);
    server.registerTool(t.name, { description: `${source}/${t.name}`, inputSchema: { q: z.string().optional() } }, async (args) => {
      if (t.behaviour === "throw") throw new Error(`${source}/${t.name} blew up`);
      const text = JSON.stringify({ source, tool: t.name, args });
      return t.behaviour === "error" ? { content: [{ type: "text" as const, text }], isError: true } : { content: [{ type: "text" as const, text }] };
    });
  }
  return server;
}

const toolName = fc.constantFrom("search", "fetch", "x", "tavily__x", "bad name", "dot.name", "y".repeat(60));
const tool = fc.record({ name: toolName, behaviour: fc.constantFrom<Behaviour>("echo", "echo", "error", "throw") });
const upstream = fc.record({
  prefix: fc.constantFrom("tavily", "web", "a-b", "z".repeat(70)),
  up: fc.oneof({ weight: 3, arbitrary: fc.constant(true) }, { weight: 1, arbitrary: fc.constant(false) }),
  tools: fc.array(tool, { minLength: 1, maxLength: 4 }),
});

// The merged surface the rules promise: own tools first, then each reachable
// upstream in order; prefix__name; an invalid or already-taken name is dropped.
function expectedSurface(own: ToolSpec[], ups: UpstreamSpec[]): Map<string, { source: string; original: string; behaviour: Behaviour }> {
  const out = new Map<string, { source: string; original: string; behaviour: Behaviour }>();
  const add = (source: string, prefix: string, tools: ToolSpec[]) => {
    const seen = new Set<string>();
    for (const t of tools) {
      if (seen.has(t.name)) continue;
      seen.add(t.name);
      const exposed = prefix ? `${prefix}__${t.name}` : t.name;
      if (!VALID_EXPOSED.test(exposed) || out.has(exposed)) continue;
      out.set(exposed, { source, original: t.name, behaviour: t.behaviour });
    }
  };
  add("own", "", own.filter((t) => VALID_EXPOSED.test(t.name)));
  ups.forEach((u, i) => {
    if (u.up) add(`up${i}`, u.prefix, u.tools);
  });
  return out;
}

describe("gateway over real HTTP upstreams", () => {
  it("serves exactly the namespaced union and routes every call to its tool", async () => {
    await fc.assert(
      fc.asyncProperty(
        fc.array(tool, { minLength: 1, maxLength: 3 }).map((ts) => ts.filter((t) => VALID_EXPOSED.test(t.name))),
        fc.array(upstream, { maxLength: 3 }),
        fc.array(fc.nat(), { minLength: 1, maxLength: 4 }),
        async (own, ups, kicks) => {
          const running: RunningHttpTransport[] = [];
          const urls: string[] = [];
          for (const [i, u] of ups.entries()) {
            if (!u.up) {
              urls.push("http://127.0.0.1:9/mcp"); // nothing listens on the discard port
              continue;
            }
            // Single-session, like a strict upstream: a second client evicts
            // the gateway's session, which the gateway must survive.
            const t = await runHttpTransport({ port: 0, createEndpoint: async () => serverWith(`up${i}`, u.tools), multiSession: false });
            running.push(t);
            urls.push(`http://127.0.0.1:${t.port}/mcp`);
          }
          const gateway = await createGatewayModule({
            ownServer: serverWith("own", own),
            upstreams: ups.map((u, i) => ({ name: `up${i}`, prefix: u.prefix, url: urls[i]!, headers: {} })),
            connectTimeoutMs: 3_000,
            callTimeoutMs: 3_000,
          });
          const [ct, st] = InMemoryTransport.createLinkedPair();
          await gateway.server.connect(st);
          const client = new Client({ name: "agent", version: "0" });
          await client.connect(ct);
          const where = JSON.stringify({ own, ups });
          try {
            const want = expectedSurface(own, ups);
            const { tools } = await client.listTools();
            expect(tools.map((t) => t.name).sort(), `${where}: the merged surface`).toEqual([...want.keys()].sort());

            const names = [...want.keys()];
            for (const [i, pick] of kicks.entries()) {
              // Evict the gateway's session at one upstream now and then.
              if (i % 2 === 1 && running.length > 0) {
                const t = running[pick % running.length]!;
                const intruder = new Client({ name: "intruder", version: "0" });
                await intruder.connect(new StreamableHTTPClientTransport(new URL(`http://127.0.0.1:${t.port}/mcp`)));
                await intruder.close();
              }
              if (names.length === 0) break;
              const exposed = names[pick % names.length]!;
              const route = want.get(exposed)!;
              const q = `call-${i}`;
              const result = await client.callTool({ name: exposed, arguments: { q } });
              const text = Array.isArray(result.content) && result.content[0]?.type === "text" ? String(result.content[0].text) : "";
              if (route.behaviour === "throw") {
                expect(result.isError, `${where}: ${exposed} threw → a tool error`).toBe(true);
                expect(text, `${where}: ${exposed} error says what failed`).toContain("blew up");
                continue;
              }
              expect(result.isError === true, `${where}: ${exposed} error flag passes through`).toBe(route.behaviour === "error");
              expect(JSON.parse(text), `${where}: ${exposed} reached its tool with its arguments`).toEqual({ source: route.source, tool: route.original, args: { q } });
            }

            const unknown = await client.callTool({ name: "nope__nothing", arguments: {} });
            expect(unknown.isError, "an unknown tool is a tool error").toBe(true);
          } finally {
            await client.close();
            await gateway.close();
            for (const t of running) await t.close();
          }
        },
      ),
      { numRuns: 60 },
    );
  }, 300_000);
});
