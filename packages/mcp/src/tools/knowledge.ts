import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { KnowledgeRepository } from "../services/knowledge";
import { jsonResult } from "../result";

export function registerKnowledgeTools(
  server: McpServer,
  knowledge: KnowledgeRepository,
): void {
  server.registerTool(
    "add_note",
    {
      title: "Save a note to the personal knowledge base",
      description:
        "Persist a freeform fact the user asked you to remember (\"запомни, " +
        "что …\", \"запиши …\", \"заметка: …\"). The note becomes semantically " +
        "searchable later via find_notes.\n\n" +
        "YOU generate the tags: pick 3–6 short, lowercase topical tags на свой " +
        "вкус — the things you'd later search this note by (people, topics, " +
        "objects), e.g. [\"роутер\", \"пароль\", \"wifi\"]. Tags are metadata " +
        "only: they help filtering and scanning, but recall runs over the note " +
        "TEXT, so write a self-contained `body` that names its subject " +
        "explicitly (\"Лёша платит за интернет 1-го числа\", not \"платит " +
        "1-го\"). Returns the new note id.",
      inputSchema: {
        body: z
          .string()
          .min(1)
          .describe(
            "The fact to remember, as a self-contained sentence including its " +
              "subject. This text is what semantic recall matches against.",
          ),
        tags: z
          .array(z.string().min(1))
          .max(12)
          .optional()
          .describe(
            "3–6 short lowercase topical tags you generate for this note. " +
              "Used for the optional overlap filter in find_notes, not for " +
              "semantic recall.",
          ),
        source: z
          .string()
          .optional()
          .describe('Optional provenance, e.g. "telegram".'),
      },
    },
    async ({ body, tags, source }) => {
      const result = await knowledge.addNote({ body, tags, source });
      return jsonResult({
        id: result.id,
        embedded: result.embedded,
        tags: result.tags,
      });
    },
  );

  server.registerTool(
    "find_notes",
    {
      title: "Semantic search over the personal knowledge base",
      description:
        "Recall notes saved with add_note by meaning, not exact wording " +
        '("что ты помнишь про роутер?", "когда Лёша платит за интернет?", ' +
        '"напомни пароль от роутера"). Returns the closest notes by semantic ' +
        "similarity to `query`, each with body, tags, source, created_at and " +
        "distance (lower = closer). Optionally pass `tags` to additionally " +
        "restrict to notes sharing at least one tag. This is the ONLY way to " +
        "read the knowledge base — use it whenever the user asks what you " +
        "know/remember about something personal.",
      inputSchema: {
        query: z
          .string()
          .min(1)
          .describe("Natural-language description of what to recall."),
        limit: z
          .number()
          .int()
          .min(1)
          .max(50)
          .optional()
          .describe("Max notes to return. Default 10."),
        tags: z
          .array(z.string().min(1))
          .optional()
          .describe(
            "Restrict to notes sharing at least one of these tags " +
              "(array overlap). Lowercase to match how tags are stored.",
          ),
      },
    },
    async ({ query, limit, tags }) => {
      const notes = await knowledge.findNotes({ query, k: limit, tags });
      return jsonResult({ count: notes.length, notes });
    },
  );
}
