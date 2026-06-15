import { fetchRecentTraces, fetchTraceById } from "../scripts/langfuse-api";
import type { Observation, Trace, TraceSummary } from "../trace-model";
import type { TraceStore } from "../db/trace-store";

// Where the judge reads runs from. Two implementations — the local mirror
// (fast, complete, Langfuse-independent) and the Langfuse public API — behind
// one interface so material assembly is source-agnostic. The online worker
// reads local (the forward-looking corpus the self-improvement loop is built
// on); the manual CLI keeps reading Langfuse.

export interface TraceSource {
  getTrace(id: string): Promise<{ trace: Trace; observations: Observation[] }>;
  // Newest-first. `unjudgedFor` (local only) returns just the runs without a
  // judgement for that (provider, promptVersion) — the dedup lives in the
  // query, not a separate KV.
  recentTraces(
    limit: number,
    unjudgedFor?: { provider: string; promptVersion: string },
  ): Promise<TraceSummary[]>;
}

export function createLangfuseTraceSource(): TraceSource {
  return {
    getTrace: (id) => fetchTraceById(id),
    recentTraces: (limit) => fetchRecentTraces(limit),
  };
}

// Reads from the local store. A trace is only written on trace.end(), so
// anything in the store is a COMPLETE run — no "is it still running?" age
// filter needed (unlike the Langfuse list, which can surface mid-run traces).
// `fallback` (e.g. Langfuse) covers getTrace for ids not mirrored locally.
export function createLocalTraceSource(store: TraceStore, fallback?: TraceSource): TraceSource {
  return {
    async getTrace(id) {
      const local = store.getTrace(id);
      if (local) return local;
      if (fallback) return fallback.getTrace(id);
      throw new Error(`trace ${id} not in local store and no fallback configured`);
    },
    async recentTraces(limit, unjudgedFor) {
      return store.listRecent(limit, unjudgedFor);
    },
  };
}
