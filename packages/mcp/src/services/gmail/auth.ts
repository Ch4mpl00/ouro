import { google } from "googleapis";
import type { Credentials, OAuth2Client } from "google-auth-library";
import { getDb } from "../../db/client";
import { GMAIL_READ_SCOPES } from "./scopes";

const PROVIDER = "gmail";

function requireEnv(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`Missing env var: ${name}`);
  return value;
}

export function createOAuth2Client(): OAuth2Client {
  return new google.auth.OAuth2(
    requireEnv("GOOGLE_CLIENT_ID"),
    requireEnv("GOOGLE_CLIENT_SECRET"),
    requireEnv("GOOGLE_REDIRECT_URI"),
  );
}

export function getAuthUrl(client: OAuth2Client = createOAuth2Client()): string {
  return client.generateAuthUrl({
    access_type: "offline",
    prompt: "consent",
    scope: GMAIL_READ_SCOPES,
  });
}

export async function exchangeCodeAndPersist(code: string): Promise<{ accountKey: string }> {
  const client = createOAuth2Client();
  const { tokens } = await client.getToken(code);
  client.setCredentials(tokens);

  const oauth2 = google.oauth2({ version: "v2", auth: client });
  const { data: userinfo } = await oauth2.userinfo.get();
  const accountKey = userinfo.email;
  if (!accountKey) {
    throw new Error("Could not resolve account email from Google userinfo response");
  }

  persistTokens(accountKey, tokens);
  return { accountKey };
}

export function persistTokens(accountKey: string, tokens: Credentials): void {
  const db = getDb();
  const existing = db
    .prepare(
      `SELECT access_token, refresh_token, expires_at FROM integration_account
       WHERE provider = ? AND account_key = ?`,
    )
    .get(PROVIDER, accountKey) as
    | { access_token: string | null; refresh_token: string | null; expires_at: string | null }
    | undefined;

  // Only overwrite a non-null with a fresh non-null. Refresh tokens in
  // particular are not always re-issued — preserve the previously persisted
  // value when the new credentials only carry an access token.
  const accessToken = tokens.access_token ?? existing?.access_token ?? null;
  const refreshToken = tokens.refresh_token ?? existing?.refresh_token ?? null;
  const expiresAt = tokens.expiry_date
    ? new Date(tokens.expiry_date).toISOString()
    : (existing?.expires_at ?? null);

  db.prepare(
    `INSERT INTO integration_account (provider, account_key, access_token, refresh_token, expires_at)
     VALUES (?, ?, ?, ?, ?)
     ON CONFLICT(provider, account_key) DO UPDATE SET
       access_token = excluded.access_token,
       refresh_token = excluded.refresh_token,
       expires_at = excluded.expires_at,
       updated_at = datetime('now')`,
  ).run(PROVIDER, accountKey, accessToken, refreshToken, expiresAt);
}
