import path from "node:path";
import Database from "better-sqlite3";
import { and, desc, eq, isNull, sql } from "drizzle-orm";
import { drizzle, type BetterSQLite3Database } from "drizzle-orm/better-sqlite3";
import { migrate } from "drizzle-orm/better-sqlite3/migrator";
import { index, primaryKey, real, sqliteTable, text } from "drizzle-orm/sqlite-core";
import type { Observation, TraceRecord, TraceSummary } from "./tracing";

// The agent's own persistence, end to end: the Drizzle schema for agent.db,
// the handle factory that applies pending migrations on boot, and the three
// stores built on top of that handle.
//
// Read top to bottom — each section only depends on the ones above it:
//
//   schema           the sqlite tables — memory, traces, judgements,
//                    improver_state — plus the row types inferred from them
//   client           createAgentDb: open the file, set pragmas, migrate,
//                    hand back a typed Drizzle handle
//   memory store     the freeform KV the agent remembers between sessions
//   trace store      the local mirror of runs + their per-node judgements
//   improver store   closed-loop improver state, one row per (skill, axis)
//
// What stays outside this file: `db/migrations/` — drizzle-kit's generated SQL
// and its `meta/` journal. That is build output, not source we hand-edit; the
// client section resolves the folder by path at boot and `pnpm db:generate:agent`
// regenerates it by diffing the schema section below against the journal.
//
// The only non-library import is ./tracing, for the observation/trace
// shapes the traces table stores and the trace store returns.

// ═══════════════════════════════════════════════════════════════════
// Schema
// ═══════════════════════════════════════════════════════════════════

// Drizzle schema for the agent's sqlite state (agent.db). The query layer
// (the memory / trace / improver store sections below) builds on this;
// migrations are generated from it with `pnpm db:generate:agent` and applied
// on boot in the client section.
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

// Closed-loop improver state (Phase 3, п3), one row per (skill, axis). The cron
// worker watermarks each cycle's outcome here, and — crucially — when it SHIPS a
// patch it records the pre-ship baseline (the axis's recent mean) + the shipped
// lesson + a "pending" monitor status. On later runs it gathers the axis scores
// of traces that ran AFTER the ship and AUTO-REVERTS (removes the lesson) if the
// live trend fell below the baseline: the gate can be fooled, prod is ground
// truth. While a ship is "pending" (too few post-ship traces yet) the worker
// does NOT author a new lesson — one change at a time, so each ship's effect is
// isolated and attributable.
export const improverState = sqliteTable(
  "improver_state",
  {
    skill: text("skill").notNull(),
    axis: text("axis").notNull(),
    lastAttemptAt: text("last_attempt_at").notNull().default(nowDefault),
    lastOutcome: text("last_outcome").notNull(), // no-candidates|no-fix|rejected|shipped|reverted|kept
    // Set when the last attempt SHIPPED; null once the ship is reverted/settled
    // into the body's history (we only actively monitor the most recent ship).
    shippedAt: text("shipped_at"),
    shippedLesson: text("shipped_lesson"), // the exact appended block, for surgical revert
    baselineMean: real("baseline_mean"), // pre-ship recent axis mean
    baselineN: real("baseline_n"), // how many nodes that mean averaged
    monitorStatus: text("monitor_status"), // pending|kept|null (no live ship)
  },
  (t) => [primaryKey({ columns: [t.skill, t.axis] })],
);

export type ImproverStateRow = typeof improverState.$inferSelect;

// The tables collected under one name. `import * as schema from "./schema"`
// used to produce this namespace; drizzle() takes it as its `schema` option and
// `typeof schema` types the handle, so it is spelled out now that the schema
// and the client share a file.
export const schema = { memory, traces, judgements, improverState };



// ═══════════════════════════════════════════════════════════════════
// Client
// ═══════════════════════════════════════════════════════════════════

// `drizzle()` augments the base class with `$client` (the raw better-sqlite3
// handle, used for `.close()`); the base type alone doesn't carry it.

// Drizzle handle for the agent's domain state (memory KV + the local trace /
// judgement mirror), in agent.db. Built once in the composition root
// (supervisor main / a script's main) and passed down — no module-level
// singleton, per the workspace DI rules. Mirrors the MCP-side Drizzle setup
// (packages/mcp/src/db/pg/client.ts), only for sqlite.

