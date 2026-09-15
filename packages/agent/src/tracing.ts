import { LangfuseSpanProcessor } from "@langfuse/otel";
import {
  LangfuseOtelSpanAttributes,
  startObservation,
  type LangfuseGeneration,
  type LangfuseGenerationAttributes,
  type LangfuseSpan,
  type LangfuseSpanAttributes,
} from "@langfuse/tracing";
import { NodeTracerProvider } from "@opentelemetry/sdk-trace-node";
import { randomUUID } from "node:crypto";
import type { TraceStore } from "./db";

// Observability, end to end: the canonical READ shape of a recorded run, the
// write-side Tracer interface the agent runtime emits through, and the three
// backends behind it — Langfuse, the local mirror, and the tee that fans one
// stream out to both.
//
// Read top to bottom — each section only depends on the ones above it:
//
//   trace model       Observation / TraceRecord / TraceSummary — what a run
//                     reads back as, plus the judge-node tag and resolveSkill
//   tracer interface  Trace / Span / Generation handles + nullTracer; the only
//                     tracing types the agent runtime ever names
//   langfuse          Langfuse v5 adapter, on OpenTelemetry
//   local recorder    mirrors the same calls into agent.db via TraceStore
//   tee               one tracer in, two backends out
//
// Outside this file: ./db/trace-store owns the table the recorder writes to
// (imported type-only, so the cycle between the two is erased at compile
// time), and judging/ consumes the read shape.

// ═══════════════════════════════════════════════════════════════════
// Trace model
// ═══════════════════════════════════════════════════════════════════

// Canonical READ shape of an agent run's trace — the structure the judge and
// the self-improvement loop consume. It mirrors the Langfuse public-API
// response so a local mirror and a Langfuse fetch are interchangeable behind
// the `TraceSource` interface. Pure types + one pure resolver, no I/O and no
// env reads — nothing in this section runs at import time, which is what lets
// the store and the judge depend on it even though the module also carries the
// Langfuse adapter below.

// Langfuse observation types. Beyond the original GENERATION/SPAN/EVENT, v5
// adds typed spans (AGENT/TOOL/CHAIN/RETRIEVER/…) the agent emits via `kind`.
// Only GENERATION/EMBEDDING carry model + token usage.
export type ObservationType =
  | "GENERATION"
  | "SPAN"
  | "EVENT"
  | "AGENT"
  | "TOOL"
  | "CHAIN"
  | "RETRIEVER"
  | "EVALUATOR"
  | "GUARDRAIL"
  | "EMBEDDING";

export interface Observation {
  id: string;
  name: string;
  type: ObservationType;
  parentObservationId: string | null;
  startTime: string;
  endTime: string;
  level: string;
  statusMessage: string | null;
  input: unknown;
  output: unknown;
  metadata: Record<string, unknown> | null;
  model: string | null;
  modelParameters: Record<string, unknown> | null;
  usage: { input: number; output: number; total: number } | null;
  usageDetails: Record<string, number> | null;
  calculatedTotalCost: number | null;
  latency: number;
}

export interface TraceRecord {
  id: string;
  name: string;
  sessionId: string | null;
  timestamp: string;
  input: unknown;
  output: unknown;
  metadata: Record<string, unknown> | null;
  // /traces?sessionId=... returns observation IDs; /traces/<id> inlines full
  // Observation objects. Both shapes are handled by fetchTraceById.
  observations: Array<string | Observation>;
  latency: number;
  totalCost: number;
  tags: string[];
}

export interface TraceSummary {
  id: string;
  name: string;
  timestamp: string;
  tags: string[];
}

// A judgeable generation tags itself with its node role under
// metadata[JUDGE_NODE_META], so the per-node judge classifies by an explicit
// producer→judge contract rather than by observation NAMES ("attempt-1",
// "llm_compose:digest") — those stay display-only and can be renamed freely.
// Kept separate from metadata.skill on purpose: `skill` means "which skill
// composed the output" (drives resolveSkill / the traces.skill column), and a
// planner generation must NOT claim that role. Agent nodes need no tag — an
// llm_agent step is already an AGENT-type span.
export const JUDGE_NODE_META = "judge_node";
export type JudgeNodeRole = "planner" | "compose";

