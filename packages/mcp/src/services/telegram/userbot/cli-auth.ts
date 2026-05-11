import "dotenv/config";
import { TelegramClient } from "telegram";
import { StringSession } from "telegram/sessions/index.js";
import input from "input";
import { saveSession } from "./auth";
import { getApiCredentials } from "./client";

// One-time interactive auth flow. Asks for phone number → sends code →
// asks for code → optionally 2FA password → saves the resulting StringSession
// into integration_account so subsequent runs reuse it without prompting.

interface MeResponse {
  id: unknown;
  username?: string;
  firstName?: string;
  phone?: string;
}

async function main(): Promise<void> {
  const { apiId, apiHash } = getApiCredentials();

  const client = new TelegramClient(new StringSession(""), apiId, apiHash, {
    connectionRetries: 5,
  });

  console.log("Starting Telegram userbot login (gramjs/MTProto)...");
  await client.start({
    phoneNumber: async () => await input.text("Phone number (e.g. +380501234567): "),
    password: async () => await input.text("2FA password (leave empty if not set): "),
    phoneCode: async () => await input.text("Code from Telegram: "),
    onError: (err) => console.error(err),
  });

  const me = (await client.getMe()) as MeResponse;
  const session = (client.session as StringSession).save();
  const accountKey = String(me.id);
  saveSession(accountKey, session, {
    username: me.username,
    firstName: me.firstName,
    phone: me.phone,
  });
  console.log(
    `\n✓ Saved session for account ${accountKey}` +
      (me.username ? ` (@${me.username})` : ""),
  );

  await client.disconnect();
  process.exit(0);
}

main().catch((e) => {
  console.error("userbot:auth failed:", e);
  process.exit(1);
});
