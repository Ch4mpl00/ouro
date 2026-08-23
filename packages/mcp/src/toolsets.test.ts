import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { describe, expect, it } from "vitest";
import {
  DEFAULT_TOOLSETS,
  parseToolsets,
  registerToolsets,
  type ToolsetDeps,
  type ToolsetName,
} from "./toolsets";

// Registration-only test: the stubs are never called, they just satisfy the
// repositories the registrars take in their signature.
function stubDeps(): ToolsetDeps {
  const unused = () => {
    throw new Error("tool handler must not run in a registration test");
  };
  return {
    news: {
      save: unused,
      upsert: unused,
      findByExternalId: unused,
      list: unused,
      search: unused,
      searchMany: unused,
      embedMissingBatch: unused,
    },
    knowledge: {
      addNote: unused,
      findNotes: unused,
      embedMissingBatch: unused,
    },
    skills: {
      listSkills: unused,
      readSkill: unused,
    },
    memory: {
      createProject: unused,
      listProjects: unused,
      listDocs: unused,
      readDoc: unused,
      writeDoc: unused,
      appendDoc: unused,
      patchDoc: unused,
      history: unused,
      revert: unused,
      remember: unused,
      getFact: unused,
      updateFact: unused,
      recall: unused,
    },
    memoryActor: "test",
  };
}

async function listToolNames(names: readonly ToolsetName[]): Promise<string[]> {
  const server = new McpServer({ name: "test", version: "0.0.0" });
  registerToolsets(server, stubDeps(), names);

  const [clientT, serverT] = InMemoryTransport.createLinkedPair();
  await server.connect(serverT);
  const client = new Client({ name: "test", version: "0.0.0" }, { capabilities: {} });
  await client.connect(clientT);

  const { tools } = await client.listTools();
  await client.close();
  await server.close();
  return tools.map((t) => t.name).sort();
}

describe("parseToolsets", () => {
  it("treats absent/empty/whitespace as the full default surface", () => {
    for (const raw of [undefined, "", "   ", ",, ,"]) {
      expect(parseToolsets(raw)).toEqual({ names: DEFAULT_TOOLSETS, restricted: false });
    }
  });

  it("parses a restricted list, trimming and de-duplicating", () => {
    expect(parseToolsets(" news-read , telegram-send ,news-read")).toEqual({
      names: ["news-read", "telegram-send"],
      restricted: true,
    });
  });

  it("rejects an unknown toolset instead of silently serving a different surface", () => {
    expect(() => parseToolsets("news-read,telegramm")).toThrow(/unknown toolset\(s\) telegramm/);
  });
});

describe("registerToolsets", () => {
  // The contract the ChatGPT tunnel deployment depends on: exactly these six
  // tools, and nothing from Gmail / Monobank / fs / signals / scheduler /
  // userbot / dreaming / knowledge / pdf.
  // Pins docker-compose's MCP_TOOLSETS for mcp-tunnel exactly. If this list
  // and that env var drift apart, ChatGPT silently gets a surface nobody
  // decided on — in either direction.
  it("exposes exactly the tunnel surface for news-read + telegram-send + skills + memory", async () => {
    expect(await listToolNames(["news-read", "telegram-send", "skills", "memory"])).toEqual([
      "append_doc",
      "create_project",
      "doc_history",
      "fetch_article",
      "get_fact",
      "list_memory",
      "list_news",
      "list_skills",
      "patch_doc",
      "read_doc",
      "read_skill",
      "recall",
      "remember",
      "revert_patch",
      "search_news",
      "send_telegram_message",
      "update_fact",
      "write_doc",
    ]);
  });

  // Everything a third-party client can reach must be safe to hand out: no
  // personal account, no signal delivery, no arbitrary fetch.
  it("keeps the tunnel surface clear of the tools that must never leave the droplet", async () => {
    const tunnel = await listToolNames(["news-read", "telegram-send", "skills", "memory"]);

    for (const forbidden of [
      "get_next_signal",
      "list_nashdom_mails",
      "list_monobank_transactions",
      "read_file",
      "fetch_url",
      "schedule_task",
      "get_telegram_chat_history",
    ]) {
      expect(tunnel).not.toContain(forbidden);
    }
  });

  it("keeps shared tunnel capabilities in the default surface", async () => {
    const all = await listToolNames(DEFAULT_TOOLSETS);
    const restricted = await listToolNames(["news-read", "telegram-send", "skills"]);
    expect(
      restricted
        .filter((name) => name !== "list_skills" && name !== "read_skill")
        .every((name) => all.includes(name)),
    ).toBe(true);
    // The agent already owns synthetic tools with these names. Keep the
    // MCP-backed export scoped to the ChatGPT tunnel to avoid duplicates.
    expect(all).not.toContain("list_skills");
    expect(all).not.toContain("read_skill");
    expect(all).toContain("get_next_signal");
    expect(all).toContain("get_telegram_chat_history");
  });
});