// Which skill composed the output. Workflow path stamps it on the
// llm_compose / llm_agent step observation's metadata.skill; agent-loop path
// lists it on trace.metadata.skills. First non-empty wins. Used both at
// record time (to fill the traces.skill column) and by the judge.
export function resolveSkill(
  observations: Array<Pick<Observation, "metadata">>,
  traceMetadata: Record<string, unknown> | null,
): string | null {
  for (const o of observations) {
    const skill = o.metadata?.skill;
    if (typeof skill === "string" && skill.length > 0) return skill;
  }
  const skills = traceMetadata?.skills;
  if (Array.isArray(skills) && typeof skills[0] === "string") return skills[0];
  return null;
}

// ═══════════════════════════════════════════════════════════════════
// Tracer interface
// ═══════════════════════════════════════════════════════════════════

// Provider-agnostic tracing interfaces. The AgentLoop and Engine speak ONLY
// these types — concrete backends (Langfuse, etc.) live behind an adapter
// that returns objects matching these shapes. Replacing the backend means
// adding a new adapter and swapping it in the composition root (supervisor
// main); no AgentLoop edits.
//
// Hierarchy: a `TraceContext` is anything that can host nested children
// (generations and spans). The root of a session is a Trace; spans can
// themselves host children, enabling deep nesting (e.g. a sub-agent's
// iterations rendered inside the parent's `invoke_sub_agent` span).

export interface TokenUsage {
  input: number;
  output: number;
  total: number;
  // Portion of `input` served from the provider's prompt cache, when the
  // provider reports it (OpenAI: prompt_tokens_details.cached_tokens).
  // Purely informational — surfaced in the trace so cache hits are visible.
  cached?: number;
}

export interface SpanEndOpts {
  output?: unknown;
  level?: "ERROR";
  statusMessage?: string;
}

export interface GenerationEndOpts extends SpanEndOpts {
  usage?: TokenUsage;
  // Metadata known only at end time, merged over what `generation()` set at
  // start. Lets a producer tag a generation conditionally on its outcome
  // (e.g. mark only the ACCEPTED planner attempt as the judgeable node).
  metadata?: Record<string, unknown>;
}

export interface Generation {
  // The backend's observation id. For Langfuse this is the OTel span id — the
  // same id the scores API targets — so the tee can force it onto the local
  // mirror and per-observation judge scores link back to the right step.
  readonly id: string;
  end(opts: GenerationEndOpts): void;
}

export interface TraceContextUpdate {
  input?: unknown;
  output?: unknown;
  metadata?: Record<string, unknown>;
}

export interface GenerationStartOpts {
  name: string;
  model: string;
  // Force a specific observation id (the tee passes the primary backend's id so
  // the secondary mirror shares it). Backends that own their id (Langfuse/OTel)
  // ignore it; the local recorder uses it instead of a random one.
  id?: string;
  // Scalar-only by design: most observability backends index these for
  // filtering, so structured objects don't belong here. Stick to the LLM
  // request parameters (temperature, top_p, reasoning_effort, ...).
  modelParameters?: Record<string, string | number>;
  input?: unknown;
  // Per-observation metadata. Use for short identity markers (`agent_id`,
  // `parent_id`) so the UI can distinguish observations from different
  // sessions in the same trace — trace.metadata alone makes nested
  // sub-agent observations look indistinguishable from the parent's.
  metadata?: Record<string, unknown>;
}

// Observation kind — how a span renders in the tracing UI. Purely
// presentational: each kind gets a distinct icon/colour, no behavioural
// difference. Maps to Langfuse's `asType`; backends that don't model
// observation types (the null tracer) ignore it. We expose only the
// kinds this codebase actually produces:
//   - "tool"   a single tool / function call
//   - "agent"  a spawned sub-agent (its own iters/tool calls nest inside)
//   - "chain"  a multi-step unit of work (workflow runner, a workflow
//              step, the compiler's retry loop)
//   - "span"   generic fallback (the default when omitted)
export type SpanKind = "tool" | "agent" | "chain" | "span";

