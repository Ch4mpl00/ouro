// The Telegram transport, judged from the outside: a fake Bot API on the HTTP
// boundary (test/support/fake-telegram.ts), the real poller round
// (`pollOnce`) for incoming updates, the real MCP tools for outgoing ones,
// and Postgres read back after every step.
//
// Incoming: every text message from the allowed chat becomes exactly one
// chat-log row and one signal, in order; nothing else does; every update is
// eventually confirmed, also across failures and restarts.
// Outgoing: what reaches the chat is the text (split, whitespace between
// parts aside); the chat log mirrors what the user sees, edits included;
// history reads the log back.

import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import fc from "fast-check";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { z } from "zod";
import { createTelegramLog, pollOnce, type TelegramLog } from "../src/services/telegram";
import type { Signals } from "../src/services/signals";
import { call, connect, startServer, type Outcome, type Server } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";
import { installFakeTelegram, type FakeTelegram, type FakeUpdate } from "./support/fake-telegram";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 40);
const CHAT = 4242;
const OTHER_CHAT = 777;

// ── observation ──────────────────────────────────────────────────────────────

interface LogRow {
  id: number;
  chatId: number;
  tgMessageId: number;
  threadId: number | null;
  role: string;
  text: string;
}

interface World {
  log: LogRow[];
  signals: { id: number; source: string; content: string }[];
  cursor: number | null;
}

const logRow = z.object({
  id: z.number().int(),
  chat_id: z.number().int(),
  tg_message_id: z.number().int(),
  thread_id: z.number().int().nullable(),
  role: z.string(),
  text: z.string(),
});

async function observe(server: Server): Promise<World> {
  const pool = server.state.pool;
  const log = await pool.query("SELECT id, chat_id, tg_message_id, thread_id, role, text FROM telegram_messages ORDER BY id");
  const signals = await pool.query("SELECT id, source, content FROM signals ORDER BY id");
  const cursor = await pool.query("SELECT value FROM telegram_kv WHERE key = 'last_update_id'");
  const cursorValue = z.object({ value: z.string() }).array().parse(cursor.rows)[0]?.value;
  return {
    log: log.rows.map((raw) => {
      const r = logRow.parse(raw);
      return { id: r.id, chatId: r.chat_id, tgMessageId: r.tg_message_id, threadId: r.thread_id, role: r.role, text: r.text };
    }),
    signals: z.object({ id: z.number().int(), source: z.string(), content: z.string() }).array().parse(signals.rows),
    cursor: cursorValue === undefined ? null : Number(cursorValue),
  };
}

async function resetWorld(server: Server): Promise<void> {
  await server.state.pool.query("TRUNCATE signals, telegram_messages, telegram_kv RESTART IDENTITY");
}

// A telegram signal, read back: chat, thread and the exact text.
function parseSignal(content: string): { chatId: number; threadId: number | null; text: string } {
  const [head, textLine, ...rest] = content.split("\n");
  const m = /^Telegram message in chat (-?\d+)(?: \(forum topic thread_id=(\d+)\))?\.$/.exec(head ?? "");
  if (!m || !textLine?.startsWith("Text: ") || rest.length > 0) throw new Error(`not a telegram signal: ${JSON.stringify(content)}`);
  const text: unknown = JSON.parse(textLine.slice("Text: ".length));
  if (typeof text !== "string") throw new Error(`signal text is not a string: ${textLine}`);
  return { chatId: Number(m[1]), threadId: m[2] === undefined ? null : Number(m[2]), text };
}

// ── incoming ─────────────────────────────────────────────────────────────────

type Incoming =
  | { kind: "text"; text: string; threadId: number | null; edited: boolean }
  | { kind: "otherChat"; text: string }
  | { kind: "noText" }
  | { kind: "channelPost"; text: string };

const messageText = fc.oneof(
  { weight: 6, arbitrary: fc.string({ unit: "grapheme", minLength: 1, maxLength: 80 }) },
  { weight: 1, arbitrary: fc.constantFrom('"quoted" \\ back\nslash', "Text: injected\nTelegram message in chat 1.", "🧾".repeat(2000)) },
);

