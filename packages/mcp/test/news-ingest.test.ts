// Harvesting Telegram channels into the news store: the real provider,
// adapter, Postgres watermark and repository, with Telegram played by a
// stand-in client that answers getMessages the way the API does (newest
// first; with `reverse`, oldest first from just after minId; never more than
// `limit`). Posts arrive — sometimes hundreds between two ticks — and ticks
// sometimes fail for a channel; once the poller has caught up, every text
// post that arrived after the first tick is stored exactly once — and
// nothing from a group chat: Telegram reports a supergroup as a channel too
// (isChannel, plus isGroup), and its messages are a private conversation.

import fc from "fast-check";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createPgClient, type PgClient } from "../src/db/pg/client";
import { createNewsModule, type NewsRepository } from "../src/services/news";
import { createPgWatermarkReader, createTelegramChannelsProvider } from "../src/services/news/providers/telegram-channels/provider";
import { createChannelsAdapter, type ChannelsClient } from "../src/services/news/providers/telegram-channels/userbot-adapter";
import { wordEmbedder } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";

interface Post {
  id: number;
  date: number;
  message: string;
}

// Telegram, as far as the harvester can tell.
const GROUP = "900";

function fakeTelegram(channels: Map<string, Post[]>, failing: Set<string>): ChannelsClient {
  return {
    getDialogs: async () => [
      { isChannel: false, isGroup: false, title: "a friend", entity: { id: 1 } },
      { isChannel: false, isGroup: true, title: "a basic group", entity: { id: 2 } },
      { isChannel: true, isGroup: true, title: "a supergroup", entity: { id: GROUP } },
      ...[...channels.keys()].filter((id) => id !== GROUP).map((id) => ({ isChannel: true, isGroup: false, title: `chan ${id}`, entity: { id, username: `u${id}` } })),
    ],
    getMessages: async (entity, { limit, minId, reverse }) => {
      const id = String(Reflect.get(Object(entity), "id"));
      if (failing.delete(id)) throw new Error("FLOOD_WAIT_7");
      const after = (channels.get(id) ?? []).filter((p) => p.id > (minId ?? 0));
      const ordered = reverse ? [...after].sort((a, b) => a.id - b.id) : [...after].sort((a, b) => b.id - a.id);
      return ordered.slice(0, limit);
    },
  };
}

type Step = { kind: "posts"; channel: number; count: number; mediaEvery: number } | { kind: "tick"; failChannel: number | null };

const step: fc.Arbitrary<Step> = fc.oneof(
  {
    weight: 3,
    arbitrary: fc.record({
      kind: fc.constant("posts" as const),
      channel: fc.integer({ min: 0, max: 2 }),
      count: fc.oneof({ weight: 4, arbitrary: fc.integer({ min: 1, max: 30 }) }, { weight: 1, arbitrary: fc.integer({ min: 201, max: 450 }) }),
      mediaEvery: fc.constantFrom(0, 7),
    }),
  },
  { weight: 3, arbitrary: fc.record({ kind: fc.constant("tick" as const), failChannel: fc.option(fc.integer({ min: 0, max: 2 }), { nil: null }) }) },
);

const available = await databaseAvailable();

describe.skipIf(!available)("Telegram channel harvest into the news store", () => {
  let db: TestDatabase;
  let pgClient: PgClient;
  let pool: pg.Pool;
  let repo: NewsRepository;

  beforeAll(async () => {
    db = await testDatabase();
    process.env.DATABASE_URL = db.url;
    pgClient = createPgClient();
    await pgClient.ensureReady();
    pool = new pg.Pool({ connectionString: db.url });
    repo = createNewsModule({ db: pgClient.db, embeddingProvider: wordEmbedder }).repository;
  }, 180_000);

  afterAll(async () => {
    await pool?.end();
    await pgClient?.close();
    await db?.drop();
  });

  it("stores every text post after the first tick exactly once, bursts and failures included", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(step, { minLength: 1, maxLength: 12, size: "max" }), async (steps) => {
        await pool.query("TRUNCATE news_items RESTART IDENTITY");
        const channels = new Map<string, Post[]>([["100", []], ["200", []], ["300", []], [GROUP, []]]);
        const ids = ["100", "200", "300"];
        const failing = new Set<string>();
        const nextId = new Map([...ids, GROUP].map((c) => [c, 1]));
        let clock = 1_790_000_000;
        const post = (channel: string, count: number, mediaEvery: number) => {
          const list = channels.get(channel)!;
          for (let i = 0; i < count; i++) {
            const id = nextId.get(channel)!;
            nextId.set(channel, id + 1);
            list.push({ id, date: clock++, message: mediaEvery > 0 && id % mediaEvery === 0 ? "" : `post ${channel}/${id}` });
          }
        };
        // History before the harvester ever runs: only the newest 50 of it
        // is wanted (the bootstrap).
        for (const c of ids) post(c, 80, 0);
        // The supergroup chats all along.
        const chat = () => post(GROUP, 5, 0);
        chat();
        const bootstrapCut = new Map(ids.map((c) => [c, nextId.get(c)! - 50]));

        const provider = createTelegramChannelsProvider(
          { userbot: createChannelsAdapter({ hasSession: async () => true, client: async () => fakeTelegram(channels, failing) }), getWatermark: createPgWatermarkReader(pgClient.db) },
          { interChannelDelayMs: 0 },
        );
        const tick = async () => {
          const items = await provider.fetch();
          if (items.length > 0) await repo.save(items);
        };
        await tick(); // the first tick bootstraps every channel

        for (const s of steps) {
          if (s.kind === "posts") {
            if (s.count > 200) witnessed.add("a burst of more than one tick's worth");
            post(ids[s.channel]!, s.count, s.mediaEvery);
            chat();
          } else {
            if (s.failChannel !== null) {
              failing.add(ids[s.failChannel]!);
              witnessed.add("a channel fails a tick");
            }
            await tick();
          }
        }
        for (let i = 0; i < 6; i++) await tick(); // catch up

        const { rows } = await pool.query("SELECT external_id, body FROM news_items WHERE source = 'channel'");
        const stored = rows.map((r) => String(r.external_id));
        expect(new Set(stored).size, "no post stored twice").toBe(stored.length);
        expect(stored.filter((id) => id.startsWith(`${GROUP}:`)), "nothing from a group chat").toEqual([]);
        for (const r of rows) expect(String(r.body).trim().length, "only text posts are stored").toBeGreaterThan(0);
        const want = ids.flatMap((c) =>
          channels
            .get(c)!
            .filter((p) => p.message !== "" && p.id >= bootstrapCut.get(c)!)
            .map((p) => `${c}:${p.id}`),
        );
        expect(stored.sort(), "every text post since the bootstrap, and nothing older").toEqual(want.sort());
      }),
      { numRuns: 20 },
    );
    expect([...witnessed].sort()).toEqual(["a burst of more than one tick's worth", "a channel fails a tick"]);
  }, 600_000);
});