export interface SpanStartOpts {
  name: string;
  input?: unknown;
  metadata?: Record<string, unknown>;
  // Defaults to "span" when omitted.
  kind?: SpanKind;
  // Force a specific observation id (see GenerationStartOpts.id).
  id?: string;
}

// A point-in-time marker with no duration and no children — renders as a
// timeline tick in the UI. Unlike a span there is no handle to close: the
// backend auto-ends it. Use for discrete moments worth flagging on the
// trace (e.g. the workflow→agentic fallback transition), NOT for units of
// work that contain other observations — those are spans.
export interface EventStartOpts {
  name: string;
  input?: unknown;
  metadata?: Record<string, unknown>;
  // Severity badge. Omit for a neutral marker; "WARNING" for a notable
  // but non-fatal moment (degraded path), "ERROR" for a failure point.
  level?: "WARNING" | "ERROR";
}

// Anything that can hold nested generations/spans and have its own
// input/output/metadata updated. Both a Trace (session root) and a Span
// (nested unit of work) qualify.
export interface TraceContext {
  update(data: TraceContextUpdate): void;
  generation(opts: GenerationStartOpts): Generation;
  span(opts: SpanStartOpts): Span;
  // Point-in-time marker (auto-ended, no handle returned). See EventStartOpts.
  event(opts: EventStartOpts): void;
}

// A Span is a TraceContext with a lifecycle terminator. Use `end` to set
// the final output + status; `update` for intermediate refinements.
export interface Span extends TraceContext {
  // The backend's observation id (see Generation.id).
  readonly id: string;
  end(opts: SpanEndOpts): void;
}

// Trace is the session-level root. Same shape as TraceContext plus an
// explicit `end()` — OTel-based backends (Langfuse v5) need the root span
// closed before flush, or the trace shows up as "in progress" forever.
// In-process backends with implicit lifecycles (or the null tracer) can
// treat `end` as a no-op.
export interface Trace extends TraceContext {
  // The backend's trace id, known at creation time. For Langfuse this is the
  // OTel trace id (the same hex the scores API and UI use) — the tee reads it
  // off the primary tracer and keys the local mirror on it, so scores link
  // back to Langfuse with no mapping table.
  readonly id: string;
  end(): void;
}

export interface TraceStartOpts {
  id: string;
  name: string;
  sessionId?: string;
  tags?: string[];
  metadata?: Record<string, unknown>;
  // Observation kind for the trace's root span. Defaults to "span"; pass
  // "agent" for a top-level agent run so the UI badges the whole trace
  // accordingly.
  kind?: SpanKind;
}

export interface Tracer {
  trace(opts: TraceStartOpts): Trace;
  // Flush buffered events. Called once on engine shutdown.
  shutdown(): Promise<void>;
}

// No-op tracer used when external tracing is disabled. Returning concrete
// no-op objects (instead of forcing every call site to null-check) keeps
// Session free of `?.` chains. The NOOP_SPAN self-reference works because
// the property is a function — by the time it runs, the const exists.
const NOOP_GENERATION: Generation = { id: "", end() {} };
const NOOP_SPAN: Span = {
  id: "",
  update() {},
  end() {},
  generation: () => NOOP_GENERATION,
  span: () => NOOP_SPAN,
  event() {},
};
const NOOP_TRACE: Trace = {
  id: "",
  update() {},
  generation: () => NOOP_GENERATION,
  span: () => NOOP_SPAN,
  event() {},
  end() {},
};

export const nullTracer: Tracer = {
  trace: () => NOOP_TRACE,
  async shutdown() {},
};

// ═══════════════════════════════════════════════════════════════════
// Langfuse adapter
// ═══════════════════════════════════════════════════════════════════

