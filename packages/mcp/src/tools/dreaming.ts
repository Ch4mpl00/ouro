import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { listSignals } from "../services/signals";
import { listSkills, readSkill, writeSkill } from "../services/skills";
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
    "list_skills",
    {
      title: "List skill files",
      description:
        "List all skill files in `<repo-root>/skills/`. Returns name, size, " +
        "and last modified timestamp for each. Skill names map 1-to-1 to " +
        "signal sources — `skills/<source>.md` is the system prompt loaded " +
        "by the supervisor when handling a signal of that source.",
      inputSchema: {},
    },
    async () => {
      const skills = await listSkills();
      return jsonResult({ count: skills.length, skills });
    },
  );

  server.registerTool(
    "read_skill",
    {
      title: "Read a skill file",
      description: "Return the raw text of `skills/<name>.md`.",
      inputSchema: {
        name: z.string().describe("Skill name without .md extension (matches signal source)."),
      },
    },
    async ({ name }) => {
      const content = await readSkill(name);
      return jsonResult({ name, content, sizeBytes: Buffer.byteLength(content, "utf-8") });
    },
  );

  server.registerTool(
    "write_skill",
    {
      title: "Overwrite a skill file",
      description:
        "Replace the content of `skills/<name>.md` with the provided text. " +
        "Used by the dreaming skill to revise instructions based on observed " +
        "patterns. Sandboxed to the skills directory — cannot escape. " +
        "Read the existing skill first; only edit when the change is clearly " +
        "warranted by signals you've actually reviewed.",
      inputSchema: {
        name: z.string().describe("Skill name without .md extension."),
        content: z.string().min(1).describe("Full new content of the skill file."),
      },
    },
    async ({ name, content }) => {
      const result = await writeSkill(name, content);
      return jsonResult({ name, ...result });
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
