import { getDb } from "../../db/client";

// MCP-owned Telegram chat log. Both incoming (poller) and outgoing
// (send_telegram_message) messages land here. Tables created in
// packages/mcp/data/schema.sql.

export type Role = "user" | "assistant";

export interface StoredMessage {
  id: number;
  chat_id: number;
  tg_message_id: number | null;
  thread_id: number | null;
  role: Role;
  text: string;
  created_at: string;
}

interface InsertInput {
  chatId: number;
  tgMessageId: number | null;
  threadId: number | null;
  role: Role;
  text: string;
}

export function recordMessage(input: InsertInput): number {
  const stmt = getDb().prepare(
    `INSERT INTO telegram_messages (chat_id, tg_message_id, thread_id, role, text)
     VALUES (?, ?, ?, ?, ?)`,
  );
  const info = stmt.run(
    input.chatId,
    input.tgMessageId,
    input.threadId,
    input.role,
    input.text,
  );
  return Number(info.lastInsertRowid);
}

// Returns messages chronologically in the order:
//   [up to `prev` messages with id <= since] ++ [all messages with id > since]
// If `since` is undefined, returns just the last `prev` messages overall.
//
// `newCount` counts ONLY user-role messages with id > since — assistant
// messages we just sent ourselves don't count as "something to react to",
// otherwise the agent would loop on its own outgoing messages.
export function listChatMessages(opts: {
  chatId?: number;
  since?: number;
  prev?: number;
}): { messages: StoredMessage[]; lastId: number | null; newCount: number } {
  const db = getDb();
  const prev = opts.prev ?? 20;

  let newRows: StoredMessage[] = [];
  if (opts.since !== undefined) {
    const newStmt = opts.chatId !== undefined
      ? db.prepare(`SELECT * FROM telegram_messages WHERE id > ? AND chat_id = ? ORDER BY id ASC`)
      : db.prepare(`SELECT * FROM telegram_messages WHERE id > ? ORDER BY id ASC`);
    newRows = (opts.chatId !== undefined
      ? newStmt.all(opts.since, opts.chatId)
      : newStmt.all(opts.since)) as StoredMessage[];
  }

  const cutoff = opts.since ?? Number.MAX_SAFE_INTEGER;
  const prevStmt = opts.chatId !== undefined
    ? db.prepare(`SELECT * FROM telegram_messages WHERE id <= ? AND chat_id = ? ORDER BY id DESC LIMIT ?`)
    : db.prepare(`SELECT * FROM telegram_messages WHERE id <= ? ORDER BY id DESC LIMIT ?`);
  const prevRowsDesc = (opts.chatId !== undefined
    ? prevStmt.all(cutoff, opts.chatId, prev)
    : prevStmt.all(cutoff, prev)) as StoredMessage[];
  const prevRows = prevRowsDesc.reverse();

  const messages = [...prevRows, ...newRows];
  const lastId = messages.length > 0 ? messages[messages.length - 1]!.id : null;
  const newCount = newRows.filter((r) => r.role === "user").length;
  return { messages, lastId, newCount };
}

// Last `limit` messages for a chat in chronological order. If `threadId` is
// provided, only messages from that forum topic are returned (NULL thread_id
// = "general"/non-topic messages, matched only when threadId is null).
export function getChatHistory(
  chatId: number,
  limit = 50,
  threadId?: number | null,
): StoredMessage[] {
  const db = getDb();
  let rows: StoredMessage[];
  if (threadId === undefined) {
    rows = db
      .prepare(`SELECT * FROM telegram_messages WHERE chat_id = ? ORDER BY id DESC LIMIT ?`)
      .all(chatId, limit) as StoredMessage[];
  } else if (threadId === null) {
    rows = db
      .prepare(
        `SELECT * FROM telegram_messages WHERE chat_id = ? AND thread_id IS NULL ORDER BY id DESC LIMIT ?`,
      )
      .all(chatId, limit) as StoredMessage[];
  } else {
    rows = db
      .prepare(
        `SELECT * FROM telegram_messages WHERE chat_id = ? AND thread_id = ? ORDER BY id DESC LIMIT ?`,
      )
      .all(chatId, threadId, limit) as StoredMessage[];
  }
  return rows.reverse();
}

export function getKv(key: string): string | null {
  const row = getDb().prepare(`SELECT value FROM telegram_kv WHERE key = ?`).get(key) as
    | { value: string }
    | undefined;
  return row?.value ?? null;
}

export function setKv(key: string, value: string): void {
  getDb()
    .prepare(
      `INSERT INTO telegram_kv (key, value) VALUES (?, ?)
       ON CONFLICT(key) DO UPDATE SET value = excluded.value`,
    )
    .run(key, value);
}

export function getLastUpdateId(): number | null {
  const v = getKv("last_update_id");
  return v ? Number(v) : null;
}

export function setLastUpdateId(id: number): void {
  setKv("last_update_id", String(id));
}
