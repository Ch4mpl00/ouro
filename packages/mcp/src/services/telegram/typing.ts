import { sendChatAction, type ChatAction } from "./client";

// In-memory typing keep-alive. Telegram's `sendChatAction` indicator
// only lives ~5s, so anything longer than that needs repeated pings.
// The agent calls `start_typing(chatId)` once, this module re-sends the
// action every ~4s until either the agent's `send_telegram_message`
// tool clears it, or a safety TTL elapses (so a crashed/forgotten
// session can't keep the typing dots forever).

interface TypingEntry {
  chatId: string | number;
  action: ChatAction;
  messageThreadId?: number;
  expiresAt: number;
}

const TICK_INTERVAL_MS = 4_000;
const DEFAULT_TTL_MS = 5 * 60_000;

const active = new Map<string, TypingEntry>();
let timer: NodeJS.Timeout | null = null;

function key(chatId: string | number, threadId?: number): string {
  return `${chatId}:${threadId ?? 0}`;
}

function ensureTimer(): void {
  if (timer) return;
  timer = setInterval(tick, TICK_INTERVAL_MS);
  // Don't keep the Node process alive just because typing is on.
  timer.unref?.();
}

function stopTimerIfIdle(): void {
  if (active.size === 0 && timer) {
    clearInterval(timer);
    timer = null;
  }
}

async function tick(): Promise<void> {
  const now = Date.now();
  for (const [k, entry] of active) {
    if (entry.expiresAt < now) {
      active.delete(k);
      continue;
    }
    try {
      await sendChatAction(entry.chatId, entry.action, entry.messageThreadId);
    } catch (err) {
      console.error(`[telegram-typing] keepalive failed for ${k}:`, err);
    }
  }
  stopTimerIfIdle();
}

export async function startTyping(
  chatId: string | number,
  action: ChatAction = "typing",
  messageThreadId?: number,
  ttlMs: number = DEFAULT_TTL_MS,
): Promise<void> {
  active.set(key(chatId, messageThreadId), {
    chatId,
    action,
    messageThreadId,
    expiresAt: Date.now() + ttlMs,
  });
  ensureTimer();
  // Fire once immediately so the indicator shows up without waiting
  // for the first tick.
  try {
    await sendChatAction(chatId, action, messageThreadId);
  } catch (err) {
    console.error(`[telegram-typing] initial send failed for ${chatId}:`, err);
  }
}

// Called from `send_telegram_message` once the reply is on its way —
// the bot's outgoing message clears the indicator client-side, and we
// stop re-sending so the next session doesn't inherit stale typing.
export function stopTyping(chatId: string | number, messageThreadId?: number): void {
  active.delete(key(chatId, messageThreadId));
  stopTimerIfIdle();
}
