import { sql } from "drizzle-orm";
import { index, primaryKey, real, sqliteTable, text } from "drizzle-orm/sqlite-core";
import type { Observation } from "../trace-model";

// Drizzle schema for the agent's sqlite state (agent.db). The query layer
// (db/memory.ts, db/trace-store.ts) builds on this; migrations are generated
// from it with `pnpm db:generate:agent` and applied on boot in db/client.ts.
// Mirrors the MCP-side Drizzle setup (packages/mcp/src/db/pg/schema.ts), only
// for sqlite. The legacy `bills` table is intentionally dropped — it was a
// no-longer-populated leftover (see CLAUDE.md).

// A `datetime('now')` default expressed once so every table reads the same.
const nowDefault = sql`(datetime('now'))`;

// Freeform key-value store. Use for anything the agent wants to remember
// between sessions that doesn't fit a typed table — watermarks, last-seen
// markers. Value is a JSON-stringified payload by convention.
export const memory = sqliteTable("memory", {
  key: text("key").primaryKey(),
  value: text("value").notNull(),
  updatedAt: text("updated_at").notNull().default(nowDefault),
});

export type MemoryRow = typeof memory.$inferSelect;

// Local mirror of every agent run's trace. Written by the local-recorder
// tracer (tee'd alongside Langfuse) so the judge and the self-improvement loop
// read runs from here — fast, and independent of Langfuse uptime. `id` IS the
// Langfuse/OTel trace id, so scores written back to Langfuse link with zero
// mapping. The full observation tree is one JSON blob: the judge reads it
// whole, never queries individual steps.
export const traces = sqliteTable(
  "traces",
  {
    id: text("id").primaryKey(), // OTel/Langfuse trace id (hex)
    name: text("name").notNull(),
    source: text("source"), // signal.source (tags[0])
    skill: text("skill"), // resolved composer skill, nullable
    sessionId: text("session_id"), // signalLabel, e.g. scheduler:242
    tags: text("tags", { mode: "json" }).$type<string[]>().notNull().default(sql`'[]'`),
    input: text("input", { mode: "json" }), // json, nullable
    output: text("output", { mode: "json" }), // json (often null on workflow path)
    metadata: text("metadata", { mode: "json" }).$type<Record<string, unknown> | null>(),
    observations: text("observations", { mode: "json" })
      .$type<Observation[]>()
      .notNull()
      .default(sql`'[]'`),
    startedAt: text("started_at").notNull(), // trace root start (ISO)
    createdAt: text("created_at").notNull().default(nowDefault),
  },
  (t) => [index("traces_started").on(t.startedAt), index("traces_skill").on(t.skill)],
);

export type TraceRow = typeof traces.$inferSelect;

// One row per JUDGED NODE: (trace, observation, judge provider, prompt
// version). A node is one generative LLM observation — the planner generation,
// an llm_compose, or an llm_agent step — owned by exactly one skill (or the
// planner). Per-node scores localize the signal to the unit the improver
// patches (a skill), so the PK is the node, not the run. Axis scores live in
// numeric columns so the improver can filter/aggregate cheaply
// (WHERE composition < 0.7, GROUP BY skill); the rich payload (labels,
// rationale, evidence, faithfulness claims) is the `detail` JSON. Axes are
// owner-type specific: a planner node fills query_formulation/process, a
// composer/agent node fills coverage/composition/faithfulness — the others
// stay null. A null axis the rubric DID emit means the judge marked it n/a.
export const judgements = sqliteTable(
  "judgements",
  {
    traceId: text("trace_id")
      .notNull()
      .references(() => traces.id),
    observationId: text("observation_id").notNull(), // the judged node's observation id
    provider: text("provider").notNull(), // codex|openai
    promptVersion: text("prompt_version").notNull(), // e.g. n1
    nodeKind: text("node_kind").notNull(), // planner|compose|agent
    skill: text("skill").notNull(), // owner skill (planner for prompt-only)
    queryFormulation: real("query_formulation"),
    process: real("process"),
    coverage: real("coverage"),
    composition: real("composition"),
    faithfulness: real("faithfulness"),
    detail: text("detail", { mode: "json" }).notNull(), // scorecard (+ faithfulness)
    createdAt: text("created_at").notNull().default(nowDefault),
  },
  (t) => [
    primaryKey({ columns: [t.traceId, t.observationId, t.provider, t.promptVersion] }),
    // The improver aggregates low-score clusters per (skill, version) → patch.
    index("judgements_skill").on(t.skill, t.promptVersion),
  ],
);

export type JudgementRow = typeof judgements.$inferSelect;
