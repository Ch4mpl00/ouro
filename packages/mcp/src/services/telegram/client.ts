// Thin wrapper over the Telegram Bot HTTP API. We only need a couple of
// methods (sendMessage, getUpdates), so a dedicated package isn't worth the
// dependency.

const API_BASE = "https://api.telegram.org";

export class TelegramConfigError extends Error {}
export class TelegramApiError extends Error {}

export function getBotToken(): string {
  const t = process.env.TELEGRAM_ASSISTANT_BOT_TOKEN;
  if (!t) {
    throw new TelegramConfigError(
      "TELEGRAM_ASSISTANT_BOT_TOKEN is not set. Add it to .env (BotFather token).",
    );
  }
  return t;
}

export function getDefaultChatId(): string | null {
  return process.env.TELEGRAM_DEFAULT_CHAT_ID ?? null;
}

// Optional `name -> thread_id` mapping for Telegram forum topics.
// Format: TELEGRAM_TOPICS_JSON='{"bills":42,"bank":43}'. Used to surface the
// list of available topics in agent system prompts so the LLM can route
// signal-driven messages into specific topics. Bot API has no method to
// list forum topics, so we rely on this static mapping.
export function getTopicMap(): Record<string, number> {
  const raw = process.env.TELEGRAM_TOPICS_JSON;
  if (!raw) return {};
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    const out: Record<string, number> = {};
    for (const [name, id] of Object.entries(parsed as Record<string, unknown>)) {
      if (typeof id === "number" && Number.isFinite(id)) out[name] = id;
    }
    return out;
  } catch {
    return {};
  }
}

interface ApiEnvelope<T> {
  ok: boolean;
  description?: string;
  result?: T;
}

async function call<T>(method: string, body?: object): Promise<T> {
  const token = getBotToken();
  const res = await fetch(`${API_BASE}/bot${token}/${method}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: body ? JSON.stringify(body) : undefined,
  });
  const json = (await res.json()) as ApiEnvelope<T>;
  if (!json.ok) {
    throw new TelegramApiError(
      `Telegram ${method} failed (${res.status}): ${json.description ?? "unknown error"}`,
    );
  }
  return json.result as T;
}

export interface SendMessageInput {
  chatId: string | number;
  text: string;
  parseMode?: "MarkdownV2" | "HTML";
  messageThreadId?: number;
}

export interface SentMessage {
  message_id: number;
  date: number;
  chat: { id: number; type: string };
}

export async function sendMessage(input: SendMessageInput): Promise<SentMessage> {
  return call<SentMessage>("sendMessage", {
    chat_id: input.chatId,
    text: input.text,
    parse_mode: input.parseMode,
    message_thread_id: input.messageThreadId,
  });
}

export interface EditMessageInput {
  chatId: string | number;
  messageId: number;
  text: string;
  parseMode?: "MarkdownV2" | "HTML";
}

// Telegram returns the edited message on success. If the new text is
// identical to the old, the API throws "message is not modified" — we
// surface that as a TelegramApiError, callers can ignore.
export async function editMessageText(input: EditMessageInput): Promise<SentMessage> {
  return call<SentMessage>("editMessageText", {
    chat_id: input.chatId,
    message_id: input.messageId,
    text: input.text,
    parse_mode: input.parseMode,
  });
}

// Delete a message the bot sent. Telegram only lets a bot delete its own
// messages (and only within 48h). Used to clear ephemeral status bubbles.
export async function deleteMessage(
  chatId: string | number,
  messageId: number,
): Promise<void> {
  await call<true>("deleteMessage", {
    chat_id: chatId,
    message_id: messageId,
  });
}

export type ChatAction =
  | "typing"
  | "upload_photo"
  | "record_video"
  | "upload_video"
  | "record_voice"
  | "upload_voice"
  | "upload_document"
  | "choose_sticker"
  | "find_location"
  | "record_video_note"
  | "upload_video_note";

export async function sendChatAction(
  chatId: string | number,
  action: ChatAction,
  messageThreadId?: number,
): Promise<void> {
  await call<true>("sendChatAction", {
    chat_id: chatId,
    action,
    message_thread_id: messageThreadId,
  });
}

export interface UpdateChat {
  id: number;
  type: string;
  title?: string;
  first_name?: string;
  last_name?: string;
  username?: string;
}

export interface UpdateMessage {
  message_id: number;
  date: number;
  chat: UpdateChat;
  text?: string;
  from?: { id: number; is_bot: boolean; first_name?: string; username?: string };
  message_thread_id?: number;
  is_topic_message?: boolean;
}

export interface Update {
  update_id: number;
  message?: UpdateMessage;
  edited_message?: UpdateMessage;
  channel_post?: UpdateMessage;
}

export interface GetUpdatesInput {
  offset?: number;
  timeout?: number;
  allowed_updates?: string[];
}

export async function getUpdates(input: GetUpdatesInput = {}): Promise<Update[]> {
  return call<Update[]>("getUpdates", input);
}
