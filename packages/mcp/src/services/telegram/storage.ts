import { sqlTime, type StateDb } from "../../db/state";

// MCP-owned Telegram chat log. Both incoming (poller) and outgoing
// (send_telegram_message) messages land here, plus the poller's update
// cursor. Tables live in the `mcp_state` database (db/state.ts).

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

export interface TelegramLog {
  record(input: { chatId: number; tgMessageId: number | null; threadId: number | null; role: Role; text: string }): Promise<number>;
  // Last `limit` messages for a chat, chronologically. `threadId` undefined =
  // every topic; null = only the General / non-topic messages.
  history(chatId: number, limit?: number, threadId?: number | null): Promise<StoredMessage[]>;
  // The bot edited one of its messages: the log shows what the chat shows.
  edited(chatId: number, tgMessageId: number, text: string): Promise<void>;
  lastUpdateId(): Promise<number | null>;
  setLastUpdateId(id: number): Promise<void>;
}

const COLUMNS = `id, chat_id, tg_message_id, thread_id, role, text, ${sqlTime("created_at")} AS created_at`;

export function createTelegramLog(db: StateDb): TelegramLog {
  return {
    record: async (input) => {
      const { rows } = await db.pool.query<{ id: number }>(
        `INSERT INTO telegram_messages (chat_id, tg_message_id, thread_id, role, text)
         VALUES ($1, $2, $3, $4, $5) RETURNING id`,
        [input.chatId, input.tgMessageId, input.threadId, input.role, input.text],
      );
      return rows[0]!.id;
    },

    history: async (chatId, limit = 50, threadId) => {
      const thread =
        threadId === undefined ? "" : threadId === null ? "AND thread_id IS NULL" : "AND thread_id = $3";
      const params = threadId == null ? [chatId, limit] : [chatId, limit, threadId];
      const { rows } = await db.pool.query<StoredMessage>(
        `SELECT ${COLUMNS} FROM telegram_messages
          WHERE chat_id = $1 ${thread}
          ORDER BY id DESC LIMIT $2`,
        params,
      );
      return rows.reverse();
    },

    edited: async (chatId, tgMessageId, text) => {
      await db.pool.query(
        `UPDATE telegram_messages SET text = $3
          WHERE chat_id = $1 AND tg_message_id = $2 AND role = 'assistant'`,
        [chatId, tgMessageId, text],
      );
    },

    lastUpdateId: async () => {
      const { rows } = await db.pool.query<{ value: string }>(
        "SELECT value FROM telegram_kv WHERE key = 'last_update_id'",
      );
      return rows[0] ? Number(rows[0].value) : null;
    },

    setLastUpdateId: async (id) => {
      await db.pool.query(
        `INSERT INTO telegram_kv (key, value) VALUES ('last_update_id', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value`,
        [String(id)],
      );
    },
  };
}
