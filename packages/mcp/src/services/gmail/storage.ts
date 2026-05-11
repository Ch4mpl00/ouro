import { getDb } from "../../db/client";

// Tiny KV for the Gmail poller. Per-subscription watermarks live here so
// we don't re-emit signals for already-processed emails across restarts.

export function getKv(key: string): string | null {
  const row = getDb()
    .prepare(`SELECT value FROM gmail_kv WHERE key = ?`)
    .get(key) as { value: string } | undefined;
  return row?.value ?? null;
}

export function setKv(key: string, value: string): void {
  getDb()
    .prepare(
      `INSERT INTO gmail_kv (key, value) VALUES (?, ?)
       ON CONFLICT(key) DO UPDATE SET value = excluded.value`,
    )
    .run(key, value);
}
