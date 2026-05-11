import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import {
  fetchChannelMessages,
  listDialogs,
} from "../services/telegram/userbot";
import { jsonResult } from "../result";

export function registerUserbotTools(server: McpServer): void {
  server.registerTool(
    "list_userbot_dialogs",
    {
      title: "List userbot dialogs (chats / channels)",
      description:
        "List the personal Telegram account's dialogs (channels, groups, " +
        "private chats) the userbot is subscribed to. Use to discover what " +
        "channels are available before calling fetch_channel_messages. " +
        "Requires `pnpm userbot:auth` to have been run once.",
      inputSchema: {
        type: z
          .enum(["channel", "group", "user", "all"])
          .optional()
          .describe(
            "Filter dialogs by type. 'channel' is the right choice for the news digest (broadcast channels). Default 'all'.",
          ),
        limit: z
          .number()
          .int()
          .min(1)
          .max(500)
          .optional()
          .describe("Max dialogs to return. Default 100."),
      },
    },
    async ({ type, limit }) => {
      const dialogs = await listDialogs(limit);
      const filtered = type && type !== "all" ? dialogs.filter((d) => d.type === type) : dialogs;
      return jsonResult({ count: filtered.length, dialogs: filtered });
    },
  );

  server.registerTool(
    "fetch_channel_messages",
    {
      title: "Fetch messages from a Telegram channel",
      description:
        "Read recent messages from a Telegram channel/group via the userbot " +
        "account (gramjs/MTProto). The channel can be a username (with or " +
        "without `@`) or a t.me URL. Returns chronologically ordered messages " +
        "with id, date, text and engagement counters (views/forwards). Pass " +
        "`sinceMessageId` to get only messages newer than the boundary.",
      inputSchema: {
        channel: z.string().describe("Channel handle or t.me URL (e.g. 'tginsider' or '@tginsider')."),
        sinceMessageId: z
          .number()
          .int()
          .optional()
          .describe("Return only messages with id > sinceMessageId. Omit for the last `limit`."),
        limit: z
          .number()
          .int()
          .min(1)
          .max(200)
          .optional()
          .describe("Max messages. Default 30."),
      },
    },
    async ({ channel, sinceMessageId, limit }) => {
      const result = await fetchChannelMessages({ channel, sinceMessageId, limit });
      return jsonResult({
        channel: result.channel,
        count: result.messages.length,
        messages: result.messages,
      });
    },
  );

}