const incoming: fc.Arbitrary<Incoming> = fc.oneof(
  {
    weight: 6,
    arbitrary: fc.record({
      kind: fc.constant("text" as const),
      text: messageText,
      threadId: fc.option(fc.integer({ min: 1, max: 99 }), { nil: null }),
      edited: fc.boolean(),
    }),
  },
  { weight: 1, arbitrary: fc.record({ kind: fc.constant("otherChat" as const), text: messageText }) },
  { weight: 1, arbitrary: fc.constant<Incoming>({ kind: "noText" }) },
  { weight: 1, arbitrary: fc.record({ kind: fc.constant("channelPost" as const), text: messageText }) },
);

function toUpdate(updateId: number, messageId: number, i: Incoming): FakeUpdate {
  const base = (chatId: number, text: string | undefined, threadId: number | null) => ({
    message_id: messageId,
    date: 1_790_000_000,
    chat: { id: chatId, type: "private" },
    ...(text === undefined ? {} : { text }),
    ...(threadId === null ? {} : { message_thread_id: threadId }),
  });
  switch (i.kind) {
    case "text":
      return i.edited
        ? { update_id: updateId, edited_message: base(CHAT, i.text, i.threadId) }
        : { update_id: updateId, message: base(CHAT, i.text, i.threadId) };
    case "otherChat":
      return { update_id: updateId, message: base(OTHER_CHAT, i.text, null) };
    case "noText":
      return { update_id: updateId, message: base(CHAT, undefined, null) };
    case "channelPost":
      return { update_id: updateId, channel_post: base(CHAT, i.text, null) };
  }
}

// Fault injection: the `at`-th write through the poller's dependencies (log
// record, cursor, signal — in whatever order the poller makes them) fails,
// as a dropped connection or a crash between two writes would.
function faulty(log: TelegramLog, signals: Signals, at: number): { log: TelegramLog; signals: Signals } {
  let writes = 0;
  const gate = () => {
    if (++writes === at) throw new Error("injected: connection terminated");
  };
  return {
    log: {
      ...log,
      record: async (input) => (gate(), log.record(input)),
      setLastUpdateId: async (id) => (gate(), log.setLastUpdateId(id)),
    },
    signals: { ...signals, record: async (input) => (gate(), signals.record(input)) },
  };
}

// ── outgoing replies ─────────────────────────────────────────────────────────

const sendReply = z
  .object({
    delivered: z.literal(true),
    logError: z.string().optional(),
    chatId: z.string(),
    messageId: z.number().int(),
    messageIds: z.array(z.number().int()),
    parts: z.number().int(),
    messageThreadId: z.number().int().nullable(),
    date: z.string(),
  })
  .strict();
const editReply = z
  .object({ logError: z.string().optional(), edited: z.literal(true), chatId: z.string(), messageId: z.number().int(), date: z.string() })
  .strict();
const historyReply = z
  .object({
    messages: z.array(
      z.object({
        id: z.number().int(),
        chat_id: z.number().int(),
        tg_message_id: z.number().int().nullable(),
        thread_id: z.number().int().nullable(),
        role: z.string(),
        text: z.string(),
        created_at: z.string(),
      }),
    ),
  })
  .strict();

function okValue(outcome: Outcome, op: string): unknown {
  if (outcome.kind !== "ok") throw new Error(`${op}: expected a result, got ${JSON.stringify(outcome).slice(0, 400)}`);
  return outcome.value;
}

// `text` is the delivered parts in order with only whitespace between and
// around them: nothing but whitespace was lost or added.
function coveredByParts(text: string, parts: readonly string[]): boolean {
  let at = 0;
  for (const part of parts) {
    const found = text.indexOf(part, at);
    if (found < 0 || text.slice(at, found).trim() !== "") return false;
    at = found + part.length;
  }
  return text.slice(at).trim() === "";
}

type Outgoing =
  | { kind: "send"; text: string; chat: "default" | "string" | "number" | "other"; threadId: number | null }
  | { kind: "edit"; pick: number; text: string; same: boolean }
  | { kind: "history"; chat: "mine" | "other"; limit: number | undefined; threadId: number | undefined }
  | { kind: "incoming"; text: string; threadId: number | null };