export type AgentDatabase = BetterSQLite3Database<typeof schema> & {
  $client: Database.Database;
};

// Resolved lazily rather than at module scope: `pnpm db:generate:agent` loads
// this file as CJS to read the schema section above, and `import.meta.dirname`
// is undefined there. Inside a function it only ever runs under a real ESM
// import. (When the schema and the client were separate files, drizzle-kit
// never loaded this code at all.)
function defaultDbPath(): string {
  return path.resolve(import.meta.dirname, "../data/agent.db");
}
// Migrations live in source (baked into the image), NOT under the agent-data
// volume — so a schema change actually ships, instead of being shadowed by the
// volume's stale copy the way the old data/schema.sql was.
function migrationsDir(): string {
  return path.resolve(import.meta.dirname, "./db/migrations");
}

export function createAgentDb(dbPath?: string): AgentDatabase {
  const sqlite = new Database(dbPath ?? process.env.AGENT_DB_PATH ?? defaultDbPath());
  sqlite.pragma("journal_mode = WAL");
  sqlite.pragma("foreign_keys = ON");
  // Several processes (supervisor, judge-worker) open the same DB and migrate
  // at startup; a busy_timeout lets the loser of the migration race wait out
  // the writer's transaction instead of throwing SQLITE_BUSY.
  sqlite.pragma("busy_timeout = 5000");

  const db = drizzle(sqlite, { schema });
  // Idempotent: applies any pending migrations, no-op once up to date. Sync
  // for better-sqlite3, so callers just get a ready-to-use handle.
  migrate(db, { migrationsFolder: migrationsDir() });
  return db;
}


// ═══════════════════════════════════════════════════════════════════
// Memory store
// ═══════════════════════════════════════════════════════════════════

// Agent-side memory KV. Lives in the `memory` table of `agent.db`. This is the
// freeform store for anything the agent wants to remember between sessions
// that doesn't fit a typed table — watermarks, last-seen markers, small
// notes. Distinct from MCP-side `tokens.db`, which holds integration
// state (OAuth tokens, queues, caches) the MCP process owns.

export interface MemoryStore {
  get(key: string): string | null;
  set(key: string, value: string): void;
}

export function createMemoryStore(db: AgentDatabase): MemoryStore {
  return {
    get(key) {
      const row = db
        .select({ value: memory.value })
        .from(memory)
        .where(eq(memory.key, key))
        .get();
      return row?.value ?? null;
    },
    set(key, value) {
      db.insert(memory)
        .values({ key, value })
        .onConflictDoUpdate({
          target: memory.key,
          set: { value, updatedAt: sql`(datetime('now'))` },
        })
        .run();
    },
  };
}

// Well-known keys injected into the session context block. Keep them here
// so writers and the supervisor agree on naming.
export const MEMORY_KEYS = {
  newsLastReadAt: "news_digest.last_read_at",
} as const;


// ═══════════════════════════════════════════════════════════════════
// Trace store
// ═══════════════════════════════════════════════════════════════════

// Local mirror of agent runs + their judgements, in agent.db. The
// local-recorder tracer writes traces here (tee'd with Langfuse); the judge
// reads them back through the TraceSource interface, and writes scores back
// here for the self-improvement loop. JSON columns are Drizzle `mode: "json"`,
// so callers pass/receive plain objects — (de)serialization is the column's
// job, not the store's.

// What the recorder hands us on trace.end(). `id` is the Langfuse/OTel trace
// id, so it doubles as the cross-system link for scores.
export interface StoredTraceInput {
  id: string;
  name: string;
  source: string | null;
  skill: string | null;
  sessionId: string | null;
  tags: string[];
  input: unknown;
  output: unknown;
  metadata: Record<string, unknown> | null;
  observations: Observation[];
  startedAt: string;
}

