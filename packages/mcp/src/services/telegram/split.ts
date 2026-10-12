// Telegram caps a message at 4096 characters. A longer text — a digest that
// came out long — is sent as several messages instead of failing the send:
// the limit is the transport's, so the transport tool deals with it, for every
// caller at once.
//
// Plain text only: the send tool sets no parse_mode, so a cut can never break
// a Markdown or HTML entity.

import { sendMessage, type SendMessageInput, type SentMessage } from "./client";

// Counted the way JS strings count: UTF-16 code units, which never undercounts
// what Telegram measures.
export const TELEGRAM_TEXT_LIMIT = 4096;

// Where a part may end, best first. A cut lands right after the separator, so
// the separator stays with the part before it and nothing is lost.
const SEPARATORS = ["\n\n", "\n", " "] as const;

// Splits `text` into parts of at most `limit` code units whose concatenation
// is exactly `text`. Prefers ending a part at a blank line (between digest
// blocks), then at a line break, then at a space — each only if that keeps the
// part at least half full, so a stray early break doesn't produce a sliver of
// a message. Failing all of that, a hard cut that never splits a surrogate
// pair.
export function splitTelegramText(text: string, limit = TELEGRAM_TEXT_LIMIT): string[] {
  if (limit < 2) throw new Error(`limit must be at least 2, got ${limit}`);
  const parts: string[] = [];
  let rest = text;
  while (rest.length > limit) {
    const cut = cutPoint(rest, limit);
    parts.push(rest.slice(0, cut));
    rest = rest.slice(cut);
  }
  if (rest.length > 0) parts.push(rest);
  return parts;
}

function cutPoint(text: string, limit: number): number {
  const window = text.slice(0, limit);
  for (const minEnd of [Math.ceil(limit / 2), 1]) {
    for (const sep of SEPARATORS) {
      const at = window.lastIndexOf(sep);
      if (at >= 0 && at + sep.length >= minEnd) return at + sep.length;
    }
  }
  const code = text.charCodeAt(limit - 1);
  const highSurrogate = code >= 0xd800 && code <= 0xdbff;
  return highSurrogate ? limit - 1 : limit;
}

// A send that stopped partway. The parts in `delivered` are already in the
// chat; the caller must not blindly resend the whole text.
export class PartialSendError extends Error {
  readonly total: number;

  constructor(
    readonly delivered: SentMessage[],
    // Every part of the text, delivered or not; delivered[i] carried parts[i].
    readonly parts: string[],
    cause: unknown,
  ) {
    const total = parts.length;
    const ids = delivered.map((m) => m.message_id).join(", ");
    super(
      `Telegram send stopped after ${delivered.length} of ${total} parts` +
        (delivered.length > 0 ? ` (delivered message ids: ${ids})` : "") +
        `: ${cause instanceof Error ? cause.message : String(cause)}`,
    );
    this.name = "PartialSendError";
    this.total = total;
  }
}

// Sends the parts in order, one after another, into the same chat and topic.
// `send` is injectable for tests. Parts that are only whitespace are skipped:
// Telegram rejects an empty message, and the text around them still arrives.
export async function sendLongMessage(
  input: SendMessageInput,
  send: (input: SendMessageInput) => Promise<SentMessage> = sendMessage,
): Promise<{ parts: string[]; sent: SentMessage[] }> {
  const parts = splitTelegramText(input.text).filter((p) => p.trim().length > 0);
  const sent: SentMessage[] = [];
  for (const part of parts) {
    try {
      sent.push(await send({ ...input, text: part }));
    } catch (err) {
      // Nothing delivered yet: the original error is the whole story.
      if (sent.length === 0) throw err;
      throw new PartialSendError(sent, parts, err);
    }
  }
  return { parts, sent };
}
