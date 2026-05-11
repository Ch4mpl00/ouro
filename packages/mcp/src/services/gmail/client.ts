import { google, type gmail_v1 } from "googleapis";
import type { OAuth2Client } from "google-auth-library";
import { getDb } from "../../db/client";
import { createOAuth2Client, persistTokens } from "./auth";

const PROVIDER = "gmail";

interface AccountRow {
  access_token: string | null;
  refresh_token: string | null;
  expires_at: string | null;
}

export function getOAuth2ClientForAccount(accountKey: string): OAuth2Client {
  const account = getDb()
    .prepare(
      `SELECT access_token, refresh_token, expires_at FROM integration_account
       WHERE provider = ? AND account_key = ?`,
    )
    .get(PROVIDER, accountKey) as AccountRow | undefined;

  if (!account) {
    throw new Error(`No Gmail account "${accountKey}". Run \`pnpm gmail:auth\` to authorize.`);
  }
  if (!account.refresh_token) {
    throw new Error(`Gmail account "${accountKey}" has no refresh token. Re-authorize.`);
  }

  const client = createOAuth2Client();
  client.setCredentials({
    access_token: account.access_token ?? undefined,
    refresh_token: account.refresh_token,
    expiry_date: account.expires_at ? new Date(account.expires_at).getTime() : undefined,
  });

  client.on("tokens", (tokens) => {
    persistTokens(accountKey, tokens);
  });

  return client;
}

export function getGmailClient(accountKey: string): gmail_v1.Gmail {
  const auth = getOAuth2ClientForAccount(accountKey);
  return google.gmail({ version: "v1", auth });
}
