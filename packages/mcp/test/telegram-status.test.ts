// The live status bubble (telegram_send_status) and the typing keep-alive
// over MCP, against the fake Bot API. Generated sequences of status calls —
// some fired at once, as an agent's parallel tool calls are — with pauses
// that let the animation run: the chat holds at most one bubble per status
// id, showing the latest text (plus its animation dots), and an emptied
// status leaves nothing behind.

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import fc from "fast-check";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { TelegramLog } from "../src/services/telegram";
import { registerTelegramTools } from "../src/tools/telegram";
import { call } from "../specs/support/environment";
import { installFakeTelegram, type FakeTelegram } from "./support/fake-telegram";

const CHAT = 4242;
let fake: FakeTelegram;
let client: Client;

const unused = (): never => {
  throw new Error("the chat log is not part of status / typing");
};
const log: TelegramLog = { record: unused, history: unused, edited: unused, lastUpdateId: unused, setLastUpdateId: unused };

beforeAll(async () => {
  fake = installFakeTelegram();
  process.env.TELEGRAM_ASSISTANT_BOT_TOKEN = fake.token;
  process.env.TELEGRAM_DEFAULT_CHAT_ID = String(CHAT);
  const server = new McpServer({ name: "t", version: "0" });
  registerTelegramTools(server, log);
  const [ct, st] = InMemoryTransport.createLinkedPair();
  await server.connect(st);
  client = new Client({ name: "c", version: "0" });
  await client.connect(ct);
});
afterAll(async () => {
  await client?.close();
  await fake?.close();
  delete process.env.TELEGRAM_ASSISTANT_BOT_TOKEN;
  delete process.env.TELEGRAM_DEFAULT_CHAT_ID;
});

type Op = { kind: "status"; id: string; text: string; parallel: number } | { kind: "wait"; ms: number };
const text = fc.oneof(
  { weight: 5, arbitrary: fc.constantFrom("Ищу новости", "Читаю PDF", "Собираю дайджест", "step 2") },
  { weight: 2, arbitrary: fc.constantFrom("", "   ") },
  { weight: 1, arbitrary: fc.constant("я".repeat(4096)) },
);
const op: fc.Arbitrary<Op> = fc.oneof(
  { weight: 5, arbitrary: fc.record({ kind: fc.constant("status" as const), id: fc.constantFrom("status:1", "status:2"), text, parallel: fc.constantFrom(1, 1, 1, 2, 3) }) },
  { weight: 1, arbitrary: fc.record({ kind: fc.constant("wait" as const), ms: fc.constantFrom(300, 1_300) }) },
);

// The bubbles in the chat, by message id, with their current text.
const bubbles = () => [...fake.chat.values()].filter((m) => m.chatId === CHAT);
const undotted = (s: string) => s.replace(/\.{0,3}$/, "");

describe("telegram_send_status", () => {
  it("keeps one bubble per status id, showing the latest text, and removes it when emptied", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(op, { minLength: 1, maxLength: 10, size: "max" }), async (ops) => {
        // Start from no status at all: empty both ids, then clear the chat.
        for (const id of ["status:1", "status:2"]) await call(client, "telegram_send_status", { id, text: "" });
        fake.chat.clear();
        const latest = new Map<string, string>();
        for (const o of ops) {
          if (o.kind === "wait") {
            await new Promise((r) => setTimeout(r, o.ms));
            if (o.ms > 1_000) witnessed.add("the animation ran");
          } else {
            const outcomes = await Promise.all(
              Array.from({ length: o.parallel }, () => call(client, "telegram_send_status", { id: o.id, text: o.text })),
            );
            if (o.parallel > 1) witnessed.add("parallel calls for one id");
            for (const out of outcomes) expect(out.kind, `${JSON.stringify(o)}: ${JSON.stringify(out).slice(0, 200)}`).toBe("ok");
            if (o.text.trim() === "") latest.delete(o.id);
            else latest.set(o.id, o.text.trim());
          }
          // Let in-flight animation edits land before looking.
          await new Promise((r) => setTimeout(r, 50));
          const shown = bubbles();
          expect(shown.length, `${JSON.stringify(o)}: one bubble per live status, no orphans`).toBe(latest.size);
          const texts = shown.map((b) => undotted(b.text)).sort();
          expect(texts, `${JSON.stringify(o)}: each bubble shows its latest text`).toEqual([...latest.values()].sort());
          for (const b of shown) expect(b.text.length, "within Telegram's limit").toBeLessThanOrEqual(4096);
        }
      }),
      { numRuns: 25 },
    );
    expect([...witnessed].sort()).toEqual(["parallel calls for one id", "the animation ran"]);
  }, 300_000);

  it("animates a long status without going over the limit", async () => {
    await call(client, "telegram_send_status", { id: "status:long", text: "я".repeat(4096) });
    const before = fake.sent.length;
    await new Promise((r) => setTimeout(r, 2_300));
    const bubble = bubbles().find((b) => b.text.startsWith("я"));
    expect(bubble?.text.length).toBeLessThanOrEqual(4096);
    expect(fake.sent.length).toBe(before);
    await call(client, "telegram_send_status", { id: "status:long", text: "" });
  }, 30_000);
});

describe("start_typing", () => {
  it("keeps the indicator alive until a message goes to that chat, then stops", async () => {
    fake.actions.length = 0;
    expect((await call(client, "start_typing", { chatId: CHAT })).kind).toBe("ok");
    await new Promise((r) => setTimeout(r, 4_500));
    const kept = fake.actions.filter((a) => a.chatId === CHAT).length;
    expect(kept, "sent at start and refreshed").toBeGreaterThanOrEqual(2);
    // Delivered (the chat log here refuses writes, which only adds a logError).
    expect((await call(client, "send_telegram_message", { text: "done" })).kind).toBe("ok");
    const after = fake.actions.length;
    await new Promise((r) => setTimeout(r, 4_500));
    expect(fake.actions.length, "no refresh after the message").toBe(after);
  }, 30_000);
});
