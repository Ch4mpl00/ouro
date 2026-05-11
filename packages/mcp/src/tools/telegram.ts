import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import {
  sendMessage,
  editMessageText,
  sendChatAction,
  getDefaultChatId,
  recordMessage,
  getChatHistory,
} from "../services/telegram";
import { jsonResult } from "../result";

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
        chatId: z
          .string()
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
        chatId: z.string().describe("Telegram chat id (the one the original message was sent to)."),
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
    "send_telegram_chat_action",
    {
      title: "Send Telegram chat action (typing indicator etc)",
      description:
        "Send a transient chat action (typing, upload_photo, etc) to a Telegram chat. " +
        "Telegram displays the indicator for ~5 seconds, then it disappears. " +
        "Call this in parallel with other tool calls when you're about to do " +
        "time-consuming work, and again at the start of each subsequent reasoning " +
        "round so the indicator stays alive until you reply.",
      inputSchema: {
        chatId: z.string().describe("Telegram chat id."),
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
        messageThreadId: z
          .number()
          .int()
          .optional()
          .describe("Forum topic thread_id (display the indicator inside a specific topic)."),
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
        chatId: z.number().int().describe("Telegram chat id."),
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
