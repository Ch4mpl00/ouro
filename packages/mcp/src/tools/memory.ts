import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { MemoryError, type MemoryService } from "../services/memory";
import { MEMORY_STATES } from "../services/memory/types";
import { jsonResult } from "../result";

// Unified memory, exposed to every agent (.claude/tasks/unified-memory.md).
//
// Two shapes behind one search: projects made of markdown documents, and flat
// facts. The agent's flow is deliberately two-step — `recall` finds *where*
// the answer lives and returns refs, then `read_doc` / `get_fact` loads the
// whole thing. Snippets alone were what made the planner fail on GAIA.
//
// Expected failures come back as `{ ok: false, error: <code>, … }` with the
// data needed to retry (the current version, the occurrence count, the exact
// literal that would have matched). They are results, not exceptions: an
// agent can act on a code, but not on a stack trace.

export interface MemoryToolDeps {
  memory: MemoryService;
  // Who is writing. Stamped onto every patch for audit and filtering, never
  // for access control (D6) — one shared space, many agents.
  actor: string;
}

const stateEnum = z.enum(MEMORY_STATES);

export function registerMemoryTools(server: McpServer, deps: MemoryToolDeps): void {
  const { memory, actor } = deps;

  // Every handler funnels through this: a MemoryError becomes a structured
  // result the model can act on, anything else stays an exception.
  const run = async (fn: () => Promise<unknown>): Promise<ReturnType<typeof jsonResult>> => {
    try {
      return jsonResult({ ok: true, ...(asObject(await fn())) });
    } catch (err) {
      if (err instanceof MemoryError) {
        return jsonResult({ ok: false, error: err.code, message: err.message, ...err.details });
      }
      throw err;
    }
  };

  server.registerTool(
    "recall",
    {
      title: "Search everything the agents remember",
      description:
        "Semantic search across ALL shared memory — project documents and " +
        "standalone facts alike. Use it whenever the user refers to something " +
        'from the past ("что там у нас было по X", "напомни про Y") and ' +
        "before starting work that might already have a project. Archived facts " +
        "are intentionally excluded from this ordinary recall path.\n\n" +
        "Returns REFS, not full content: `doc:<project>/<file>#<chunk>` or " +
        "`fact:<id>`. Follow the interesting ones with read_doc / get_fact — " +
        "the snippet is for choosing, the document is for answering.",
      inputSchema: {
        query: z.string().min(1).describe("What to look for, in natural language."),
        limit: z.number().int().min(1).max(50).optional().describe("Max hits (default 10)."),
        tags: z.array(z.string().min(1)).optional().describe("Keep only hits carrying one of these tags."),
      },
    },
    async ({ query, limit, tags }) =>
      // Public recall is deliberately active-only. Letting the model choose
      // lifecycle states made ordinary queries opt into archived facts and
      // then resolve them through get_fact, defeating archive as a discovery
      // boundary. The service keeps its states filter for a future explicit
      // history/admin surface; this general-purpose tool never forwards it.
      run(async () => ({ hits: await memory.recall({ query, limit, tags }) })),
  );

  server.registerTool(
    "remember",
    {
      title: "Store a standalone fact",
      description:
        'Persist a freeform fact the user asked you to remember ("запомни, что …"). ' +
        "For anything with ongoing progress use a project document instead — a " +
        "fact is a single self-contained statement, not a running log.\n\n" +
        "YOU generate the tags: 3–6 short lowercase topical words. Recall runs " +
        "over the TEXT, so write a body that names its own subject (\"Лёша " +
        'платит за интернет 1-го числа", not "платит 1-го").',
      inputSchema: {
        body: z.string().min(1).describe("The fact, as a self-contained sentence including its subject."),
        tags: z.array(z.string().min(1)).max(12).optional().describe("3–6 short lowercase topical tags."),
        source: z.string().optional().describe('Optional provenance, e.g. "telegram".'),
      },
    },
    async ({ body, tags, source }) => run(async () => ({ fact: await memory.remember({ body, tags, source }) })),
  );

  server.registerTool(
    "get_fact",
    {
      title: "Read one fact in full",
      description: "Load a fact by id — the read half of a `fact:<id>` ref returned by recall.",
      inputSchema: { id: z.number().int().positive().describe("Fact id, the number in a fact:<id> ref.") },
    },
    async ({ id }) => run(async () => ({ fact: await memory.getFact(id) })),
  );

  server.registerTool(
    "update_fact",
    {
      title: "Correct a fact or retire it",
      description:
        "Change a fact's text, tags or lifecycle state. Nothing is ever deleted: " +
        'mark a finished or obsolete item `done` / `archived` and it drops out of ' +
        "the default recall while staying findable on request.",
      inputSchema: {
        id: z.number().int().positive(),
        body: z.string().min(1).optional().describe("Replacement text."),
        tags: z.array(z.string().min(1)).max(12).optional().describe("Replacement tags (not merged)."),
        state: stateEnum.optional().describe("active = current, done = finished, archived = out of the way."),
      },
    },
    async ({ id, body, tags, state }) =>
      run(async () => ({ fact: await memory.updateFact({ id, body, tags, state }) })),
  );

  server.registerTool(
    "list_memory",
    {
      title: "List projects, or the documents in one",
      description:
        "Without `project`: every project. With it: that project's documents, " +
        "each with a one-line summary, version and size.\n\n" +
        "ALWAYS call this before creating a document. It is what keeps a " +
        "project from growing notes.md, notes2.md and progress-new.md until " +
        "nobody knows which one is current.",
      inputSchema: {
        project: z.string().optional().describe("Project slug. Omit to list all projects."),
      },
    },
    async ({ project }) =>
      run(async () =>
        project === undefined
          ? { projects: await memory.listProjects() }
          : await memory.listDocs(project),
      ),
  );

  server.registerTool(
    "create_project",
    {
      title: "Start a new project",
      description:
        "Create an empty project — a folder of markdown documents with progress, " +
        "e.g. preparing for an interview on a topic. Check list_memory first; " +
        "reusing an existing project is almost always right.",
      inputSchema: {
        slug: z.string().min(1).describe('Lowercase id, hyphens only, e.g. "leetcode-graphs".'),
        title: z.string().min(1).describe("Human-readable name."),
      },
    },
    async ({ slug, title }) => run(async () => ({ project: await memory.createProject({ slug, title }) })),
  );

  server.registerTool(
    "read_doc",
    {
      title: "Read a project document",
      description:
        "Return a document's full markdown plus its `version`. You need that " +
        "version to patch it, and the text has to be fresh or your quoted " +
        "`old` strings won't match. Read immediately before writing.",
      inputSchema: {
        project: z.string().min(1),
        doc: z.string().min(1).describe('Document filename, e.g. "roadmap.md".'),
      },
    },
    async ({ project, doc }) => run(async () => ({ doc: await memory.readDoc(project, doc) })),
  );

  server.registerTool(
    "append_doc",
    {
      title: "Add text to the end of a document (safe default)",
      description:
        "Append to a document, or to one section of it. THE PREFERRED WRITE: it " +
        "cannot destroy existing text and needs no prior read or version.\n\n" +
        "Use it for anything that is a record of what happened — progress " +
        "entries, notes, mistakes. Progress is history: correct an old entry by " +
        "appending a correction, never by editing the past.",
      inputSchema: {
        project: z.string().min(1),
        doc: z.string().min(1),
        text: z.string().min(1).describe("Markdown to append. One blank line is inserted before it."),
        under_heading: z
          .string()
          .optional()
          .describe('Append at the end of this section instead of the file, e.g. "Прогресс".'),
        rationale: z.string().optional().describe("The user's own words that prompted this write."),
      },
    },
    async ({ project, doc, text, under_heading, rationale }) =>
      run(() => memory.appendDoc({ project, doc, text, underHeading: under_heading, actor, rationale })),
  );

  server.registerTool(
    "patch_doc",
    {
      title: "Change existing text in a document",
      description:
        "Apply search/replace edits. Each `old` must appear EXACTLY ONCE — copy " +
        "it verbatim from a fresh read_doc, including punctuation, dashes and " +
        "ё. `new: \"\"` deletes. All edits apply or none do.\n\n" +
        "Failures come back with what you need to fix them: `version_conflict` " +
        "gives the current version, an ambiguous quote gives the match count, " +
        "and a miss gives the literal from the document that nearly matched.",
      inputSchema: {
        project: z.string().min(1),
        doc: z.string().min(1),
        expected_version: z.number().int().min(1).describe("Version from the read_doc you just did."),
        edits: z
          .array(
            z.object({
              old: z.string().min(1).describe("Exact text to find, unique in the document."),
              new: z.string().describe('Replacement. Empty string deletes.'),
            }),
          )
          .min(1),
        rationale: z.string().optional().describe("The user's own words that prompted this change."),
      },
    },
    async ({ project, doc, expected_version, edits, rationale }) =>
      run(() => memory.patchDoc({ project, doc, expectedVersion: expected_version, edits, actor, rationale })),
  );

  server.registerTool(
    "write_doc",
    {
      title: "Create a document, or replace one wholesale",
      description:
        "Create a new document, or overwrite an existing one entirely. THE ONLY " +
        "OP THAT CAN LOSE CONTENT — prefer append_doc for additions and " +
        "patch_doc for changes; use this to create, or when the user explicitly " +
        "asks to rewrite from scratch.\n\n" +
        "Creating: omit expected_version. Overwriting: read_doc first and pass " +
        "its version. Always give a `summary` — it is the line other agents see " +
        "in list_memory.",
      inputSchema: {
        project: z.string().min(1),
        doc: z.string().min(1).describe('Filename, e.g. "roadmap.md".'),
        body: z.string().describe("Full markdown content."),
        summary: z.string().optional().describe("One line describing what this document is for."),
        expected_version: z
          .number()
          .int()
          .min(0)
          .optional()
          .describe("Required when the document already exists. Omit when creating."),
        rationale: z.string().optional(),
      },
    },
    async ({ project, doc, body, summary, expected_version, rationale }) =>
      run(() =>
        memory.writeDoc({ project, doc, body, summary, expectedVersion: expected_version, actor, rationale }),
      ),
  );

  server.registerTool(
    "doc_history",
    {
      title: "Who changed a document, when and why",
      description:
        "List a document's patches, newest first: patch id, kind, actor, the " +
        "rationale recorded at the time, and the versions it moved between. " +
        "Patch ids come from here — never invent one.",
      inputSchema: {
        project: z.string().min(1),
        doc: z.string().min(1),
        limit: z.number().int().min(1).max(100).optional().describe("Default 20."),
      },
    },
    async ({ project, doc, limit }) =>
      run(async () => ({ history: await memory.history(project, doc, limit) })),
  );

  server.registerTool(
    "revert_patch",
    {
      title: "Undo a recorded change",
      description:
        "Undo one patch from doc_history. The newest patch always reverts " +
        "exactly. An older one is undone in place when later patches left its " +
        "text alone; when they didn't, the call fails and tells you which " +
        "version a rollback would restore.\n\n" +
        "`rollback: true` then restores the whole document to the state before " +
        "that patch, DISCARDING everything written after it — only do that when " +
        "the user asked for it. Reverting is itself recorded; history is never " +
        "rewritten.",
      inputSchema: {
        project: z.string().min(1),
        doc: z.string().min(1),
        patch_id: z.string().min(1).describe("Patch id from doc_history, e.g. pa:4f2a1c."),
        rollback: z
          .boolean()
          .optional()
          .describe("Discard everything after this patch and restore the document as it was before it."),
      },
    },
    async ({ project, doc, patch_id, rollback }) =>
      run(() => memory.revert({ project, doc, patchId: patch_id, rollback, actor })),
  );
}

// Handlers return either a plain payload object or a WriteResult; both are
// spread into the envelope so the JSON stays flat for the model.
function asObject(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" ? (value as Record<string, unknown>) : { value };
}
