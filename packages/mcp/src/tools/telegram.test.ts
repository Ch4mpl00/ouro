import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { z } from "zod";
import type { TelegramLog } from "../services/telegram";
import { registerTelegramSendTools } from "./telegram";

// send_telegram_message over the MCP boundary with the Telegram HTTP API
// stubbed: a text over 4096 characters goes out as several messages, each
// one logged, and a send that fails partway says what already arrived.

let client: Client;
let logged: { tgMessageId: number | null; text: string }[];
let sentTexts: string[];
let failOnCall: number | null;

beforeEach(async () => {
  logged = [];
  sentTexts = [];
  failOnCall = null;
  vi.stubEnv("TELEGRAM_ASSISTANT_BOT_TOKEN", "test-token");
  vi.stubGlobal("fetch", async (_url: string, init: { body: string }) => {
    const body = JSON.parse(init.body) as { text: string };
    if (sentTexts.length === failOnCall) {
      return new Response(JSON.stringify({ ok: false, description: "Too Many Requests" }), { status: 429 });
    }
    sentTexts.push(body.text);
    const result = { message_id: 500 + sentTexts.length, date: 1_790_000_000, chat: { id: 42, type: "private" } };
    return new Response(JSON.stringify({ ok: true, result }));
  });
  const log: TelegramLog = {
    record: async (input) => {
      logged.push({ tgMessageId: input.tgMessageId, text: input.text });
      return logged.length;
    },
    history: async () => [],
    edited: async () => {},
    lastUpdateId: async () => null,
    setLastUpdateId: async () => {},
  };
  const server = new McpServer({ name: "test", version: "0.0.0" });
  registerTelegramSendTools(server, log);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);
  client = new Client({ name: "test", version: "0.0.0" }, { capabilities: {} });
  await client.connect(clientTransport);
});

afterEach(() => {
  vi.unstubAllEnvs();
  vi.unstubAllGlobals();
});

const digest = `${"• новость ".repeat(300)}\n\n${"• ещё новость ".repeat(200)}`;

const TextContent = z.array(z.object({ type: z.literal("text"), text: z.string() }));

async function send(text: string) {
  const result = await client.callTool({ name: "send_telegram_message", arguments: { chatId: 42, text } });
  const [block] = TextContent.parse(result.content);
  return { isError: result.isError === true, text: block?.text ?? "" };
}

describe("send_telegram_message — long text", () => {
  it("sends a text over 4096 characters as consecutive messages and logs each", async () => {
    expect(digest.length).toBeGreaterThan(4096);
    const out = await send(digest);
    expect(out.isError).toBe(false);
    expect(JSON.parse(out.text)).toMatchObject({ delivered: true, messageId: 501, messageIds: [501, 502], parts: 2 });
    expect(sentTexts.join("")).toBe(digest);
    expect(sentTexts.every((t) => t.length <= 4096)).toBe(true);
    expect(logged).toEqual([
      { tgMessageId: 501, text: sentTexts[0] },
      { tgMessageId: 502, text: sentTexts[1] },
    ]);
  });

  it("keeps a short text a single message", async () => {
    const out = await send("короткое сообщение");
    expect(JSON.parse(out.text)).toMatchObject({ messageId: 501, messageIds: [501], parts: 1 });
  });

  it("reports the parts already delivered when a later part fails, and logs them", async () => {
    failOnCall = 1;
    const out = await send(digest);
    expect(out.isError).toBe(true);
    expect(out.text).toMatch(/after 1 of 2 parts \(delivered message ids: 501\)/);
    expect(logged.map((l) => l.tgMessageId)).toEqual([501]);
  });
});
