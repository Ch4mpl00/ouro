import "dotenv/config";
import { getUpdates, type UpdateChat } from "./client";

function chatLabel(chat: UpdateChat): string {
  if (chat.title) return chat.title;
  const parts = [chat.first_name, chat.last_name, chat.username && `@${chat.username}`];
  return parts.filter(Boolean).join(" ");
}

async function main(): Promise<void> {
  const updates = await getUpdates();
  if (updates.length === 0) {
    console.log(
      "No updates yet. Open Telegram, start a chat with your bot, send any message, then re-run.",
    );
    return;
  }

  const seen = new Map<number, UpdateChat>();
  for (const u of updates) {
    const chat = u.message?.chat ?? u.edited_message?.chat ?? u.channel_post?.chat;
    if (!chat) continue;
    if (!seen.has(chat.id)) seen.set(chat.id, chat);
  }

  console.log(`Found ${seen.size} distinct chat(s):\n`);
  for (const chat of seen.values()) {
    console.log(`  chat_id=${chat.id}  type=${chat.type}  ${chatLabel(chat)}`);
  }
  console.log(
    "\nSet TELEGRAM_DEFAULT_CHAT_ID in .env to the chat_id you want notifications routed to.",
  );
}

main().catch((err: unknown) => {
  console.error(err);
  process.exitCode = 1;
});
