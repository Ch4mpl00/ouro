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
}

export interface Generation {
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
  end(opts: SpanEndOpts): void;
}

// Trace is the session-level root. Same shape as TraceContext plus an
// explicit `end()` — OTel-based backends (Langfuse v5) need the root span
// closed before flush, or the trace shows up as "in progress" forever.
// In-process backends with implicit lifecycles (or the null tracer) can
// treat `end` as a no-op.
export interface Trace extends TraceContext {
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
const NOOP_GENERATION: Generation = { end() {} };
const NOOP_SPAN: Span = {
  update() {},
  end() {},
  generation: () => NOOP_GENERATION,
  span: () => NOOP_SPAN,
  event() {},
};
const NOOP_TRACE: Trace = {
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
