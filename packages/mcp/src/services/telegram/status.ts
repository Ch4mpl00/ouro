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
// The map lives in MCP process memory (like the typing keep-alive in
// `typing.ts`): it spans the tool calls of one workflow but nothing more.
// Status messages are intentionally NOT written to the chat log — they're
// ephemeral progress, not conversation, and get deleted at the end. The
// real answer ships via `send_telegram_message`.

interface StatusEntry {
  chatId: string | number;
  messageId: number;
  messageThreadId?: number;
}

const statuses = new Map<string, StatusEntry>();

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
    try {
      await deleteMessage(existing.chatId, existing.messageId);
    } catch (err) {
      // Already gone (user deleted it, or >48h old) — it's cleared either way.
      if (!(err instanceof TelegramApiError)) throw err;
    }
    return { action: "deleted", id, chatId: existing.chatId, messageId: existing.messageId };
  }

  // Edit in place when we already have a bubble for this id.
  if (existing) {
    try {
      const edited = await editMessageText({
        chatId: existing.chatId,
        messageId: existing.messageId,
        text,
      });
      return { action: "updated", id, chatId: existing.chatId, messageId: edited.message_id };
    } catch (err) {
      if (err instanceof TelegramApiError && /not modified/i.test(err.message)) {
        // Identical text — Telegram rejects the edit; treat as success.
        return { action: "updated", id, chatId: existing.chatId, messageId: existing.messageId };
      }
      if (err instanceof TelegramApiError && /(not found|to edit)/i.test(err.message)) {
        // The bubble vanished (deleted upstream) — drop the stale entry and
        // fall through to re-create so the status keeps working.
        statuses.delete(id);
      } else {
        throw err;
      }
    }
  }

  // First call (or recovery after a vanished bubble): send + remember.
  const sent = await sendMessage({
    chatId: input.chatId,
    text,
    messageThreadId: input.messageThreadId,
  });
  statuses.set(id, {
    chatId: input.chatId,
    messageId: sent.message_id,
    messageThreadId: input.messageThreadId,
  });
  return {
    action: "created",
    id,
    chatId: input.chatId,
    messageId: sent.message_id,
    messageThreadId: input.messageThreadId ?? null,
  };
}