// Langfuse v5 adapter. Built on OpenTelemetry — the LangfuseSpanProcessor
// reads spans created via @langfuse/tracing's `startObservation` chain and
// ships them to Langfuse Cloud. v5 sends the `x-langfuse-ingestion-version: 4`
// header which moves traces into the fast-ingestion lane (sub-second UI
// freshness), unlike v3 which had a 5-10min compatibility-tier delay.
//
// We use the imperative `startObservation` / `parent.startObservation(...)`
// chain rather than the callback patterns (`startActiveObservation`,
// `propagateAttributes`) so Session keeps its current shape: open scope,
// open children, close. Children are always created via explicit parent —
// no OTel active-context juggling needed.

interface LangfuseTracerConfig {
  publicKey: string;
  secretKey: string;
  baseUrl?: string;
}

export function createLangfuseTracer(config: LangfuseTracerConfig): Tracer {
  const processor = new LangfuseSpanProcessor({
    publicKey: config.publicKey,
    secretKey: config.secretKey,
    baseUrl: config.baseUrl,
  });
  const provider = new NodeTracerProvider({ spanProcessors: [processor] });
  provider.register();

  return {
    trace(opts: TraceStartOpts): Trace {
      // No active OTel context here → the new observation becomes a root
      // span, which Langfuse renders as a top-level trace. `kind` badges
      // the whole trace (e.g. "agent" for a signal-handling session).
      const root = startRootObservation(opts.name, opts.metadata, opts.kind);
      // Trace-level attributes (sessionId, tags, traceName) live on
      // well-known OTel attribute keys that LangfuseSpanProcessor reads
      // off the root span. Children created via `root.startObservation`
      // pick up the trace association from the OTel parent link.
      const otel = root.otelSpan;
      otel.setAttribute(LangfuseOtelSpanAttributes.TRACE_NAME, opts.name);
      if (opts.sessionId !== undefined) {
        otel.setAttribute(LangfuseOtelSpanAttributes.TRACE_SESSION_ID, opts.sessionId);
      }
      if (opts.tags && opts.tags.length > 0) {
        otel.setAttribute(LangfuseOtelSpanAttributes.TRACE_TAGS, opts.tags);
      }
      return wrapTrace(root);
    },
    async shutdown(): Promise<void> {
      // forceFlush ships any buffered spans; provider.shutdown closes
      // the OTel pipeline. Both are required to avoid dropping the
      // final batch on SIGTERM.
      await processor.forceFlush();
      await provider.shutdown();
    },
  };
}

// ─── wrappers ────────────────────────────────────────────────────────

function applyContextUpdate(s: LangfuseSpan, data: TraceContextUpdate): void {
  const patch: Record<string, unknown> = {};
  if (data.input !== undefined) patch.input = data.input;
  if (data.output !== undefined) patch.output = data.output;
  if (data.metadata !== undefined) patch.metadata = data.metadata;
  if (Object.keys(patch).length > 0) s.update(patch);
}

function startGenerationChild(parent: LangfuseSpan, opts: GenerationStartOpts): LangfuseGeneration {
  const attrs: LangfuseGenerationAttributes = {
    input: opts.input,
    model: opts.model,
    modelParameters: opts.modelParameters,
    metadata: opts.metadata,
  };
  return parent.startObservation(opts.name, attrs, { asType: "generation" });
}

// Root-level counterpart of startSpanChild: opens a top-level observation
// (which Langfuse renders as a trace) with the requested kind. No active
// OTel parent context → it becomes a root span.
function startRootObservation(
  name: string,
  metadata: Record<string, unknown> | undefined,
  kind: SpanKind | undefined,
): LangfuseSpan {
  switch (kind) {
    case "tool":
      return startObservation(name, { metadata }, { asType: "tool" });
    case "agent":
      return startObservation(name, { metadata }, { asType: "agent" });
    case "chain":
      return startObservation(name, { metadata }, { asType: "chain" });
    default:
      return startObservation(name, { metadata });
  }
}

