import { getDb } from "../../db/client";

// Shared storage for the news-digest signal source. Two distinct watermarks
// live in `news_digest_kv`:
//   - `last_fire_date` — owned by the poller, "did we already fire the
//     daily signal for YYYY-MM-DD" guard (see poller.ts).
//   - `last_read_at` — owned by the agent. The agent stamps it after every
//     channel-post read so the next digest knows which `since` to query
//     from, instead of always defaulting to "now - 24h".

const LAST_READ_KEY = "last_read_at";

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

export function getLastNewsReadAt(): string | null {
  return getKv(LAST_READ_KEY);
}

export function setLastNewsReadAt(iso: string): void {
  setKv(LAST_READ_KEY, iso);
}
