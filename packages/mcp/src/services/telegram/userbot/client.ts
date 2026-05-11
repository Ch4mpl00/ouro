import { TelegramClient } from "telegram";
import { StringSession } from "telegram/sessions/index.js";
import { getSavedSession } from "./auth";

// Lazy singleton: gramjs holds a long-lived MTProto socket. We connect on
// first use so the MCP server can boot before the userbot is authorized
// (auth is a one-time interactive flow via `pnpm userbot:auth`). Tools that
// need the userbot await getUserbotClient(); attempts before auth raise a
// clear error that points the user at the auth command.

let cached: TelegramClient | null = null;

function requireEnv(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`Missing env var: ${name}`);
  return v;
}

export function getApiCredentials(): { apiId: number; apiHash: string } {
  const apiIdRaw = requireEnv("TELEGRAM_APP_ID");
  const apiHash = requireEnv("TELEGRAM_APP_API_HASH");
  const apiId = Number(apiIdRaw);
  if (!Number.isFinite(apiId)) {
    throw new Error(`TELEGRAM_APP_ID must be numeric, got ${apiIdRaw}`);
  }
  return { apiId, apiHash };
}

export async function getUserbotClient(): Promise<TelegramClient> {
  if (cached?.connected) return cached;
  const saved = getSavedSession();
  if (!saved) {
    throw new Error(
      "Telegram userbot is not authorized. Run `pnpm userbot:auth` once to log in.",
    );
  }
  const { apiId, apiHash } = getApiCredentials();
  const client = new TelegramClient(new StringSession(saved.session), apiId, apiHash, {
    connectionRetries: 5,
  });
  await client.connect();
  cached = client;
  return client;
}

export async function disconnectUserbot(): Promise<void> {
  if (cached) {
    await cached.disconnect();
    cached = null;
  }
}
