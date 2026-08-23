import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { EmbeddingService } from "../services/embeddings/service";
import { createIndexer } from "../services/memory/indexer";
import { createMemoryService } from "../services/memory/service";
import { createInMemoryStore } from "../services/memory/store.memory";
import { registerMemoryTools } from "./memory";

// The tool layer's own contract: the surface it registers, and the promise
// that an expected failure arrives as data the model can act on rather than
// as a thrown error.

function fakeEmbed(text: string): number[] {
  const vector = new Array<number>(32).fill(0);
  for (const token of text.toLowerCase().match(/[\p{L}\p{N}]+/gu) ?? []) {
    let hash = 0;
    for (const ch of token) hash = (hash * 31 + ch.codePointAt(0)!) % 32;
    vector[hash] = (vector[hash] ?? 0) + 1;
  }
  return vector;
}

let client: Client;
let server: McpServer;

async function call(name: string, args: Record<string, unknown> = {}): Promise<Record<string, unknown>> {
  const result = await client.callTool({ name, arguments: args });
  const content = result.content as { type: string; text: string }[];
  const block = content.find((item) => item.type === "text");
  if (!block) throw new Error(`${name} returned no text content`);
  return JSON.parse(block.text) as Record<string, unknown>;
}

beforeEach(async () => {
  const store = createInMemoryStore();
  const embeddings: EmbeddingService = {
    embed: async (text) => fakeEmbed(text),
    embedBatch: async (texts) => texts.map(fakeEmbed),
  };
  const indexer = createIndexer({ store, embeddings });
  const memory = createMemoryService({ store, indexer });
  server = new McpServer({ name: "test", version: "0.0.0" });
  registerMemoryTools(server, { memory, actor: "claude-code@laptop" });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);
  client = new Client({ name: "test", version: "0.0.0" }, { capabilities: {} });
  await client.connect(clientTransport);
});

afterEach(async () => {
  await client.close();
  await server.close();
});

describe("memory tool surface", () => {
  it("registers exactly the documented tools", async () => {
    const names = (await client.listTools()).tools.map((t) => t.name).sort();

    expect(names).toEqual([
      "append_doc",
      "create_project",
      "doc_history",
      "get_fact",
      "list_memory",
      "patch_doc",
      "read_doc",
      "recall",
      "remember",
      "revert_patch",
      "update_fact",
      "write_doc",
    ]);
  });

  it("steers write_doc away from being the default by requiring a version", async () => {
    const schema = (await client.listTools()).tools.find((t) => t.name === "write_doc");

    expect(schema?.description).toMatch(/ONLY\s+OP THAT CAN LOSE CONTENT/);
  });
});

describe("memory tools end to end", () => {
  it("round-trips a project through the MCP boundary", async () => {
    expect(await call("create_project", { slug: "graphs", title: "Графы" })).toMatchObject({ ok: true });
    await call("write_doc", {
      project: "graphs",
      doc: "roadmap.md",
      body: "# Roadmap\n\n- [ ] BFS\n",
      summary: "Темы",
    });

    const listed = await call("list_memory", { project: "graphs" });
    expect(listed).toMatchObject({ ok: true, docs: [{ name: "roadmap.md", summary: "Темы", version: 1 }] });

    const read = await call("read_doc", { project: "graphs", doc: "roadmap.md" });
    expect(read).toMatchObject({ ok: true, doc: { version: 1, body: "# Roadmap\n\n- [ ] BFS\n" } });
  });

  it("stamps the instance's actor onto writes without the client saying who it is", async () => {
    await call("create_project", { slug: "graphs", title: "Графы" });
    await call("write_doc", { project: "graphs", doc: "log.md", body: "start\n" });
    await call("append_doc", { project: "graphs", doc: "log.md", text: "день 1", rationale: "запиши прогресс" });

    const history = await call("doc_history", { project: "graphs", doc: "log.md" });

    expect(history.history).toMatchObject([
      { kind: "append", actor: "claude-code@laptop", rationale: "запиши прогресс" },
      { kind: "write", actor: "claude-code@laptop" },
    ]);
  });

  // The point of the envelope: a conflict is data, not an exception. The model
  // gets the number it needs to retry.
  it("returns a version conflict as a result the model can act on", async () => {
    await call("create_project", { slug: "graphs", title: "Графы" });
    await call("write_doc", { project: "graphs", doc: "roadmap.md", body: "- [ ] BFS\n" });
    await call("append_doc", { project: "graphs", doc: "roadmap.md", text: "- [ ] Dijkstra" });

    const conflict = await call("patch_doc", {
      project: "graphs",
      doc: "roadmap.md",
      expected_version: 1,
      edits: [{ old: "- [ ] BFS", new: "- [x] BFS" }],
    });

    expect(conflict).toMatchObject({ ok: false, error: "version_conflict", currentVersion: 2 });
  });

  it("hands back the literal that would have matched after a normalised quote", async () => {
    await call("create_project", { slug: "graphs", title: "Графы" });
    await call("write_doc", { project: "graphs", doc: "goal.md", body: "Цель — пройти графы.\n" });

    const failed = await call("patch_doc", {
      project: "graphs",
      doc: "goal.md",
      expected_version: 1,
      edits: [{ old: "Цель - пройти графы.", new: "Цель — сдать интервью." }],
    });

    expect(failed).toMatchObject({ ok: false, error: "edit_failed", applied: false });
    expect(failed.failures).toMatchObject([{ suggestions: ["Цель — пройти графы."] }]);
  });

  it("recalls across documents and facts and resolves the refs it returns", async () => {
    await call("create_project", { slug: "graphs", title: "Графы" });
    await call("write_doc", {
      project: "graphs",
      doc: "progress.md",
      body: "# Прогресс\n\nЗастрял на релаксации рёбер\n",
    });
    const remembered = await call("remember", { body: "Dijkstra не любит отрицательные веса", tags: ["графы"] });

    const recalled = await call("recall", { query: "релаксация рёбер Dijkstra" });
    const hits = recalled.hits as { ref: string }[];

    expect(hits.some((hit) => hit.ref.startsWith("doc:graphs/progress.md#"))).toBe(true);
    expect(hits.some((hit) => hit.ref.startsWith("fact:"))).toBe(true);

    const factId = (remembered.fact as { id: number }).id;
    expect(await call("get_fact", { id: factId })).toMatchObject({
      ok: true,
      fact: { body: "Dijkstra не любит отрицательные веса" },
    });
  });

  it("reports an unknown project as a result listing the real ones", async () => {
    await call("create_project", { slug: "graphs", title: "Графы" });

    expect(await call("read_doc", { project: "graph", doc: "x.md" })).toMatchObject({
      ok: false,
      error: "project_not_found",
      projects: ["graphs"],
    });
  });
});
