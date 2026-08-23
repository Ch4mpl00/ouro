import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { describe, expect, it } from "vitest";
import type { SkillCatalog, SkillDocument, SkillSummary } from "../services/skills";
import { registerSkillsTools } from "./skills";

function textResult(content: unknown): unknown {
  if (!Array.isArray(content)) throw new Error("MCP result content must be an array");
  const block = content.find(
    (item): item is { type: "text"; text: string } =>
      typeof item === "object" &&
      item !== null &&
      "type" in item &&
      item.type === "text" &&
      "text" in item &&
      typeof item.text === "string",
  );
  if (!block) throw new Error("MCP result must contain text");
  return JSON.parse(block.text);
}

describe("skills MCP tools", () => {
  it("lists filenames and reads one file through MCP", async () => {
    const summary: SkillSummary = {
      id: "telegram",
      name: "telegram",
      fileName: "telegram.md",
      title: "Telegram",
      description: "Handle Telegram signals.",
      tools: "*",
      source: "default",
      sizeBytes: 42,
      modifiedAt: "2026-08-23T00:00:00.000Z",
      patched: true,
    };
    const document: SkillDocument = {
      ...summary,
      content: "---\ntools: *\n---\n\n# Telegram\n",
      patch: "Always reply in Russian.\n",
      effectiveInstructions:
        "# Telegram\n\n<!-- improver-patch -->\nAlways reply in Russian.\n",
    };
    const catalog: SkillCatalog = {
      listSkills: async () => [summary],
      readSkill: async (fileName) => fileName === summary.fileName ? document : null,
    };
    const server = new McpServer({ name: "test", version: "0.0.0" });
    registerSkillsTools(server, catalog);
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    await server.connect(serverTransport);
    const client = new Client({ name: "test", version: "0.0.0" }, { capabilities: {} });
    await client.connect(clientTransport);

    const listed = textResult((await client.callTool({ name: "list_skills", arguments: {} })).content);
    expect(listed).toEqual({ count: 1, skills: [summary] });

    const read = textResult(
      (await client.callTool({
        name: "read_skill",
        arguments: { fileName: "telegram.md" },
      })).content,
    );
    // The patch must reach the caller as its own field — a reader that only
    // gets `content` would be looking at instructions the agent no longer runs.
    expect(read).toEqual({
      found: true,
      fileName: "telegram.md",
      content: document.content,
      patch: document.patch,
      effectiveInstructions: document.effectiveInstructions,
      metadata: summary,
    });

    await client.close();
    await server.close();
  });
});
