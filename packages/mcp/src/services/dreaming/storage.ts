import { getDb } from "../../db/client";

const LAST_DREAMING_KEY = "last_dreaming_at";

export function getKv(key: string): string | null {
  const row = getDb().prepare(`SELECT value FROM dreaming_kv WHERE key = ?`).get(key) as
    | { value: string }
    | undefined;
  return row?.value ?? null;
}

export function setKv(key: string, value: string): void {
  getDb()
    .prepare(
      `INSERT INTO dreaming_kv (key, value) VALUES (?, ?)
       ON CONFLICT(key) DO UPDATE SET value = excluded.value`,
    )
    .run(key, value);
}

export function getLastDreamingAt(): string | null {
  return getKv(LAST_DREAMING_KEY);
}

export function setLastDreamingAt(iso: string): void {
  setKv(LAST_DREAMING_KEY, iso);
}
