import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { Signals } from "../services/signals";
import { jsonResult } from "../result";

// "dreaming tools" is a historical name — only `list_signals` lives here
// now. The previous fire timestamp the dreaming skill needs is delivered
// directly in the signal content (header `Previous fire: …`), so no
// separate watermark KV / tool is required.
export function registerDreamingTools(server: McpServer, signals: Signals): void {
  server.registerTool(
    "list_signals",
    {
      title: "List past signals",
      description:
        "Read-only view of past signals (does not pop or mutate the queue). " +
        "Optional filters: `since` (ISO timestamp, returns signals created " +
        "after this), `source` (e.g. 'telegram', 'nashdom-bill'). Default " +
        "limit 200. Used by the dreaming skill to review what happened " +
        "since the previous reflection.",
      inputSchema: {
        since: z
          .string()
          .optional()
          .describe("ISO timestamp. Only signals with created_at > since are returned."),
        source: z.string().optional().describe("Restrict to a single signal source."),
        limit: z.number().int().min(1).max(2000).optional().describe("Max rows. Default 200."),
      },
    },
    async ({ since, source, limit }) => {
      // "YYYY-MM-DD HH:MM:SS" and full ISO both parse, so a `since` copied
      // from a signal or from a "Previous fire:" header both work; the
      // comparison is on real timestamps.
      const sinceDate = since === undefined ? undefined : parseSince(since);
      const rows = await signals.list({ since: sinceDate, source, limit });
      return jsonResult({ count: rows.length, signals: rows });
    },
  );
}

// A signal's created_at ("2026-10-02 09:00:00") is UTC without a zone.
function parseSince(since: string): Date {
  const text = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(since) ? `${since.replace(" ", "T")}Z` : since;
  const date = new Date(text);
  if (Number.isNaN(date.getTime())) throw new Error(`since: not a date: ${JSON.stringify(since)}`);
  return date;
}