// One judged node: its identity (trace + observation), owner attribution
// (node kind + skill), the numeric axis scores (null = not emitted by this
// node's rubric, or marked n/a), and the rich payload.
export interface JudgementInput {
  traceId: string;
  observationId: string;
  nodeKind: string;
  skill: string;
  provider: string;
  promptVersion: string;
  scores: {
    query_formulation: number | null;
    process: number | null;
    coverage: number | null;
    composition: number | null;
    faithfulness: number | null;
  };
  detail: unknown;
}

// One judged node read back for the improver: identity + numeric axis scores +
// the rich payload. Same shape as JudgementInput minus the filter keys, plus the
// owning trace's startedAt — the run's wall-clock time, which the improver uses
// to draw the cluster from a RECENT window (fix current failures, not old ones)
// while the holdout is all-time.
export interface JudgementRecord {
  traceId: string;
  observationId: string;
  nodeKind: string;
  skill: string;
  scores: {
    query_formulation: number | null;
    process: number | null;
    coverage: number | null;
    composition: number | null;
    faithfulness: number | null;
  };
  detail: unknown;
  startedAt: string;
}

export interface TraceStore {
  writeTrace(t: StoredTraceInput): void;
  // Read-back in the canonical {trace, observations} shape (same as a Langfuse
  // fetch), so the judge's material assembly is source-agnostic.
  getTrace(id: string): { trace: TraceRecord; observations: Observation[] } | null;
  // Newest-first, mirrors fetchRecentTraces. Optionally only traces lacking a
  // judgement for (provider, promptVersion) — the local replacement for the
  // memory-KV dedup + age window.
  listRecent(limit: number, unjudgedFor?: { provider: string; promptVersion: string }): TraceSummary[];
  writeJudgement(j: JudgementInput): void;
  // Every judged node for a (skill, provider, promptVersion) — the improver's
  // corpus. It clusters the low scorers and holds out the high ones in code.
  listJudgements(filter: { skill: string; provider: string; promptVersion: string }): JudgementRecord[];
  // Distinct skills that have any judgement for (provider, promptVersion) — the
  // cron improver iterates these (× each axis) instead of a hardcoded list.
  listJudgedSkills(filter: { provider: string; promptVersion: string }): string[];
}

