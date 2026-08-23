import { sql } from "drizzle-orm";
import {
  bigint,
  bigserial,
  index,
  integer,
  jsonb,
  pgTable,
  text,
  timestamp,
  unique,
  vector,
} from "drizzle-orm/pg-core";

// One row per piece of content (HN/Habr article, Telegram channel post).
// external_id is the natural id in its source: URL for HN/Habr,
// "<chat_id>:<tg_message_id>" for channels.

export const newsItems = pgTable(
  "news_items",
  {
    id: bigserial("id", { mode: "number" }).primaryKey(),
    source: text("source").notNull(),
    externalId: text("external_id").notNull(),
    title: text("title"),
    url: text("url"),
    body: text("body").notNull().default(""),
    metadata: jsonb("metadata").notNull().default(sql`'{}'::jsonb`),
    postedAt: timestamp("posted_at", { withTimezone: true }),
    fetchedAt: timestamp("fetched_at", { withTimezone: true })
      .notNull()
      .defaultNow(),
    embeddedAt: timestamp("embedded_at", { withTimezone: true }),
    embedding: vector("embedding", { dimensions: 1536 }),
  },
  // No ANN index on `embedding`: ivfflat with low row counts (<10k)
  // is effectively a random filter — small clusters + probes=1 default
  // means most relevant rows live in clusters the query never visits.
  // Sequential scan + cosineDistance on 1536-dim vectors is sub-ms at
  // this scale. Add ivfflat (or hnsw) back when the corpus crosses
  // ~50–100k rows; tune `lists` ≈ sqrt(rows) and bump probes per query.
  (t) => [
    unique("news_items_source_external_uniq").on(t.source, t.externalId),
    index("news_items_posted_at").on(t.postedAt.desc()),
    index("news_items_source_posted").on(t.source, t.postedAt.desc()),
  ],
);

export type NewsItemRow = typeof newsItems.$inferSelect;
export type NewsItemInsert = typeof newsItems.$inferInsert;

// Personal knowledge base: freeform notes the user asks the agent to
// remember ("запомни, что …"), recalled later by semantic search over
// `body` ("что ты помнишь про …"). Distinct from news_items (external
// harvested content) and agent.db memory (opaque internal KV).
//
// `tags` are LLM-generated at add time (the agent picks a handful "на
// свой вкус") and used only as structured metadata for now — stored,
// returned, and filterable via array-overlap (&&). They do NOT
// participate in the embedding: only `body` is vectorised. Folding tags
// into the embedded text later is a one-line change in the embed step
// (no schema migration) — revisit if recall over body alone underperforms.
export const knowledgeBaseNotes = pgTable(
  "knowledge_base_notes",
  {
    id: bigserial("id", { mode: "number" }).primaryKey(),
    body: text("body").notNull(),
    tags: text("tags")
      .array()
      .notNull()
      .default(sql`'{}'::text[]`),
    source: text("source"),
    createdAt: timestamp("created_at", { withTimezone: true })
      .notNull()
      .defaultNow(),
    updatedAt: timestamp("updated_at", { withTimezone: true })
      .notNull()
      .defaultNow(),
    embeddedAt: timestamp("embedded_at", { withTimezone: true }),
    embedding: vector("embedding", { dimensions: 1536 }),
  },
  // No ANN index on `embedding` — same rationale as news_items: a seq
  // scan over a small corpus is sub-ms; add ivfflat/hnsw past ~50–100k
  // rows. GIN on `tags` keeps the optional array-overlap filter cheap.
  (t) => [
    index("kb_notes_created_at").on(t.createdAt.desc()),
    index("kb_notes_tags").using("gin", t.tags),
  ],
);

export type KnowledgeBaseNoteRow = typeof knowledgeBaseNotes.$inferSelect;
export type KnowledgeBaseNoteInsert = typeof knowledgeBaseNotes.$inferInsert;

// ---------------------------------------------------------------------------
// Unified memory (.claude/tasks/unified-memory.md)
//
// Two read models — projects made of markdown documents, and flat facts —
// plus one search projection over both. Embeddings live ONLY in memory_index:
// news_items and knowledge_base_notes each carry their own copy of the
// embed/backfill dance, and a third would have made that a pattern.
// ---------------------------------------------------------------------------

export const projects = pgTable("memory_projects", {
  id: bigserial("id", { mode: "number" }).primaryKey(),
  slug: text("slug").notNull().unique(),
  title: text("title").notNull(),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  updatedAt: timestamp("updated_at", { withTimezone: true }).notNull().defaultNow(),
});

