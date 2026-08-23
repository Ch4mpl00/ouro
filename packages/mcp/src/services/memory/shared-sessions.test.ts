import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { afterEach, describe, expect, it } from "vitest";
import { runHttpTransport, type RunningHttpTransport } from "../../http-transport";
import { registerMemoryTools } from "../../tools/memory";
import type { EmbeddingService } from "../embeddings/service";
import { createIndexer } from "./indexer";
import { createMemoryService } from "./service";
import { createInMemoryStore } from "./store.memory";
import type { MemoryStore } from "./store";

// The claim D1 makes, exercised rather than asserted by construction: several
// agents hold a memory connection to ONE restricted MCP instance at the same
// time, over real HTTP, and see each other's writes.
//
// This is the shape mcp-tunnel runs in — MCP_TOOLSETS scoped, therefore
// multi-session — and it is what has to hold before ChatGPT is pointed at
// memory: tunnel-client keeps a probe session of its own, so an arriving
// ChatGPT client is always at least the second connection.

let running: RunningHttpTransport | undefined;
const clients: Client[] = [];

afterEach(async () => {
  for (const client of clients.splice(0)) await client.close();
  await running?.close();
  running = undefined;
});

function fakeEmbed(text: string): number[] {
  const vector = new Array<number>(32).fill(0);
  for (const token of text.toLowerCase().match(/[\p{L}\p{N}]+/gu) ?? []) {
    let hash = 0;
    for (const ch of token) hash = (hash * 31 + ch.codePointAt(0)!) % 32;
    vector[hash] = (vector[hash] ?? 0) + 1;
  }
  return vector;
}

// One store, one service — exactly like production, where every session of an
// instance shares the same Postgres. Only the McpServer is per-session.
function endpointFactory(store: MemoryStore): (actor: string) => McpServer {
  const embeddings: EmbeddingService = {
    embed: async (text) => fakeEmbed(text),
    embedBatch: async (texts) => texts.map(fakeEmbed),
  };
  const memory = createMemoryService({ store, indexer: createIndexer({ store, embeddings }) });
  return (actor) => {
    const server = new McpServer({ name: "memory-test", version: "0.0.0" });
    registerMemoryTools(server, { memory, actor });
    return server;
  };
}

async function connect(port: number, name: string): Promise<Client> {
  const client = new Client({ name, version: "1.0.0" });
  clients.push(client);
  await client.connect(new StreamableHTTPClientTransport(new URL(`http://127.0.0.1:${port}/mcp`)));
  return client;
}

async function call(
  client: Client,
  name: string,
  args: Record<string, unknown> = {},
): Promise<Record<string, unknown>> {
  const result = await client.callTool({ name, arguments: args });
  const content = result.content as { type: string; text: string }[];
  const block = content.find((item) => item.type === "text");
  if (!block) throw new Error(`${name} returned no text content`);
  return JSON.parse(block.text) as Record<string, unknown>;
}

describe("memory over a multi-session MCP instance", () => {
  it("lets two concurrent clients share one memory, each signed as itself", async () => {
    const store = createInMemoryStore();
    // The instance decides each session's actor; in production that is
    // MCP_MEMORY_ACTOR, one value per instance. Here two are built so the
    // history assertion can tell the sessions apart.
    const build = endpointFactory(store);
    const actors = ["supervisor", "chatgpt"];
    let built = 0;
    running = await runHttpTransport({
      port: 0,
      multiSession: true,
      createEndpoint: async () => build(actors[built++] ?? "unknown"),
    });

    // Both stay connected, which is the tunnel's shape: the probe session
    // never goes away when ChatGPT arrives.
    const supervisor = await connect(running.port, "supervisor");
    const chatgpt = await connect(running.port, "chatgpt");

    expect((await chatgpt.listTools()).tools.map((t) => t.name)).toContain("recall");

    await call(supervisor, "create_project", { slug: "graphs", title: "Графы" });
    await call(supervisor, "write_doc", {
      project: "graphs",
      doc: "roadmap.md",
      body: "# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n",
      summary: "Темы",
    });

    // ChatGPT sees what the supervisor wrote…
    const listed = await call(chatgpt, "list_memory", { project: "graphs" });
    expect(listed).toMatchObject({ ok: true, docs: [{ name: "roadmap.md", summary: "Темы" }] });

    // …and writes back into the same document.
    const read = await call(chatgpt, "read_doc", { project: "graphs", doc: "roadmap.md" });
    const version = (read.doc as { version: number }).version;
    const patched = await call(chatgpt, "patch_doc", {
      project: "graphs",
      doc: "roadmap.md",
      expected_version: version,
      edits: [{ old: "- [ ] BFS", new: "- [x] BFS" }],
      rationale: "отметь BFS",
    });
    expect(patched).toMatchObject({ ok: true });

    // The supervisor's session, still open, sees it immediately.
    const after = await call(supervisor, "read_doc", { project: "graphs", doc: "roadmap.md" });
    expect((after.doc as { body: string }).body).toContain("- [x] BFS");

    // And history attributes the change to the client that made it.
    const history = await call(supervisor, "doc_history", { project: "graphs", doc: "roadmap.md" });
    expect(history.history).toMatchObject([
      { kind: "patch", actor: "chatgpt", rationale: "отметь BFS" },
      { kind: "write", actor: "supervisor" },
    ]);
  });

  it("recalls across sessions: one writes a fact, the other finds it", async () => {
    const store = createInMemoryStore();
    const build = endpointFactory(store);
    running = await runHttpTransport({
      port: 0,
      multiSession: true,
      createEndpoint: async () => build("chatgpt"),
    });

    const writer = await connect(running.port, "writer");
    const reader = await connect(running.port, "reader");

    await call(writer, "remember", { body: "Dijkstra не любит отрицательные веса", tags: ["графы"] });

    const recalled = await call(reader, "recall", { query: "отрицательные веса Dijkstra" });
    const hits = recalled.hits as { ref: string; text: string }[];

    expect(hits[0]?.text).toContain("Dijkstra");
    expect(hits[0]?.ref).toMatch(/^fact:\d+$/);
  });

  // The version check is what keeps two live sessions from clobbering each
  // other; over HTTP it must still arrive as data, not as a transport error.
  it("returns a conflict to the loser of a concurrent edit rather than losing a write", async () => {
    const store = createInMemoryStore();
    const build = endpointFactory(store);
    running = await runHttpTransport({
      port: 0,
      multiSession: true,
      createEndpoint: async () => build("chatgpt"),
    });

    const a = await connect(running.port, "a");
    const b = await connect(running.port, "b");
    await call(a, "create_project", { slug: "p", title: "P" });
    await call(a, "write_doc", { project: "p", doc: "d.md", body: "- [ ] one\n- [ ] two\n" });

    // Both read version 1, then both try to patch it.
    const first = await call(a, "patch_doc", {
      project: "p",
      doc: "d.md",
      expected_version: 1,
      edits: [{ old: "- [ ] one", new: "- [x] one" }],
    });
    const second = await call(b, "patch_doc", {
      project: "p",
      doc: "d.md",
      expected_version: 1,
      edits: [{ old: "- [ ] two", new: "- [x] two" }],
    });

    expect(first).toMatchObject({ ok: true });
    expect(second).toMatchObject({ ok: false, error: "version_conflict", currentVersion: 2 });

    // The loser's change is absent, not half-applied — it re-reads and retries.
    const body = ((await call(a, "read_doc", { project: "p", doc: "d.md" })).doc as { body: string }).body;
    expect(body).toBe("- [x] one\n- [ ] two\n");
  });
});
