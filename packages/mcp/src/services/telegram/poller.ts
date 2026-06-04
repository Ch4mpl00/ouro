import { getUpdates, getDefaultChatId, type UpdateMessage } from "./client";
import { recordMessage, getLastUpdateId, setLastUpdateId } from "./storage";
import { recordSignal } from "../signals";

// Long-poll loop. The MCP server starts this once at boot. Each iteration
// blocks up to LONG_POLL_TIMEOUT seconds inside Telegram's getUpdates,
// then persists any text messages (filtered to the configured default chat)
// and advances the last_update_id watermark so we never replay.
//
// Telegram's bot API only allows one consumer of getUpdates per bot, so
// running this inside the MCP server (the canonical Telegram surface) is
// the correct ownership: nothing else in the system polls.

const LONG_POLL_TIMEOUT = 25;
const ERROR_BACKOFF_MS = 5_000;

function logPrefix(): string {
  return `[${new Date().toISOString()}] [tg-poller]`;
}

function pickMessage(u: { message?: UpdateMessage; edited_message?: UpdateMessage }): UpdateMessage | undefined {
  return u.message ?? u.edited_message;
}

export function startTelegramPoller(): void {
  const allowedChatRaw = getDefaultChatId();
  if (!allowedChatRaw) {
    console.warn(`${logPrefix()} TELEGRAM_DEFAULT_CHAT_ID is not set — poller disabled (would accept anyone)`);
    return;
  }
  const allowedChatId = Number(allowedChatRaw);
  if (!Number.isFinite(allowedChatId)) {
    console.warn(`${logPrefix()} TELEGRAM_DEFAULT_CHAT_ID is not a valid number, got ${allowedChatRaw} — poller disabled`);
    return;
  }

  console.log(`${logPrefix()} starting (chat=${allowedChatId}, timeout=${LONG_POLL_TIMEOUT}s)`);

  let stopping = false;
  const stop = (): void => {
    stopping = true;
  };
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);

  void (async () => {
    while (!stopping) {
      try {
        const lastId = getLastUpdateId();
        const offset = lastId !== null ? lastId + 1 : undefined;
        const updates = await getUpdates({
          offset,
          timeout: LONG_POLL_TIMEOUT,
          allowed_updates: ["message", "edited_message"],
        });

        for (const u of updates) {
          const msg = pickMessage(u);
          if (!msg) continue;
          if (msg.chat.id !== allowedChatId) {
            console.warn(`${logPrefix()} ignoring message from chat=${msg.chat.id} (not allowed chat)`);
          } else if (msg.text && msg.text.length > 0) {
            const threadId = msg.message_thread_id ?? null;
            recordMessage({
              chatId: msg.chat.id,
              tgMessageId: msg.message_id,
              threadId,
              role: "user",
              text: msg.text,
            });
            // Signal content is DATA, not agent instructions. It used to
            // carry "Reply by calling send_telegram_message(...)" — useful
            // for the old agentic loop, but poison for the workflow path:
            // a non-tool `llm_compose` step fed this text would dutifully
            // emit a send_telegram_message tool-call as plain text, which
            // then got delivered verbatim. The planner already knows how to
            // reply (send step + chatId from envContext); reply/thread
            // conventions live in planner.md.
            const content = [
              `Telegram message in chat ${msg.chat.id}${threadId !== null ? ` (forum topic thread_id=${threadId})` : ""}.`,
              `Text: ${JSON.stringify(msg.text)}`,
            ].join("\n");
            recordSignal({ source: "telegram", content });
            console.log(
              `${logPrefix()} stored msg ${msg.message_id}${threadId !== null ? ` (thread=${threadId})` : ""} + signal queued`,
            );
          }
          setLastUpdateId(u.update_id);
        }
      } catch (err) {
        console.error(`${logPrefix()} poll error:`, err instanceof Error ? err.message : err);
        await new Promise((r) => setTimeout(r, ERROR_BACKOFF_MS));
      }
    }
    console.log(`${logPrefix()} stopped`);
  })();
}
