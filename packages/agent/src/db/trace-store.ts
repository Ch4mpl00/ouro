import type Database from "better-sqlite3";
import type { Observation, Trace, TraceSummary } from "../trace-model";

// Local mirror of agent runs + their judgements, in agent.db. The
// local-recorder tracer writes traces here (tee'd with Langfuse); the judge
// reads them back through the TraceSource interface, and writes scores back
// here for the self-improvement loop. The store owns serialization — callers
// pass/receive plain objects, the JSON columns stay an implementation detail.

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
}

interface TraceRow {
  id: string;
  name: string;
  source: string | null;
  skill: string | null;
  session_id: string | null;
  tags: string;
  input: string | null;
  output: string | null;
  metadata: string | null;
  observations: string;
  started_at: string;
}

function parseJson<T>(value: string | null, fallback: T): T {
  if (value === null) return fallback;
  try {
    return JSON.parse(value) as T;
  } catch {
    return fallback;
  }
}

export function createTraceStore(db: Database.Database): TraceStore {
  const upsertTrace = db.prepare(
    `INSERT INTO traces
       (id, name, source, skill, session_id, tags, input, output, metadata, observations, started_at)
     VALUES
       (@id, @name, @source, @skill, @session_id, @tags, @input, @output, @metadata, @observations, @started_at)
     ON CONFLICT(id) DO UPDATE SET
       name = excluded.name,
       source = excluded.source,
       skill = excluded.skill,
       session_id = excluded.session_id,
       tags = excluded.tags,
       input = excluded.input,
       output = excluded.output,
       metadata = excluded.metadata,
       observations = excluded.observations,
       started_at = excluded.started_at`,
  );
  const selectTrace = db.prepare(`SELECT * FROM traces WHERE id = ?`);
  const selectRecent = db.prepare(
    `SELECT id, name, tags, started_at FROM traces ORDER BY started_at DESC LIMIT ?`,
  );
  // LEFT JOIN + IS NULL = traces with no judgement row for this (provider,
  // version). One query, no per-trace round-trip.
  const selectRecentUnjudged = db.prepare(
    `SELECT t.id, t.name, t.tags, t.started_at
       FROM traces t
       LEFT JOIN judgements j
         ON j.trace_id = t.id AND j.provider = ? AND j.prompt_version = ?
      WHERE j.trace_id IS NULL
      ORDER BY t.started_at DESC
      LIMIT ?`,
  );
  const upsertJudgement = db.prepare(
    `INSERT INTO judgements
       (trace_id, observation_id, provider, prompt_version, node_kind, skill,
        query_formulation, process, coverage, composition, faithfulness, detail)
     VALUES
       (@trace_id, @observation_id, @provider, @prompt_version, @node_kind, @skill,
        @query_formulation, @process, @coverage, @composition, @faithfulness, @detail)
     ON CONFLICT(trace_id, observation_id, provider, prompt_version) DO UPDATE SET
       node_kind = excluded.node_kind,
       skill = excluded.skill,
       query_formulation = excluded.query_formulation,
       process = excluded.process,
       coverage = excluded.coverage,
       composition = excluded.composition,
       faithfulness = excluded.faithfulness,
       detail = excluded.detail,
       created_at = datetime('now')`,
  );
  function rowToSummary(row: Pick<TraceRow, "id" | "name" | "tags" | "started_at">): TraceSummary {
    return {
      id: row.id,
      name: row.name,
      timestamp: row.started_at,
      tags: parseJson<string[]>(row.tags, []),
    };
  }

  return {
    writeTrace(t) {
      upsertTrace.run({
        id: t.id,
        name: t.name,
        source: t.source,
        skill: t.skill,
        session_id: t.sessionId,
        tags: JSON.stringify(t.tags),
        input: t.input === undefined ? null : JSON.stringify(t.input),
        output: t.output === undefined ? null : JSON.stringify(t.output),
        metadata: t.metadata === null ? null : JSON.stringify(t.metadata),
        observations: JSON.stringify(t.observations),
        started_at: t.startedAt,
      });
    },

    getTrace(id) {
      const row = selectTrace.get(id) as TraceRow | undefined;
      if (!row) return null;
      const observations = parseJson<Observation[]>(row.observations, []);
      const trace: Trace = {
        id: row.id,
        name: row.name,
        sessionId: row.session_id,
        timestamp: row.started_at,
        input: parseJson<unknown>(row.input, null),
        output: parseJson<unknown>(row.output, null),
        metadata: parseJson<Record<string, unknown> | null>(row.metadata, null),
        observations,
        latency: 0,
        totalCost: 0,
        tags: parseJson<string[]>(row.tags, []),
      };
      return { trace, observations };
    },

    listRecent(limit, unjudgedFor) {
      const rows = unjudgedFor
        ? (selectRecentUnjudged.all(unjudgedFor.provider, unjudgedFor.promptVersion, limit) as TraceRow[])
        : (selectRecent.all(limit) as TraceRow[]);
      return rows.map(rowToSummary);
    },

    writeJudgement(j) {
      upsertJudgement.run({
        trace_id: j.traceId,
        observation_id: j.observationId,
        provider: j.provider,
        prompt_version: j.promptVersion,
        node_kind: j.nodeKind,
        skill: j.skill,
        query_formulation: j.scores.query_formulation,
        process: j.scores.process,
        coverage: j.scores.coverage,
        composition: j.scores.composition,
        faithfulness: j.scores.faithfulness,
        detail: JSON.stringify(j.detail),
      });
    },
  };
}
