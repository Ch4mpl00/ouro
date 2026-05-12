import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { listDialogs, listChannelPosts } from "../services/telegram/userbot";
import { getLastNewsReadAt, setLastNewsReadAt } from "../services/news-digest";
import { jsonResult } from "../result";

export function registerUserbotTools(server: McpServer): void {
  server.registerTool(
    "list_userbot_dialogs",
    {
      title: "List userbot dialogs (chats / channels)",
      description:
        "List the personal Telegram account's dialogs (channels, groups, " +
        "private chats) the userbot is subscribed to. Mostly useful for " +
        "discovery / debugging — for reading posts use `list_channel_posts`, " +
        "the userbot poller already harvests every subscribed channel in " +
        "the background. Requires `pnpm userbot:auth` to have been run once.",
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
    "list_channel_posts",
    {
      title: "Read harvested Telegram channel posts",
      description:
        "Query the local store of channel posts the userbot poller has " +
        "already collected. Returns posts with `posted_at > since` ordered " +
        "by `posted_at` ascending. Use this — not a live fetch — for digests " +
        "and ad-hoc reads; the background poller refreshes every ~30min so " +
        "data is at most that stale. Each row includes chat_id, chat_title, " +
        "chat_username, tg_message_id, posted_at (the original publication " +
        "date), text, views, forwards.",
      inputSchema: {
        since: z
          .string()
          .describe(
            "ISO timestamp. Only posts with posted_at > since are returned. " +
              "Typical use: now - 24h for a daily digest.",
          ),
        channel: z
          .string()
          .optional()
          .describe(
            "Restrict to one channel. Match is on chat_username or chat_id. " +
              "Omit to read across every subscribed channel.",
          ),
        limit: z
          .number()
          .int()
          .min(1)
          .max(2000)
          .optional()
          .describe("Max rows. Default 500."),
      },
    },
    async ({ since, channel, limit }) => {
      const posts = listChannelPosts({ since, channel, limit });
      return jsonResult({ count: posts.length, posts });
    },
  );

  server.registerTool(
    "get_last_news_read_at",
    {
      title: "Get the watermark for previously-read channel posts",
      description:
        "Returns the ISO timestamp the agent last stamped via " +
        "`set_last_news_read_at`. Use this as the `since` argument to " +
        "`list_channel_posts` so each digest only sees posts published " +
        "after the previous one (no overlap, no missed posts). Returns " +
        "{ lastReadAt: null } on the very first run — bootstrap with " +
        "now - 24h in that case.",
      inputSchema: {},
    },
    async () => {
      return jsonResult({ lastReadAt: getLastNewsReadAt() });
    },
  );

  server.registerTool(
    "set_last_news_read_at",
    {
      title: "Stamp the watermark for previously-read channel posts",
      description:
        "Persist the news-read watermark. Call this once at the end of a " +
        "successful digest / channel-posts read, with `now` (an ISO " +
        "timestamp). The NEXT digest will then pass this value to " +
        "`list_channel_posts` as `since`. Idempotent — overwrites the " +
        "previous value.",
      inputSchema: {
        timestamp: z.string().describe("ISO timestamp to store as last_read_at."),
      },
    },
    async ({ timestamp }) => {
      setLastNewsReadAt(timestamp);
      return jsonResult({ ok: true, lastReadAt: timestamp });
    },
  );
}
