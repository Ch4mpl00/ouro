import { getDb } from "../../db/client";

export function getKv(key: string): string | null {
  const row = getDb().prepare(`SELECT value FROM news_kv WHERE key = ?`).get(key) as
    | { value: string }
    | undefined;
  return row?.value ?? null;
}

export function setKv(key: string, value: string): void {
  getDb()
    .prepare(
      `INSERT INTO news_kv (key, value) VALUES (?, ?)
       ON CONFLICT(key) DO UPDATE SET value = excluded.value`,
    )
    .run(key, value);
}
