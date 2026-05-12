import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { listSignals } from "../services/signals";
import { setLastDreamingAt } from "../services/dreaming";
import { jsonResult } from "../result";

export function registerDreamingTools(server: McpServer): void {
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
      const rows = listSignals({ since, source, limit });
      return jsonResult({ count: rows.length, signals: rows });
    },
  );

  server.registerTool(
    "set_last_dreaming_at",
    {
      title: "Stamp the dreaming watermark",
      description:
        "Persist `last_dreaming_at` so the next dreaming session knows the " +
        "cutoff. Call this once at the end of a successful dreaming session, " +
        "with the timestamp shown in the signal content (the 'Now is:' value).",
      inputSchema: {
        timestamp: z.string().describe("ISO timestamp to store as last_dreaming_at."),
      },
    },
    async ({ timestamp }) => {
      setLastDreamingAt(timestamp);
      return jsonResult({ ok: true, lastDreamingAt: timestamp });
    },
  );
}
