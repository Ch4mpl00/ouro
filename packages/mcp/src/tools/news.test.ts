import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { beforeEach, describe, expect, it } from "vitest";
import { createGuardedFetch } from "../services/fetch";
import type { NewsRepository, SearchManyOpts } from "../services/news/core/repository";
import { registerNewsTools } from "./news";

// search_news's `k` cap at the MCP boundary: 150, so the digest's decide
// filter can see a wide net, and nothing above it.

const unused = async (): Promise<never> => {
  throw new Error("not used by this test");
};

let client: Client;
let calls: SearchManyOpts[];

beforeEach(async () => {
  calls = [];
  const news: NewsRepository = {
    save: unused,
    upsert: unused,
    findByExternalId: unused,
    list: unused,
    search: unused,
    searchMany: async (opts) => {
      calls.push(opts);
      return [];
    },
    embedMissingBatch: unused,
  };
  const server = new McpServer({ name: "test", version: "0.0.0" });
  registerNewsTools(server, news, createGuardedFetch());
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);
  client = new Client({ name: "test", version: "0.0.0" }, { capabilities: {} });
  await client.connect(clientTransport);
});

describe("search_news k", () => {
  it("accepts up to 150 and passes it through", async () => {
    const result = await client.callTool({ name: "search_news", arguments: { queries: ["a", "b"], k: 150 } });
    expect(result.isError).toBeFalsy();
    expect(calls.map((c) => c.k)).toEqual([150]);
  });

  it("rejects 151 before it reaches the store", async () => {
    const result = await client.callTool({ name: "search_news", arguments: { queries: ["a"], k: 151 } });
    expect(result.isError).toBe(true);
    expect(calls).toEqual([]);
  });
});
