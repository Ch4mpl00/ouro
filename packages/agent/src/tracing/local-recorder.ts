import { randomUUID } from "node:crypto";
import type { Observation, ObservationType } from "../trace-model";
import { resolveSkill } from "../trace-model";
import type { TraceStore } from "../db/trace-store";
import type {
  EventStartOpts,
  Generation,
  GenerationEndOpts,
  GenerationStartOpts,
  Span,
  SpanEndOpts,
  SpanKind,
  SpanStartOpts,
  Trace,
  TraceContextUpdate,
  TraceStartOpts,
  Tracer,
} from "./index";

// Local-recording Tracer. Mirrors the same calls the Langfuse adapter
// receives into an in-memory observation tree, then flushes one row to the
// TraceStore on trace.end(). Used as the secondary leg of teeTracer so every
// run lands in agent.db regardless of Langfuse uptime. Faithful to the
// canonical READ shape (../trace-model) — what comes back out of the store is
// structurally identical to a Langfuse fetch, so the judge can't tell them
// apart.

function nowIso(): string {
  return new Date().toISOString();
}

function kindToType(kind: SpanKind | undefined): ObservationType {
  switch (kind) {
    case "tool":
      return "TOOL";
    case "agent":
      return "AGENT";
    case "chain":
      return "CHAIN";
    default:
      return "SPAN";
  }
}

function mergeMeta(
  base: Record<string, unknown> | null,
  patch: Record<string, unknown> | undefined,
): Record<string, unknown> | null {
  if (patch === undefined) return base;
  return { ...(base ?? {}), ...patch };
}

// Per-trace accumulator. One instance lives for the duration of a run and is
// shared by the root and every nested span/generation.
interface RunState {
  id: string;
  name: string;
  sessionId: string | null;
  tags: string[];
  metadata: Record<string, unknown> | null;
  input: unknown;
  output: unknown;
  startedAt: string;
  observations: Observation[];
}

function newObservation(
  parentId: string | null,
  name: string,
  type: ObservationType,
  fields: Partial<Observation>,
): Observation {
  const start = nowIso();
  return {
    id: randomUUID(),
    name,
    type,
    parentObservationId: parentId,
    startTime: start,
    endTime: start,
    level: "DEFAULT",
    statusMessage: null,
    input: null,
    output: null,
    metadata: null,
    model: null,
    modelParameters: null,
    usage: null,
    usageDetails: null,
    calculatedTotalCost: null,
    latency: 0,
    ...fields,
  };
}

function makeGeneration(state: RunState, parentId: string, opts: GenerationStartOpts): Generation {
  const obs = newObservation(parentId, opts.name, "GENERATION", {
    // Use the forced id (the tee passes the Langfuse observation id) so judge
    // scores written against this node link back to the right Langfuse step.
    ...(opts.id ? { id: opts.id } : {}),
    input: opts.input ?? null,
    model: opts.model,
    modelParameters: opts.modelParameters ?? null,
    metadata: opts.metadata ?? null,
  });
  state.observations.push(obs);
  return {
    id: obs.id,
    end(o: GenerationEndOpts): void {
      obs.endTime = nowIso();
      if (o.output !== undefined) obs.output = o.output;
      if (o.level !== undefined) obs.level = o.level;
      if (o.statusMessage !== undefined) obs.statusMessage = o.statusMessage;
      if (o.metadata !== undefined) obs.metadata = mergeMeta(obs.metadata, o.metadata);
      if (o.usage) {
        obs.usage = { input: o.usage.input, output: o.usage.output, total: o.usage.total };
        obs.usageDetails = {
          input: o.usage.input,
          output: o.usage.output,
          total: o.usage.total,
          ...(o.usage.cached !== undefined ? { cached: o.usage.cached } : {}),
        };
      }
    },
  };
}

function makeSpan(state: RunState, parentId: string, opts: SpanStartOpts): Span {
  const obs = newObservation(parentId, opts.name, kindToType(opts.kind), {
    // Forced id (Langfuse observation id) so per-node scores link — see makeGeneration.
    ...(opts.id ? { id: opts.id } : {}),
    input: opts.input ?? null,
    metadata: opts.metadata ?? null,
  });
  state.observations.push(obs);
  return {
    id: obs.id,
    update(data: TraceContextUpdate): void {
      if (data.input !== undefined) obs.input = data.input;
      if (data.output !== undefined) obs.output = data.output;
      obs.metadata = mergeMeta(obs.metadata, data.metadata);
    },
    end(o: SpanEndOpts): void {
      obs.endTime = nowIso();
      if (o.output !== undefined) obs.output = o.output;
      if (o.level !== undefined) obs.level = o.level;
      if (o.statusMessage !== undefined) obs.statusMessage = o.statusMessage;
    },
    generation: (o) => makeGeneration(state, obs.id, o),
    span: (o) => makeSpan(state, obs.id, o),
    event: (o) => pushEvent(state, obs.id, o),
  };
}

function pushEvent(state: RunState, parentId: string, opts: EventStartOpts): void {
  state.observations.push(
    newObservation(parentId, opts.name, "EVENT", {
      input: opts.input ?? null,
      metadata: opts.metadata ?? null,
      level: opts.level ?? "DEFAULT",
    }),
  );
}

export function createLocalRecorderTracer(store: TraceStore): Tracer {
  return {
    trace(opts: TraceStartOpts): Trace {
      const state: RunState = {
        id: opts.id,
        name: opts.name,
        sessionId: opts.sessionId ?? null,
        tags: opts.tags ?? [],
        metadata: opts.metadata ?? null,
        input: undefined,
        output: undefined,
        startedAt: nowIso(),
        observations: [],
      };
      // Root observation mirrors the trace; the judge skips it (it matches
      // trace.name with a null parent) but it keeps the tree well-formed.
      const root = newObservation(null, opts.name, kindToType(opts.kind), {
        metadata: opts.metadata ?? null,
      });
      state.observations.push(root);

      return {
        id: state.id,
        update(data: TraceContextUpdate): void {
          if (data.input !== undefined) state.input = data.input;
          if (data.output !== undefined) state.output = data.output;
          state.metadata = mergeMeta(state.metadata, data.metadata);
        },
        generation: (o) => makeGeneration(state, root.id, o),
        span: (o) => makeSpan(state, root.id, o),
        event: (o) => pushEvent(state, root.id, o),
        end(): void {
          store.writeTrace({
            id: state.id,
            name: state.name,
            source: state.tags[0] ?? null,
            skill: resolveSkill(state.observations, state.metadata),
            sessionId: state.sessionId,
            tags: state.tags,
            input: state.input,
            output: state.output,
            metadata: state.metadata,
            observations: state.observations,
            startedAt: state.startedAt,
          });
        },
      };
    },
    async shutdown(): Promise<void> {
      // Writes are synchronous on trace.end() — nothing buffered to flush.
    },
  };
}
