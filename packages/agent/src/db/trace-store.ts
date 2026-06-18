import { and, desc, eq, isNull } from "drizzle-orm";
import type { Observation, Trace, TraceSummary } from "../trace-model";
import type { AgentDatabase } from "./client";
import { judgements, traces } from "./schema";

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
  getTrace(id: string): { trace: Trace; observations: Observation[] } | null;
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
      const trace: Trace = {
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