// A project is a flat folder of markdown documents (D5) — no fixed
// passport/roadmap columns, because the right shape is not known yet and the
// agent grows it bottom-up. `summary` is the registry line that stops
// notes.md / notes2.md / progress-new.md from multiplying.
export const projectDocs = pgTable(
  "memory_project_docs",
  {
    id: bigserial("id", { mode: "number" }).primaryKey(),
    projectId: bigint("project_id", { mode: "number" })
      .notNull()
      .references(() => projects.id, { onDelete: "cascade" }),
    name: text("name").notNull(),
    summary: text("summary"),
    bodyMd: text("body_md").notNull().default(""),
    // Bumped on every write. Every mutating op compares against it, which is
    // what makes concurrent agents safe without a lock.
    version: integer("version").notNull().default(1),
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
    updatedAt: timestamp("updated_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [unique("memory_docs_project_name_uniq").on(t.projectId, t.name)],
);

// One row per write of any shape. `body_before` is the whole previous
// document: it makes "roll roadmap.md back to v7" answerable and gives a
// failed mid-stack revert an exact fallback (D9).
export const docPatches = pgTable(
  "memory_doc_patches",
  {
    id: bigserial("id", { mode: "number" }).primaryKey(),
    docId: bigint("doc_id", { mode: "number" })
      .notNull()
      .references(() => projectDocs.id, { onDelete: "cascade" }),
    // Short random handle the agent quotes back ("revert pa:4f2a"). Random,
    // not sequential — a sequence invites guessing a neighbour's id.
    pid: text("pid").notNull(),
    kind: text("kind").notNull(),
    edits: jsonb("edits").notNull().default(sql`'[]'::jsonb`),
    bodyBefore: text("body_before").notNull().default(""),
    versionBefore: integer("version_before").notNull(),
    versionAfter: integer("version_after").notNull(),
    actor: text("actor").notNull(),
    // The utterance that caused the write ("отметь, что Dijkstra пройден") —
    // the one thing the document itself genuinely cannot contain (D3).
    rationale: text("rationale"),
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [
    unique("memory_patch_doc_pid_uniq").on(t.docId, t.pid),
    index("memory_patches_doc_created").on(t.docId, t.createdAt.desc()),
  ],
);

// Freeform facts — what knowledge_base_notes held, plus a state and an update
// path (D2). Nothing is deleted; `state` is how things decay (D7).
export const facts = pgTable(
  "memory_facts",
  {
    id: bigserial("id", { mode: "number" }).primaryKey(),
    body: text("body").notNull(),
    tags: text("tags").array().notNull().default(sql`'{}'::text[]`),
    source: text("source"),
    state: text("state").notNull().default("active"),
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
    updatedAt: timestamp("updated_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [index("memory_facts_tags").using("gin", t.tags)],
);

// The flat search projection (D8). Every read model lands here in
// comparable-sized chunks with the subject denormalised into `text`, so a
// fragment matches even though the document it came from stayed clean.
export const memoryIndex = pgTable(
  "memory_index",
  {
    id: bigserial("id", { mode: "number" }).primaryKey(),
    // The owning object: `doc:<slug>/<name>` or `fact:<id>`. Re-indexing
    // deletes by exact equality on this, so `fact:88` can never wipe
    // `fact:880` the way a LIKE prefix would.
    sourceRef: text("source_ref").notNull(),
    // The addressable chunk: `doc:<slug>/<name>#2`, `fact:88`. This is what
    // recall hands back and read_doc / get_fact resolve.
    ref: text("ref").notNull(),
    text: text("text").notNull(),
    tags: text("tags").array().notNull().default(sql`'{}'::text[]`),
    actor: text("actor"),
    state: text("state").notNull().default("active"),
    ts: timestamp("ts", { withTimezone: true }).notNull().defaultNow(),
    embeddedAt: timestamp("embedded_at", { withTimezone: true }),
    embedding: vector("embedding", { dimensions: 1536 }),
  },
  // No ANN index for the same reason as news_items: ivfflat under ~50k rows
  // is a random filter, and a seq scan over 1536-dim vectors is sub-ms here.
  (t) => [
    index("memory_index_source_ref").on(t.sourceRef),
    index("memory_index_state_ts").on(t.state, t.ts.desc()),
    index("memory_index_tags").using("gin", t.tags),
  ],
);

export type ProjectRow = typeof projects.$inferSelect;
export type ProjectDocRow = typeof projectDocs.$inferSelect;
export type DocPatchRow = typeof docPatches.$inferSelect;
export type FactRow = typeof facts.$inferSelect;
export type MemoryIndexRow = typeof memoryIndex.$inferSelect;
