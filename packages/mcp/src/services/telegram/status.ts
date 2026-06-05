import {
  sendMessage,
  editMessageText,
  deleteMessage,
  TelegramApiError,
} from "./client";

// In-memory live-status registry. A workflow shows progress in a SINGLE
// Telegram message that is edited in place, instead of spamming the chat
// with one message per step. Keyed by a caller-chosen id (e.g.
// `status:<signalId>`):
//
//   - first call with non-empty text  → send a message, remember its id
//   - later calls with the same id     → edit that message in place
//   - call with empty text             → delete the message, forget the id
//
// On top of that, MCP animates the bubble on its own — like the typing
// keep-alive in `typing.ts`. A background timer ticks ~1/s and edits the
// active bubble, cycling trailing dots after the text (0 → . → .. → ... →
// 0 → …), WITHOUT the workflow doing anything. The caller just sets a base
// text; "aliveness" is MCP's job. Animation restarts on a text update,
// stops when the bubble is cleared, and freezes after a safety TTL (so a
// forgotten bubble doesn't edit forever).
//
// The map lives in MCP process memory; it spans the tool calls of one
// workflow but nothing more. Status messages are intentionally NOT written
// to the chat log — they're ephemeral progress, not conversation, and get
// deleted at the end. The real answer ships via `send_telegram_message`.

interface StatusEntry {
  chatId: string | number;
  messageId: number;
  messageThreadId?: number;
  baseText: string;
  frame: number;
  // Stop animating (freeze on the last frame) once this passes — a safety
  // net so a bubble nobody cleared doesn't edit indefinitely.
  expiresAt: number;
  // Don't edit before this — set a few seconds out on a 429/error so we
  // back off instead of hammering.
  nextEditAt: number;
  // An edit is in flight; skip this tick to avoid overlapping edits racing.
  busy: boolean;
  // Set false after the TTL freeze so the timer can shut down when idle.
  animating: boolean;
}

const ANIM_INTERVAL_MS = 1_000;
const ANIM_TTL_MS = 3 * 60_000;
const BACKOFF_MS = 5_000;

// Trailing dots appended after the text, one more per tick, wrapping at 3
// back to none: 0 → . → .. → ... → 0 → … Adjacent frames always differ, so
// Telegram never rejects an edit as "not modified".
const DOT_CYCLE = 4;

const statuses = new Map<string, StatusEntry>();
let timer: NodeJS.Timeout | null = null;

function render(baseText: string, frame: number): string {
  return baseText + ".".repeat(frame % DOT_CYCLE);
}

function ensureTimer(): void {
  if (timer) return;
  timer = setInterval(tick, ANIM_INTERVAL_MS);
  // Don't keep the Node process alive just because a bubble is animating.
  timer.unref?.();
}

function stopTimerIfIdle(): void {
  const anyAnimating = [...statuses.values()].some((e) => e.animating);
  if (!anyAnimating && timer) {
    clearInterval(timer);
    timer = null;
  }
}

async function tick(): Promise<void> {
  const now = Date.now();
  for (const entry of statuses.values()) {
    if (!entry.animating) continue;
    if (entry.expiresAt < now) {
      // Freeze on the current frame — leave the last text as-is.
      entry.animating = false;
      continue;
    }
    if (entry.busy || now < entry.nextEditAt) continue;
    entry.frame += 1;
    const text = render(entry.baseText, entry.frame);
    entry.busy = true;
    try {
      await editMessageText({ chatId: entry.chatId, messageId: entry.messageId, text });
    } catch (err) {
      // 429 / transient / vanished bubble — back this entry off a few
      // seconds rather than retrying every tick. ("not modified" can't
      // happen since adjacent frames differ, but tolerate it anyway.)
      if (!(err instanceof TelegramApiError && /not modified/i.test(err.message))) {
        entry.nextEditAt = Date.now() + BACKOFF_MS;
      }
    } finally {
      entry.busy = false;
    }
  }
  stopTimerIfIdle();
}

export interface SendStatusInput {
  id: string;
  text: string;
  chatId: string | number;
  messageThreadId?: number;
}

export interface SendStatusResult {
  // created: first bubble sent · updated: edited in place ·
  // deleted: cleared an existing bubble · noop: clear requested but nothing tracked
  action: "created" | "updated" | "deleted" | "noop";
  id: string;
  chatId?: string | number;
  messageId?: number;
  messageThreadId?: number | null;
}

export async function sendStatus(input: SendStatusInput): Promise<SendStatusResult> {
  const { id } = input;
  const text = input.text.trim();
  const existing = statuses.get(id);

  // Empty text = clear: delete the live bubble and forget it.
  if (text.length === 0) {
    if (!existing) return { action: "noop", id };
    statuses.delete(id);
    stopTimerIfIdle();
    try {
      await deleteMessage(existing.chatId, existing.messageId);
    } catch (err) {
      // Already gone (user deleted it, or >48h old) — it's cleared either way.
      if (!(err instanceof TelegramApiError)) throw err;
    }
    return { action: "deleted", id, chatId: existing.chatId, messageId: existing.messageId };
  }

  // Edit in place when we already have a bubble for this id: reset the
  // animation to the new base text (frame 0).
  if (existing) {
    try {
      const edited = await editMessageText({
        chatId: existing.chatId,
        messageId: existing.messageId,
        text: render(text, 0),
      });
      existing.baseText = text;
      existing.frame = 0;
      existing.expiresAt = Date.now() + ANIM_TTL_MS;
      existing.nextEditAt = 0;
      existing.animating = true;
      ensureTimer();
      return { action: "updated", id, chatId: existing.chatId, messageId: edited.message_id };
    } catch (err) {
      if (err instanceof TelegramApiError && /(not found|to edit)/i.test(err.message)) {
        // The bubble vanished (deleted upstream) — drop the stale entry and
        // fall through to re-create so the status keeps working.
        statuses.delete(id);
      } else if (!(err instanceof TelegramApiError && /not modified/i.test(err.message))) {
        throw err;
      } else {
        // Identical text already showing — refresh animation state, done.
        existing.baseText = text;
        existing.expiresAt = Date.now() + ANIM_TTL_MS;
        existing.animating = true;
        ensureTimer();
        return { action: "updated", id, chatId: existing.chatId, messageId: existing.messageId };
      }
    }
  }

  // First call (or recovery after a vanished bubble): send + remember.
  const sent = await sendMessage({
    chatId: input.chatId,
    text: render(text, 0),
    messageThreadId: input.messageThreadId,
  });
  statuses.set(id, {
    chatId: input.chatId,
    messageId: sent.message_id,
    messageThreadId: input.messageThreadId,
    baseText: text,
    frame: 0,
    expiresAt: Date.now() + ANIM_TTL_MS,
    nextEditAt: 0,
    busy: false,
    animating: true,
  });
  ensureTimer();
  return {
    action: "created",
    id,
    chatId: input.chatId,
    messageId: sent.message_id,
    messageThreadId: input.messageThreadId ?? null,
  };
}
