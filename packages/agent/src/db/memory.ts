import type Database from "better-sqlite3";

// Agent-side memory KV. Lives in `memory` table of `agent.db`. This is the
// freeform store for anything the agent wants to remember between sessions
// that doesn't fit a typed table — watermarks, last-seen markers, small
// notes. Distinct from MCP-side `tokens.db`, which holds integration
// state (OAuth tokens, queues, caches) the MCP process owns.

export interface MemoryStore {
  get(key: string): string | null;
  set(key: string, value: string): void;
}

export function createMemoryStore(db: Database.Database): MemoryStore {
  const selectStmt = db.prepare(`SELECT value FROM memory WHERE key = ?`);
  const upsertStmt = db.prepare(
    `INSERT INTO memory (key, value) VALUES (?, ?)
     ON CONFLICT(key) DO UPDATE SET
       value = excluded.value,
       updated_at = datetime('now')`,
  );
  return {
    get(key) {
      const row = selectStmt.get(key) as { value: string } | undefined;
      return row?.value ?? null;
    },
    set(key, value) {
      upsertStmt.run(key, value);
    },
  };
}

// Well-known keys injected into the session context block. Keep them here
// so writers and the supervisor agree on naming.
export const MEMORY_KEYS = {
  newsLastReadAt: "news_digest.last_read_at",
} as const;