// Map our backend-agnostic SpanKind to Langfuse's `asType`. The `tool` /
// `agent` / `chain` observation classes are structurally identical to
// LangfuseSpan (their attribute type IS LangfuseSpanAttributes), differing
// only in the UI badge — so they wrap with the same LangfuseSpan logic.
// The literal `asType` is required for overload resolution; a switch keeps
// it literal without casting a dynamic string.
function startSpanChild(parent: LangfuseSpan, opts: SpanStartOpts): LangfuseSpan {
  const attrs: LangfuseSpanAttributes = { input: opts.input, metadata: opts.metadata };
  switch (opts.kind) {
    case "tool":
      return parent.startObservation(opts.name, attrs, { asType: "tool" });
    case "agent":
      return parent.startObservation(opts.name, attrs, { asType: "agent" });
    case "chain":
      return parent.startObservation(opts.name, attrs, { asType: "chain" });
    default:
      return parent.startObservation(opts.name, attrs, { asType: "span" });
  }
}

// Point-in-time marker. Langfuse auto-ends `event` observations, so there
// is nothing to close and no handle to return.
function startEventChild(parent: LangfuseSpan, opts: EventStartOpts): void {
  parent.startObservation(
    opts.name,
    { input: opts.input, metadata: opts.metadata, level: opts.level },
    { asType: "event" },
  );
}

function wrapTrace(s: LangfuseSpan): Trace {
  // The OTel trace id is assigned when the span is created (part of its
  // SpanContext), not at export time — so it's valid immediately, even if
  // Langfuse ingestion later 502s. This IS the Langfuse trace id.
  const id = s.otelSpan.spanContext().traceId;
  return {
    id,
    update(data: TraceContextUpdate): void {
      applyContextUpdate(s, data);
    },
    generation(opts: GenerationStartOpts): Generation {
      return wrapGeneration(startGenerationChild(s, opts));
    },
    span(opts: SpanStartOpts): Span {
      return wrapSpan(startSpanChild(s, opts));
    },
    event(opts: EventStartOpts): void {
      startEventChild(s, opts);
    },
    end(): void {
      s.end();
    },
  };
}

function wrapSpan(s: LangfuseSpan): Span {
  return {
    // OTel span id = the Langfuse observation id (see wrapGeneration).
    id: s.otelSpan.spanContext().spanId,
    update(data: TraceContextUpdate): void {
      applyContextUpdate(s, data);
    },
    end(opts: SpanEndOpts): void {
      const patch: Record<string, unknown> = {};
      if (opts.output !== undefined) patch.output = opts.output;
      if (opts.level !== undefined) patch.level = opts.level;
      if (opts.statusMessage !== undefined) patch.statusMessage = opts.statusMessage;
      if (Object.keys(patch).length > 0) s.update(patch);
      s.end();
    },
    generation(opts: GenerationStartOpts): Generation {
      return wrapGeneration(startGenerationChild(s, opts));
    },
    span(opts: SpanStartOpts): Span {
      return wrapSpan(startSpanChild(s, opts));
    },
    event(opts: EventStartOpts): void {
      startEventChild(s, opts);
    },
  };
}

function wrapGeneration(g: LangfuseGeneration): Generation {
  return {
    // OTel span id = the Langfuse observation id the scores API targets.
    id: g.otelSpan.spanContext().spanId,
    end(opts: GenerationEndOpts): void {
      const patch: Record<string, unknown> = {};
      if (opts.output !== undefined) patch.output = opts.output;
      if (opts.level !== undefined) patch.level = opts.level;
      if (opts.statusMessage !== undefined) patch.statusMessage = opts.statusMessage;
      if (opts.metadata !== undefined) patch.metadata = opts.metadata;
      if (opts.usage) {
        patch.usageDetails = {
          input: opts.usage.input,
          output: opts.usage.output,
          total: opts.usage.total,
          // Langfuse renders extra usageDetails keys as-is — show the
          // cached-input portion when the provider reported it.
          ...(opts.usage.cached !== undefined ? { cached: opts.usage.cached } : {}),
        };
      }
      if (Object.keys(patch).length > 0) g.update(patch);
      g.end();
    },
  };
}

