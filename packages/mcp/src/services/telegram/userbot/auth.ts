import type { StateDb } from "../../../db/state";

// Userbot credentials live in the same `integration_account` table as
// Gmail/etc. The session string goes in `access_token` (it IS the
// long-lived credential — there's no refresh, no expiry under normal use).
// Compromise of this value = full account access; it lives only in the
// `mcp_state` database.

const PROVIDER = "telegram_userbot";

export interface SavedSession {
  accountKey: string;
  session: string;
}

export interface UserbotSessions {
  // `accountKey` omitted → the most recently saved session.
  saved(accountKey?: string): Promise<SavedSession | null>;
  save(accountKey: string, session: string, metadata?: Record<string, unknown>): Promise<void>;
}

export function createUserbotSessions(db: StateDb): UserbotSessions {
  return {
    saved: async (accountKey) => {
      const { rows } = accountKey
        ? await db.pool.query<{ account_key: string; access_token: string }>(
            `SELECT account_key, access_token FROM integration_account
              WHERE provider = $1 AND account_key = $2`,
            [PROVIDER, accountKey],
          )
        : await db.pool.query<{ account_key: string; access_token: string }>(
            `SELECT account_key, access_token FROM integration_account
              WHERE provider = $1 ORDER BY created_at DESC LIMIT 1`,
            [PROVIDER],
          );
      const row = rows[0];
      return row ? { accountKey: row.account_key, session: row.access_token } : null;
    },

    save: async (accountKey, session, metadata) => {
      await db.pool.query(
        `INSERT INTO integration_account (provider, account_key, access_token, metadata)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (provider, account_key) DO UPDATE SET
           access_token = EXCLUDED.access_token,
           metadata = EXCLUDED.metadata,
           updated_at = now()`,
        [PROVIDER, accountKey, session, metadata ? JSON.stringify(metadata) : null],
      );
    },
  };
}
