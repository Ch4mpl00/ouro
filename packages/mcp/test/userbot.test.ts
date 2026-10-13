// list_userbot_dialogs over MCP, with Telegram's dialog list played by a
// stand-in that answers getDialogs the way gramjs does: most recent first,
// at most `limit` when one is given, and the flags gramjs sets — a
// supergroup is `isChannel` AND `isGroup`, a basic group only `isGroup`, a
// broadcast channel only `isChannel`.
//
// Rules: each dialog is typed by what it is (a supergroup is a group); a
// type filter returns the first `limit` dialogs of that type, in Telegram's
// order — not whatever survives filtering the first `limit` of all.

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import fc from "fast-check";
import { beforeAll, describe, expect, it } from "vitest";
import { registerUserbotTools } from "../src/tools/userbot";
import { call } from "../specs/support/environment";

type Kind = "user" | "basicGroup" | "supergroup" | "channel";
const TYPE: Record<Kind, string> = { user: "user", basicGroup: "group", supergroup: "group", channel: "channel" };

let dialogs: Kind[] = [];
let client: Client;

beforeAll(async () => {
  const server = new McpServer({ name: "userbot", version: "0" });
  registerUserbotTools(server, {
    client: async () => ({
      getDialogs: async ({ limit }) =>
        dialogs.slice(0, limit ?? dialogs.length).map((kind, i) => ({
          id: 1000 + i,
          isUser: kind === "user",
          isGroup: kind === "basicGroup" || kind === "supergroup",
          isChannel: kind === "supergroup" || kind === "channel",
          title: `${kind} ${i}`,
          entity: { id: 1000 + i, username: kind === "channel" ? `chan${i}` : undefined },
          unreadCount: i % 4,
        })),
    }),
  });
  const [ct, st] = InMemoryTransport.createLinkedPair();
  await server.connect(st);
  client = new Client({ name: "t", version: "0" });
  await client.connect(ct);
});

describe("list_userbot_dialogs", () => {
  it("types each dialog by what it is and filters before limiting", async () => {
    await fc.assert(
      fc.asyncProperty(
        fc.array(fc.constantFrom<Kind>("user", "basicGroup", "supergroup", "channel"), { maxLength: 60 }),
        fc.option(fc.constantFrom("channel", "group", "user", "all"), { nil: undefined }),
        fc.option(fc.integer({ min: 1, max: 30 }), { nil: undefined }),
        async (kinds, type, limit) => {
          dialogs = kinds;
          const args: Record<string, unknown> = {};
          if (type !== undefined) args.type = type;
          if (limit !== undefined) args.limit = limit;
          const out = await call(client, "list_userbot_dialogs", args);
          if (out.kind !== "ok") throw new Error(JSON.stringify(out));
          const got = Reflect.get(Object(out.value), "dialogs");
          if (!Array.isArray(got)) throw new Error("no dialogs");
          const want = kinds
            .map((kind, i) => ({ id: String(1000 + i), type: TYPE[kind] }))
            .filter((d) => type === undefined || type === "all" || d.type === type)
            .slice(0, limit ?? 100);
          const where = JSON.stringify({ kinds, type, limit });
          expect(got.map((d) => ({ id: String(Reflect.get(Object(d), "id")), type: Reflect.get(Object(d), "type") })), where).toEqual(want);
          expect(Reflect.get(Object(out.value), "count"), where).toBe(want.length);
        },
      ),
      { numRuns: 300 },
    );
  });
});
