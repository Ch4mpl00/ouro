import { getDb } from "./client";

// Agent-side memory KV. Lives in `memory` table of `agent.db`. This is the
// freeform store for anything the agent wants to remember between sessions
// that doesn't fit a typed table — watermarks, last-seen markers, small
// notes. Distinct from MCP-side `tokens.db`, which holds integration
// state (OAuth tokens, queues, caches) the MCP process owns.

export function getMemory(key: string): string | null {
  const row = getDb()
    .prepare(`SELECT value FROM memory WHERE key = ?`)
    .get(key) as { value: string } | undefined;
  return row?.value ?? null;
}

export function setMemory(key: string, value: string): void {
  getDb()
    .prepare(
      `INSERT INTO memory (key, value) VALUES (?, ?)
       ON CONFLICT(key) DO UPDATE SET
         value = excluded.value,
         updated_at = datetime('now')`,
    )
    .run(key, value);
}

// Well-known keys injected into the session context block. Keep them here
// so writers and the supervisor agree on naming.
export const MEMORY_KEYS = {
  newsLastReadAt: "news_digest.last_read_at",
} as const;