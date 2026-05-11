import { getDb } from "../../../db/client";

// Userbot credentials live in the same `integration_account` table as
// Gmail/etc. The session string goes in `access_token` (it IS the
// long-lived credential — there's no refresh, no expiry under normal use).
// Compromise of this value = full account access; the file is in .gitignore
// and lives only in the local SQLite store.

const PROVIDER = "telegram_userbot";

interface SavedSession {
  accountKey: string;
  session: string;
}

export function getSavedSession(accountKey?: string): SavedSession | null {
  const db = getDb();
  const row = accountKey
    ? db
        .prepare(
          `SELECT account_key, access_token FROM integration_account
            WHERE provider = ? AND account_key = ?`,
        )
        .get(PROVIDER, accountKey)
    : db
        .prepare(
          `SELECT account_key, access_token FROM integration_account
            WHERE provider = ?
            ORDER BY created_at DESC LIMIT 1`,
        )
        .get(PROVIDER);
  if (!row) return null;
  const r = row as { account_key: string; access_token: string };
  return { accountKey: r.account_key, session: r.access_token };
}

export function saveSession(
  accountKey: string,
  session: string,
  metadata?: Record<string, unknown>,
): void {
  getDb()
    .prepare(
      `INSERT INTO integration_account (provider, account_key, access_token, metadata)
       VALUES (?, ?, ?, ?)
       ON CONFLICT(provider, account_key) DO UPDATE SET
         access_token = excluded.access_token,
         metadata = excluded.metadata,
         updated_at = datetime('now')`,
    )
    .run(PROVIDER, accountKey, session, metadata ? JSON.stringify(metadata) : null);
}
