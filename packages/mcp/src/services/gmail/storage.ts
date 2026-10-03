import type { Credentials } from "google-auth-library";
import type { StateDb } from "../../db/state";

// Gmail state in `mcp_state`: OAuth tokens in `integration_account` (exactly
// as googleapis hands them out), per-subscription watermarks in `gmail_kv` so
// we don't re-emit signals for already-processed emails across restarts.

const PROVIDER = "gmail";

export interface GmailAccount {
  access_token: string | null;
  refresh_token: string | null;
  expires_at: string | null;
}

export interface GmailStore {
  getKv(key: string): Promise<string | null>;
  setKv(key: string, value: string): Promise<void>;
  account(accountKey: string): Promise<GmailAccount | null>;
  // The most recently authorized account, when GMAIL_ACCOUNT_KEY names none.
  latestAccountKey(): Promise<string | null>;
  persistTokens(accountKey: string, tokens: Credentials): Promise<void>;
}

export function createGmailStore(db: StateDb): GmailStore {
  const account = async (accountKey: string): Promise<GmailAccount | null> => {
    const { rows } = await db.pool.query<GmailAccount>(
      `SELECT access_token, refresh_token, expires_at FROM integration_account
        WHERE provider = $1 AND account_key = $2`,
      [PROVIDER, accountKey],
    );
    return rows[0] ?? null;
  };

  return {
    getKv: async (key) => {
      const { rows } = await db.pool.query<{ value: string }>("SELECT value FROM gmail_kv WHERE key = $1", [key]);
      return rows[0]?.value ?? null;
    },

    setKv: async (key, value) => {
      await db.pool.query(
        `INSERT INTO gmail_kv (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value`,
        [key, value],
      );
    },

    account,

    latestAccountKey: async () => {
      const { rows } = await db.pool.query<{ account_key: string }>(
        `SELECT account_key FROM integration_account
          WHERE provider = $1 ORDER BY created_at DESC LIMIT 1`,
        [PROVIDER],
      );
      return rows[0]?.account_key ?? null;
    },

    persistTokens: async (accountKey, tokens) => {
      const existing = await account(accountKey);
      // Only overwrite a non-null with a fresh non-null. Refresh tokens in
      // particular are not always re-issued — preserve the previously
      // persisted value when the new credentials only carry an access token.
      const accessToken = tokens.access_token ?? existing?.access_token ?? null;
      const refreshToken = tokens.refresh_token ?? existing?.refresh_token ?? null;
      const expiresAt = tokens.expiry_date
        ? new Date(tokens.expiry_date).toISOString()
        : (existing?.expires_at ?? null);
      await db.pool.query(
        `INSERT INTO integration_account (provider, account_key, access_token, refresh_token, expires_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (provider, account_key) DO UPDATE SET
           access_token = EXCLUDED.access_token,
           refresh_token = EXCLUDED.refresh_token,
           expires_at = EXCLUDED.expires_at,
           updated_at = now()`,
        [PROVIDER, accountKey, accessToken, refreshToken, expiresAt],
      );
    },
  };
}
