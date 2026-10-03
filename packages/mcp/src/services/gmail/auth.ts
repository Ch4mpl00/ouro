import { google } from "googleapis";
import type { Credentials, OAuth2Client } from "google-auth-library";
import { GMAIL_READ_SCOPES } from "./scopes";
import type { GmailStore } from "./storage";

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

export async function exchangeCodeAndPersist(store: GmailStore, code: string): Promise<{ accountKey: string }> {
  const client = createOAuth2Client();
  const { tokens } = await client.getToken(code);
  client.setCredentials(tokens);

  const oauth2 = google.oauth2({ version: "v2", auth: client });
  const { data: userinfo } = await oauth2.userinfo.get();
  const accountKey = userinfo.email;
  if (!accountKey) {
    throw new Error("Could not resolve account email from Google userinfo response");
  }

  await store.persistTokens(accountKey, tokens);
  return { accountKey };
}
