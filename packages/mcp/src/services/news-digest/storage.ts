import { getDb } from "../../db/client";

// Internal KV for the news-digest poller. Currently holds only
// `last_fire_date` — the "did we already fire today's daily signal"
// guard. The agent-side read watermark (`last_read_at`) moved to
// `agent.db memory` since it's reasoning state, not integration state.

export function getKv(key: string): string | null {
  const row = getDb()
    .prepare(`SELECT value FROM news_digest_kv WHERE key = ?`)
    .get(key) as { value: string } | undefined;
  return row?.value ?? null;
}

export function setKv(key: string, value: string): void {
  getDb()
    .prepare(
      `INSERT INTO news_digest_kv (key, value) VALUES (?, ?)
       ON CONFLICT(key) DO UPDATE SET value = excluded.value`,
    )
    .run(key, value);
}