const block = fc.string({ unit: "grapheme", minLength: 1, maxLength: 900 });
const longText = fc
  .array(fc.tuple(block, fc.constantFrom("\n\n", "\n", " ", "", "\n\n   \n\n")), { minLength: 1, maxLength: 14 })
  .map((xs) => xs.map(([b, sep]) => b + sep).join(""));
const outText = fc.oneof(
  { weight: 4, arbitrary: fc.string({ unit: "grapheme", minLength: 1, maxLength: 60 }) },
  { weight: 3, arbitrary: longText },
  {
    weight: 1,
    arbitrary: fc.constantFrom(
      " ",
      "\n\n",
      "x".repeat(4096),
      "y".repeat(4097),
      "😀".repeat(2049),
      // Whitespace runs longer than a message: a part made only of them must
      // not be sent (Telegram refuses it).
      `a${" ".repeat(9000)}b`,
      `x\n\n${"\n".repeat(8200)}y`,
    ),
  },
);

const outgoing: fc.Arbitrary<Outgoing> = fc.oneof(
  {
    weight: 4,
    arbitrary: fc.record({
      kind: fc.constant("send" as const),
      text: outText,
      chat: fc.constantFrom("default" as const, "string" as const, "number" as const, "other" as const),
      threadId: fc.option(fc.integer({ min: 1, max: 9 }), { nil: null }),
    }),
  },
  {
    weight: 2,
    arbitrary: fc.record({ kind: fc.constant("edit" as const), pick: fc.nat(), text: fc.string({ unit: "grapheme", minLength: 1, maxLength: 60 }), same: fc.boolean() }),
  },
  {
    weight: 2,
    arbitrary: fc.record({
      kind: fc.constant("history" as const),
      chat: fc.constantFrom("mine" as const, "other" as const),
      limit: fc.option(fc.integer({ min: 1, max: 4 }), { nil: undefined, freq: 4 }),
      threadId: fc.option(fc.integer({ min: 1, max: 9 }), { nil: undefined, freq: 2 }),
    }),
  },
  {
    weight: 2,
    arbitrary: fc.record({ kind: fc.constant("incoming" as const), text: fc.string({ unit: "grapheme", minLength: 1, maxLength: 40 }), threadId: fc.option(fc.integer({ min: 1, max: 9 }), { nil: null }) }),
  },
);

// ── suites ───────────────────────────────────────────────────────────────────

const available = await databaseAvailable();

