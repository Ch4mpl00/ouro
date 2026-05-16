import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import {
  sendMessage,
  editMessageText,
  sendChatAction,
  getDefaultChatId,
  recordMessage,
  getChatHistory,
  startTyping,
  stopTyping,
} from "../services/telegram";
import { jsonResult } from "../result";

// Telegram chat ids are numbers, but the model sometimes emits them as
// strings and sometimes as numbers. Accept both at the schema layer and
// normalize to a single shape downstream — otherwise things like the
// typing-keepalive map key (`${chatId}:${threadId}`) end up with different
// string vs number keys and stopTyping silently misses.
const chatIdAsString = z
  .union([z.string(), z.number().int()])
  .transform((v) => String(v));

const chatIdAsNumber = z
  .union([z.string(), z.number().int()])
  .transform((v) => (typeof v === "string" ? Number(v) : v));

export function registerTelegramTools(server: McpServer): void {
  server.registerTool(
    "send_telegram_message",
    {
      title: "Send Telegram message",
      description:
        "Send a Telegram message via the assistant bot. If chatId is omitted, " +
        "TELEGRAM_DEFAULT_CHAT_ID env is used. Pass messageThreadId to send into " +
        "a specific forum topic (replies to a topic message must keep the same " +
        "messageThreadId so they land in the same topic; the system prompt " +
        "lists configured topic name → thread_id pairs if available). The " +
        "returned messageId can be persisted and passed to edit_telegram_message " +
        "later (e.g. to mark a bill as paid). The outgoing message is also " +
        "recorded in the local Telegram chat log so the conversation history " +
        "stays in sync.",
      inputSchema: {
        text: z.string().min(1).max(4096),
        chatId: chatIdAsString
          .optional()
          .describe("Telegram chat id. Falls back to TELEGRAM_DEFAULT_CHAT_ID if omitted."),
        messageThreadId: z
          .number()
          .int()
          .optional()
          .describe(
            "Forum topic thread_id. Required to reply inside a topic; omit for non-topic chats or the General topic.",
          ),
      },
    },
    async ({ text, chatId, messageThreadId }) => {
      const target = chatId ?? getDefaultChatId();
      if (!target) {
        throw new Error(
          "No chat target. Pass chatId, or set TELEGRAM_DEFAULT_CHAT_ID in .env (find your id with `pnpm telegram:get-chat-id`).",
        );
      }
      const sent = await sendMessage({ chatId: target, text, messageThreadId });
      // Outgoing message clears the bot's typing indicator client-side;
      // also stop our keep-alive so it doesn't bleed into the next session.
      stopTyping(target, messageThreadId);
      recordMessage({
        chatId: sent.chat.id,
        tgMessageId: sent.message_id,
        threadId: messageThreadId ?? null,
        role: "assistant",
        text,
      });
      return jsonResult({
        delivered: true,
        chatId: target,
        messageId: sent.message_id,
        messageThreadId: messageThreadId ?? null,
        date: new Date(sent.date * 1000).toISOString(),
      });
    },
  );

  server.registerTool(
    "edit_telegram_message",
    {
      title: "Edit Telegram message",
      description:
        "Edit a previously-sent Telegram message in place. Use to update a bill " +
        "notification when status changes (e.g. mark as PAID). messageId is the value " +
        "returned by send_telegram_message; chatId is the same chat the message was sent to.",
      inputSchema: {
        chatId: chatIdAsString.describe("Telegram chat id (the one the original message was sent to)."),
        messageId: z.number().int().describe("messageId returned by send_telegram_message."),
        text: z.string().min(1).max(4096),
      },
    },
    async ({ chatId, messageId, text }) => {
      const edited = await editMessageText({ chatId, messageId, text });
      return jsonResult({
        edited: true,
        chatId,
        messageId: edited.message_id,
        date: new Date(edited.date * 1000).toISOString(),
      });
    },
  );

  server.registerTool(
    "start_typing",
    {
      title: "Start typing indicator (auto-refresh until reply)",
      description:
        "Show a chat action indicator (typing by default) in a Telegram chat " +
        "and keep it alive. MCP re-sends the action every ~4s in the " +
        "background — call this ONCE at the start of a session, no need to " +
        "ping it on every reasoning round. The indicator clears " +
        "automatically when your `send_telegram_message` to the same chat/" +
        "thread is delivered. A safety TTL stops the keep-alive after " +
        "5 minutes if no message ever ships.",
      inputSchema: {
        chatId: chatIdAsString.describe("Telegram chat id."),
        action: z
          .enum([
            "typing",
            "upload_photo",
            "record_video",
            "upload_video",
            "record_voice",
            "upload_voice",
            "upload_document",
            "choose_sticker",
            "find_location",
            "record_video_note",
            "upload_video_note",
          ])
          .optional()
          .describe("Chat action to display. Defaults to 'typing'."),
        messageThreadId: z
          .number()
          .int()
          .optional()
          .describe("Forum topic thread_id (display the indicator inside a specific topic)."),
      },
    },
    async ({ chatId, action, messageThreadId }) => {
      await startTyping(chatId, action ?? "typing", messageThreadId);
      return jsonResult({ started: true });
    },
  );

  // Kept as an escape hatch for one-off, non-typing actions (e.g. a single
  // `upload_photo` blip right before posting an image). For typing during a
  // multi-step reasoning round, use `start_typing` — it self-refreshes.
  server.registerTool(
    "send_telegram_chat_action",
    {
      title: "Send a one-shot Telegram chat action (no auto-refresh)",
      description:
        "Send a single chat action ping (~5s lifespan, no keep-alive). " +
        "Prefer `start_typing` for the common 'show typing while I work' " +
        "case — this one is for one-off non-typing actions like a brief " +
        "`upload_photo` before sending an image.",
      inputSchema: {
        chatId: chatIdAsString.describe("Telegram chat id."),
        action: z
          .enum([
            "typing",
            "upload_photo",
            "record_video",
            "upload_video",
            "record_voice",
            "upload_voice",
            "upload_document",
            "choose_sticker",
            "find_location",
            "record_video_note",
            "upload_video_note",
          ])
          .describe("Chat action to display."),
        messageThreadId: z.number().int().optional(),
      },
    },
    async ({ chatId, action, messageThreadId }) => {
      await sendChatAction(chatId, action, messageThreadId);
      return jsonResult({ sent: true });
    },
  );

  server.registerTool(
    "get_telegram_chat_history",
    {
      title: "Get Telegram chat history",
      description:
        "Read the last N messages of a Telegram chat from the local log, in " +
        "chronological order. Pass threadId to scope to a single forum topic " +
        "(reply context for a topic message). Omit threadId to see all topics " +
        "interleaved.",
      inputSchema: {
        chatId: chatIdAsNumber.describe("Telegram chat id."),
        limit: z.number().int().min(1).max(500).optional().describe("Max messages. Default 50."),
        threadId: z
          .number()
          .int()
          .optional()
          .describe(
            "Forum topic thread_id. Restricts results to a single topic. Omit for unfiltered history.",
          ),
      },
    },
    async ({ chatId, limit, threadId }) => {
      const messages = getChatHistory(chatId, limit, threadId);
      return jsonResult({ messages });
    },
  );
}