// Auto-configure from env. Returns null if keys missing — the caller
// substitutes `nullTracer`.
export function langfuseTracerFromEnv(): Tracer | null {
  const secretKey = process.env.LANGFUSE_SECRET_KEY;
  const publicKey = process.env.LANGFUSE_PUBLIC_KEY;
  const baseUrl = process.env.LANGFUSE_BASE_URL;
  if (!secretKey || !publicKey) return null;
  return createLangfuseTracer({ secretKey, publicKey, baseUrl });
}

// ═══════════════════════════════════════════════════════════════════
// Local recorder
// ═══════════════════════════════════════════════════════════════════

// Local-recording Tracer. Mirrors the same calls the Langfuse adapter
// receives into an in-memory observation tree, then flushes one row to the
// TraceStore on trace.end(). Used as the secondary leg of teeTracer so every
// run lands in agent.db regardless of Langfuse uptime. Faithful to the
// canonical READ shape above — what comes back out of the store is
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

// ═══════════════════════════════════════════════════════════════════
// Tee
// ═══════════════════════════════════════════════════════════════════

// Fan one tracer's calls out to two backends. Named after unix `tee`: one
// input stream, two sinks. Used to mirror every run into the local store
// (./db/trace-store) while still shipping to Langfuse — the agent emits
// through ONE tracer and stays unaware there are two.
//
// The trace id comes from the PRIMARY (Langfuse = the OTel trace id) and is
// forced onto the SECONDARY (local) so the mirror keys on the same id — that
// is what lets scores written later link back to Langfuse with no mapping.
// Children (span/generation/event) get independent ids in each backend; the
// judge never cross-references child ids across systems, so no override is
// needed below the root.

// Create a child on the primary, then the secondary WITH the primary's id
// forced — so the local mirror's observation id == the Langfuse observation id,
// and per-observation judge scores link back to the right step.
function forkGeneration(a: TraceContext, b: TraceContext, opts: GenerationStartOpts): Generation {
  const ga = a.generation(opts);
  const gb = b.generation({ ...opts, id: ga.id });
  return teeGeneration(ga, gb);
}

function forkSpan(a: TraceContext, b: TraceContext, opts: SpanStartOpts): Span {
  const sa = a.span(opts);
  const sb = b.span({ ...opts, id: sa.id });
  return teeSpan(sa, sb);
}

function teeGeneration(a: Generation, b: Generation): Generation {
  return {
    id: a.id,
    end(opts: GenerationEndOpts): void {
      a.end(opts);
      b.end(opts);
    },
  };
}

function teeSpan(a: Span, b: Span): Span {
  return {
    id: a.id,
    update(data: TraceContextUpdate): void {
      a.update(data);
      b.update(data);
    },
    end(opts: SpanEndOpts): void {
      a.end(opts);
      b.end(opts);
    },
    generation(opts: GenerationStartOpts): Generation {
      return forkGeneration(a, b, opts);
    },
    span(opts: SpanStartOpts): Span {
      return forkSpan(a, b, opts);
    },
    event(opts: EventStartOpts): void {
      a.event(opts);
      b.event(opts);
    },
  };
}

function teeTrace(a: Trace, b: Trace): Trace {
  return {
    id: a.id,
    update(data: TraceContextUpdate): void {
      a.update(data);
      b.update(data);
    },
    generation(opts: GenerationStartOpts): Generation {
      return forkGeneration(a, b, opts);
    },
    span(opts: SpanStartOpts): Span {
      return forkSpan(a, b, opts);
    },
    event(opts: EventStartOpts): void {
      a.event(opts);
      b.event(opts);
    },
    end(): void {
      a.end();
      b.end();
    },
  };
}

export function teeTracer(primary: Tracer, secondary: Tracer): Tracer {
  return {
    trace(opts: TraceStartOpts): Trace {
      const a = primary.trace(opts);
      // Force the primary's id onto the secondary so the local mirror shares
      // the Langfuse trace id.
      const b = secondary.trace({ ...opts, id: a.id });
      return teeTrace(a, b);
    },
    async shutdown(): Promise<void> {
      await Promise.all([primary.shutdown(), secondary.shutdown()]);
    },
  };
}