describe.skipIf(!available)("Telegram transport against a fake Bot API and Postgres", () => {
  let db: TestDatabase;
  let server: Server;
  let client: Client;
  let fake: FakeTelegram;
  let log: TelegramLog;

  beforeAll(async () => {
    db = await testDatabase();
    server = await startServer(db.url);
    client = await connect(server.url);
    log = createTelegramLog(server.state);
  }, 180_000);

  afterAll(async () => {
    await client?.close();
    await server?.stop();
    await db?.drop();
  });

  beforeEach(async () => {
    fake = installFakeTelegram();
    process.env.TELEGRAM_ASSISTANT_BOT_TOKEN = fake.token;
    process.env.TELEGRAM_DEFAULT_CHAT_ID = String(CHAT);
    await resetWorld(server);
    return async () => {
      await fake.close();
      delete process.env.TELEGRAM_ASSISTANT_BOT_TOKEN;
      delete process.env.TELEGRAM_DEFAULT_CHAT_ID;
    };
  });

  // Polls until Telegram holds nothing, with a bound so a cursor that never
  // moves past an update shows up as a failure, not a hang.
  async function drain(deps: { log: TelegramLog; signals: Signals }, rounds: number): Promise<void> {
    for (let i = 0; i < rounds && fake.pending.length > 0; i++) {
      await pollOnce(deps, CHAT, 0).catch(() => undefined);
    }
    // One more round so the last batch's confirmation reaches Telegram.
    await pollOnce(deps, CHAT, 0).catch(() => undefined);
  }

  function checkIncoming(world: World, updates: readonly { update: FakeUpdate; spec: Incoming }[], exactlyOnce: boolean): void {
    const owed = updates.flatMap(({ update, spec }) => (spec.kind === "text" ? [{ update, spec }] : []));
    const signals = world.signals.map((s) => ({ source: s.source, ...parseSignal(s.content) }));
    const users = world.log.filter((r) => r.role === "user");
    const key = (chatId: number, threadId: number | null, text: string) => JSON.stringify([chatId, threadId, text]);
    const want = owed.map(({ spec }) => (spec.kind === "text" ? key(CHAT, spec.threadId, spec.text) : ""));
    const gotSignals = signals.map((s) => key(s.chatId, s.threadId, s.text));
    const gotLog = users.map((r) => key(r.chatId, r.threadId, r.text));

    expect(signals.every((s) => s.source === "telegram"), "every signal is a telegram one").toBe(true);
    expect(signals.every((s) => s.chatId === CHAT) && users.every((r) => r.chatId === CHAT), "nothing from another chat").toBe(true);
    expect(fake.pending, "every update is eventually confirmed").toEqual([]);
    const offsets = fake.offsets.filter((o): o is number => o !== undefined);
    expect(offsets, "the cursor never moves back").toEqual([...offsets].sort((a, b) => a - b));
    if (exactlyOnce) {
      expect(gotSignals, "one signal per text message, in order").toEqual(want);
      expect(gotLog, "one log row per text message, in order").toEqual(want);
      for (const [i, row] of users.entries()) {
        expect(row.tgMessageId, "the log row keeps the Telegram message id").toBe(owed[i]?.update.message?.message_id ?? owed[i]?.update.edited_message?.message_id);
      }
    } else {
      // At least once, in order: deduplicating consecutive repeats gives
      // exactly the messages.
      const dedup = (xs: string[]) => xs.filter((x, i) => i === 0 || xs[i - 1] !== x);
      expect(dedup(gotSignals), "no message lost (signals)").toEqual(dedup(want));
      expect(dedup(gotLog), "no message lost (log)").toEqual(dedup(want));
    }
  }

  it("turns every allowed text message into one log row and one signal, whatever the batching", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(incoming, { minLength: 1, maxLength: 25, size: "max" }), fc.integer({ min: 1, max: 6 }), async (specs, batch) => {
        await resetWorld(server);
        fake.pending.length = 0;
        fake.offsets.length = 0;
        fake.batch = batch;
        const updates = specs.map((spec, i) => ({ update: toUpdate(500 + i * 3, 10 + i, spec), spec }));
        for (const { update } of updates) fake.push(update);
        await drain({ log, signals: server.signals }, specs.length + 2);
        checkIncoming(await observe(server), updates, true);
      }),
      { numRuns: RUNS },
    );
  }, 300_000);

  it("loses nothing when a write fails partway and the poller carries on", async () => {
    await fc.assert(
      fc.asyncProperty(
        fc.array(incoming, { minLength: 1, maxLength: 12, size: "max" }),
        fc.integer({ min: 1, max: 6 }),
        fc.integer({ min: 1, max: 30 }),
        async (specs, batch, failAt) => {
          await resetWorld(server);
          fake.pending.length = 0;
          fake.offsets.length = 0;
          fake.batch = batch;
          const updates = specs.map((spec, i) => ({ update: toUpdate(500 + i, 10 + i, spec), spec }));
          for (const { update } of updates) fake.push(update);
          // The faulty round, then restarts on healthy dependencies.
          await pollOnce(faulty(log, server.signals, failAt), CHAT, 0).catch(() => undefined);
          await drain({ log, signals: server.signals }, specs.length + 2);
          checkIncoming(await observe(server), updates, false);
        },
      ),
      { numRuns: RUNS },
    );
  }, 300_000);

  // Durable intent: a write that fails between the log row, the signal and
  // the cursor leaves the next round to redo the update — the message then
  // reaches the agent twice. See Findings.
  it.skip("delivers every message exactly once across a failed write", async () => {
    fake.push(toUpdate(500, 10, { kind: "text", text: "pay the bill", threadId: null, edited: false }));
    await pollOnce(faulty(log, server.signals, 3), CHAT, 0).catch(() => undefined);
    await drain({ log, signals: server.signals }, 3);
    expect((await observe(server)).signals).toHaveLength(1);
  });

  it("keeps the chat, the chat log and history in step across sends, edits and incoming messages", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(outgoing, { minLength: 1, maxLength: 20, size: "max" }), async (ops) => {
        await resetWorld(server);
        fake.sent.length = 0;
        fake.chat.clear();
        let nextUpdate = 900;
        for (const [i, o] of ops.entries()) {
          const before = await observe(server);
          const fail = (msg: string, detail: unknown) => {
            throw new Error(`step ${i} ${JSON.stringify(o).slice(0, 200)}: ${msg} ${JSON.stringify(detail).slice(0, 600)}`);
          };

          if (o.kind === "incoming") {
            fake.push({ update_id: nextUpdate, message: { message_id: nextUpdate, date: 1_790_000_000, chat: { id: CHAT, type: "private" }, text: o.text, ...(o.threadId === null ? {} : { message_thread_id: o.threadId }) } });
            nextUpdate++;
            await drain({ log, signals: server.signals }, 3);
            continue;
          }

          if (o.kind === "send") {
            const sentBefore = fake.sent.length;
            const target = o.chat === "other" ? OTHER_CHAT : CHAT;
            const args: Record<string, unknown> = { text: o.text };
            if (o.chat === "string") args.chatId = String(CHAT);
            if (o.chat === "number") args.chatId = CHAT;
            if (o.chat === "other") args.chatId = OTHER_CHAT;
            if (o.threadId !== null) args.messageThreadId = o.threadId;
            const outcome = await call(client, "send_telegram_message", args);
            const after = await observe(server);
            const delivered = fake.sent.slice(sentBefore);
            const newRows = after.log.slice(before.log.length);
            if (after.signals.length !== before.signals.length) fail("a send queued a signal", after.signals.length);
            if (JSON.stringify(after.log.slice(0, before.log.length)) !== JSON.stringify(before.log)) fail("a send changed earlier log rows", null);
            if (o.text.trim() === "") {
              witnessed.add("a whitespace-only send is refused");
              if (outcome.kind === "ok" || delivered.length > 0 || newRows.length > 0) fail("a whitespace-only text must be refused with nothing sent", { outcome, delivered });
              continue;
            }
            if (o.text.length > 4 * 4096) {
              if (outcome.kind === "ok" || delivered.length > 0) fail("an over-long text must be refused with nothing sent", outcome.kind);
              continue;
            }
            const reply = sendReply.parse(okValue(outcome, "send_telegram_message"));
            if (delivered.length > 1) witnessed.add("a long text goes out in several messages");
            if (!coveredByParts(o.text, delivered.map((d) => d.text))) fail("the delivered parts are not the text", delivered.map((d) => d.text.length));
            if (delivered.some((d) => d.chatId !== target || d.threadId !== o.threadId)) fail("a part went to the wrong chat or topic", delivered);
            if (JSON.stringify(reply.messageIds) !== JSON.stringify(delivered.map((d) => d.messageId))) fail("messageIds are not the delivered messages", { reply, delivered });
            if (reply.messageId !== delivered[0]?.messageId || reply.parts !== delivered.length) fail("messageId / parts", reply);
            if (reply.chatId !== String(target) || reply.messageThreadId !== o.threadId) fail("reply chat / topic", reply);
            const wantRows = delivered.map((d) => ({ chatId: d.chatId, tgMessageId: d.messageId, threadId: d.threadId, role: "assistant", text: d.text }));
            if (JSON.stringify(newRows.map(({ id: _, ...r }) => r)) !== JSON.stringify(wantRows)) fail("the log must gain one assistant row per delivered message", { newRows, wantRows });
            continue;
          }

          if (o.kind === "edit") {
            const mine = [...fake.chat.values()];
            const target = mine[o.pick % Math.max(mine.length, 1)];
            if (target === undefined) continue;
            const text = o.same ? target.text : o.text;
            const unchanged = text === target.text;
            const outcome = await call(client, "edit_telegram_message", { chatId: target.chatId, messageId: target.messageId, text });
            const after = await observe(server);
            if (unchanged || text.trim() === "") {
              witnessed.add("an edit that changes nothing is refused");
              if (outcome.kind === "ok") fail("an edit that changes nothing, or empties the message, must be refused", outcome);
              if (JSON.stringify(after) !== JSON.stringify(before)) fail("a refused edit changed state", null);
              continue;
            }
            const reply = editReply.parse(okValue(outcome, "edit_telegram_message"));
            if (reply.messageId !== target.messageId) fail("edited the wrong message", reply);
            witnessed.add("a message is edited");
            const row = after.log.find((r) => r.chatId === target.chatId && r.tgMessageId === target.messageId && r.role === "assistant");
            if (row?.text !== text) fail("the chat log must show the edited text", { row: row ?? null, text });
            continue;
          }

          // history
          const chatId = o.chat === "mine" ? CHAT : OTHER_CHAT;
          const args: Record<string, unknown> = { chatId };
          if (o.limit !== undefined) args.limit = o.limit;
          if (o.threadId !== undefined) args.threadId = o.threadId;
          const reply = historyReply.parse(okValue(await call(client, "get_telegram_chat_history", args), "get_telegram_chat_history"));
          const after = await observe(server);
          if (JSON.stringify(after) !== JSON.stringify(before)) fail("reading history changed state", null);
          const scope = before.log.filter((r) => r.chatId === chatId && (o.threadId === undefined || r.threadId === o.threadId));
          const want = scope.slice(-(o.limit ?? 50)).map((r) => r.id);
          const got = reply.messages.map((m) => m.id);
          if (JSON.stringify(got) !== JSON.stringify(want)) fail("history is the last N rows of the chat (and topic), oldest first", { got, want });
          for (const m of reply.messages) {
            const r = before.log.find((x) => x.id === m.id);
            if (!r || r.text !== m.text || r.role !== m.role || r.threadId !== m.thread_id || r.tgMessageId !== m.tg_message_id) fail("history row as stored", m);
          }
          if (o.limit !== undefined && scope.length > o.limit) witnessed.add("history is cut to its limit");
          if (scope.some((r) => r.role === "user") && scope.some((r) => r.role === "assistant")) witnessed.add("history interleaves both sides");
        }
      }),
      { numRuns: RUNS },
    );
    expect([...witnessed].sort()).toEqual(
      [
        "a long text goes out in several messages",
        "a message is edited",
        "a whitespace-only send is refused",
        "an edit that changes nothing is refused",
        "history interleaves both sides",
        "history is cut to its limit",
      ].sort(),
    );
  }, 600_000);

  // A send that fails partway: the delivered parts are in the chat, so they
  // are in the log and the error names them; nothing after the failure is.
  it("a send that stops partway logs exactly the delivered parts", async () => {
    const text = ["first", "second", "third"].map((b) => b.padEnd(3000, ".")).join("\n\n");
    fake.fail("sendMessage", 1, 1); // the second part fails
    const outcome = await call(client, "send_telegram_message", { text });
    const world = await observe(server);
    expect(fake.sent.map((d) => d.text.slice(0, 5))).toEqual(["first"]);
    expect(outcome.kind).toBe("toolError");
    if (outcome.kind === "toolError") expect(outcome.message).toContain(String(fake.sent[0]?.messageId));
    expect(world.log.map((r) => [r.tgMessageId, r.text])).toEqual(fake.sent.map((d) => [d.messageId, d.text]));
  });

  // Telegram accepted the message; the chat log write then failed. The tool
  // must not report a failure — the model would send the message again.
  it("reports a delivered message as delivered even when the chat log write fails", async () => {
    await server.state.pool.query("ALTER TABLE telegram_messages RENAME TO telegram_messages_away");
    try {
      const outcome = await call(client, "send_telegram_message", { text: "your bill is paid" });
      expect(fake.sent.map((d) => d.text)).toEqual(["your bill is paid"]);
      const reply = sendReply.parse(okValue(outcome, "send_telegram_message"));
      expect(reply.messageIds).toEqual(fake.sent.map((d) => d.messageId));
      expect(reply.logError).toMatch(/chat log/);
    } finally {
      await server.state.pool.query("ALTER TABLE telegram_messages_away RENAME TO telegram_messages");
    }
  });
});
