import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { listDialogs } from "../services/telegram/userbot";
import { jsonResult } from "../result";

export function registerUserbotTools(server: McpServer): void {
  server.registerTool(
    "list_userbot_dialogs",
    {
      title: "List userbot dialogs (chats / channels)",
      description:
        "List the personal Telegram account's dialogs (channels, groups, " +
        "private chats) the userbot is subscribed to. Mostly useful for " +
        "discovery / debugging — to read channel posts use `list_news` " +
        "with source='channel'; the news poller harvests every subscribed " +
        "channel in the background. Requires `pnpm userbot:auth` to have " +
        "been run once.",
      inputSchema: {
        type: z
          .enum(["channel", "group", "user", "all"])
          .optional()
          .describe(
            "Filter dialogs by type. 'channel' is the right choice for the " +
              "news digest (broadcast channels). Default 'all'.",
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
}