export function createTraceStore(db: AgentDatabase): TraceStore {
  return {
    writeTrace(t) {
      const row = {
        id: t.id,
        name: t.name,
        source: t.source,
        skill: t.skill,
        sessionId: t.sessionId,
        tags: t.tags,
        input: t.input ?? null,
        output: t.output ?? null,
        metadata: t.metadata,
        observations: t.observations,
        startedAt: t.startedAt,
      };
      db.insert(traces)
        .values(row)
        .onConflictDoUpdate({
          target: traces.id,
          // Re-record everything but the immutable id / created_at.
          set: {
            name: row.name,
            source: row.source,
            skill: row.skill,
            sessionId: row.sessionId,
            tags: row.tags,
            input: row.input,
            output: row.output,
            metadata: row.metadata,
            observations: row.observations,
            startedAt: row.startedAt,
          },
        })
        .run();
    },

    getTrace(id) {
      const row = db.select().from(traces).where(eq(traces.id, id)).get();
      if (!row) return null;
      const observations = row.observations ?? [];
      const trace: TraceRecord = {
        id: row.id,
        name: row.name,
        sessionId: row.sessionId,
        timestamp: row.startedAt,
        input: row.input,
        output: row.output,
        metadata: row.metadata ?? null,
        observations,
        latency: 0,
        totalCost: 0,
        tags: row.tags,
      };
      return { trace, observations };
    },

    listRecent(limit, unjudgedFor) {
      const cols = {
        id: traces.id,
        name: traces.name,
        tags: traces.tags,
        startedAt: traces.startedAt,
      };
      const rows = unjudgedFor
        ? // LEFT JOIN + IS NULL = traces with no judgement row for this
          // (provider, version). One query, no per-trace round-trip.
          db
            .select(cols)
            .from(traces)
            .leftJoin(
              judgements,
              and(
                eq(judgements.traceId, traces.id),
                eq(judgements.provider, unjudgedFor.provider),
                eq(judgements.promptVersion, unjudgedFor.promptVersion),
              ),
            )
            .where(isNull(judgements.traceId))
            .orderBy(desc(traces.startedAt))
            .limit(limit)
            .all()
        : db.select(cols).from(traces).orderBy(desc(traces.startedAt)).limit(limit).all();

      return rows.map((row) => ({
        id: row.id,
        name: row.name,
        timestamp: row.startedAt,
        tags: row.tags,
      }));
    },

    writeJudgement(j) {
      const scores = {
        queryFormulation: j.scores.query_formulation,
        process: j.scores.process,
        coverage: j.scores.coverage,
        composition: j.scores.composition,
        faithfulness: j.scores.faithfulness,
      };
      db.insert(judgements)
        .values({
          traceId: j.traceId,
          observationId: j.observationId,
          provider: j.provider,
          promptVersion: j.promptVersion,
          nodeKind: j.nodeKind,
          skill: j.skill,
          ...scores,
          detail: j.detail,
        })
        .onConflictDoUpdate({
          target: [
            judgements.traceId,
            judgements.observationId,
            judgements.provider,
            judgements.promptVersion,
          ],
          set: { nodeKind: j.nodeKind, skill: j.skill, ...scores, detail: j.detail },
        })
        .run();
    },

    listJudgements(filter) {
      const rows = db
        .select({ j: judgements, startedAt: traces.startedAt })
        .from(judgements)
        .innerJoin(traces, eq(judgements.traceId, traces.id))
        .where(
          and(
            eq(judgements.skill, filter.skill),
            eq(judgements.provider, filter.provider),
            eq(judgements.promptVersion, filter.promptVersion),
          ),
        )
        .all();
      return rows.map(({ j, startedAt }) => ({
        traceId: j.traceId,
        observationId: j.observationId,
        nodeKind: j.nodeKind,
        skill: j.skill,
        scores: {
          query_formulation: j.queryFormulation,
          process: j.process,
          coverage: j.coverage,
          composition: j.composition,
          faithfulness: j.faithfulness,
        },
        detail: j.detail,
        startedAt,
      }));
    },

    listJudgedSkills(filter) {
      return db
        .selectDistinct({ skill: judgements.skill })
        .from(judgements)
        .where(
          and(
            eq(judgements.provider, filter.provider),
            eq(judgements.promptVersion, filter.promptVersion),
          ),
        )
        .all()
        .map((r) => r.skill);
    },
  };
}


// ═══════════════════════════════════════════════════════════════════
// Improver store
// ═══════════════════════════════════════════════════════════════════

// Persistent state for the closed-loop improver (Phase 3, п3): one row per
// (skill, axis), holding the last cycle's outcome and — when the last attempt
// shipped — the live-monitor bookkeeping (pre-ship baseline + shipped lesson +
// status). Kept in its own tiny store, not bolted onto TraceStore: the improver
// owns this state, the trace mirror does not. See `improverState` in the
// schema section above.

export type ImproverState = ImproverStateRow;

// The mutable fields a cycle writes back. Identity (skill, axis) is separate.
export interface ImproverStateUpdate {
  lastOutcome: string;
  shippedAt: string | null;
  shippedLesson: string | null;
  baselineMean: number | null;
  baselineN: number | null;
  monitorStatus: string | null;
}

export interface ImproverStore {
  get(skill: string, axis: string): ImproverState | null;
  upsert(skill: string, axis: string, lastAttemptAt: string, patch: ImproverStateUpdate): void;
}

export function createImproverStore(db: AgentDatabase): ImproverStore {
  return {
    get(skill, axis) {
      return (
        db
          .select()
          .from(improverState)
          .where(and(eq(improverState.skill, skill), eq(improverState.axis, axis)))
          .get() ?? null
      );
    },

    upsert(skill, axis, lastAttemptAt, patch) {
      const row = { skill, axis, lastAttemptAt, ...patch };
      db.insert(improverState)
        .values(row)
        .onConflictDoUpdate({
          target: [improverState.skill, improverState.axis],
          set: {
            lastAttemptAt: row.lastAttemptAt,
            lastOutcome: row.lastOutcome,
            shippedAt: row.shippedAt,
            shippedLesson: row.shippedLesson,
            baselineMean: row.baselineMean,
            baselineN: row.baselineN,
            monitorStatus: row.monitorStatus,
          },
        })
        .run();
    },
  };
}
