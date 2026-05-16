// Provider-agnostic tracing interfaces. The Session and Engine speak ONLY
// these types — concrete backends (Langfuse, etc.) live behind an adapter
// that returns objects matching these shapes. Replacing the backend means
// adding a new adapter and swapping it in `createEngine`; no Session edits.
//
// Hierarchy: a `TraceContext` is anything that can host nested children
// (generations and spans). The root of a session is a Trace; spans can
// themselves host children, enabling deep nesting (e.g. a sub-agent's
// iterations rendered inside the parent's `invoke_sub_agent` span).

export interface TokenUsage {
  input: number;
  output: number;
  total: number;
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
}

export interface SpanStartOpts {
  name: string;
  input?: unknown;
}

// Anything that can hold nested generations/spans and have its own
// input/output/metadata updated. Both a Trace (session root) and a Span
// (nested unit of work) qualify.
export interface TraceContext {
  update(data: TraceContextUpdate): void;
  generation(opts: GenerationStartOpts): Generation;
  span(opts: SpanStartOpts): Span;
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
};
const NOOP_TRACE: Trace = {
  update() {},
  generation: () => NOOP_GENERATION,
  span: () => NOOP_SPAN,
  end() {},
};

export const nullTracer: Tracer = {
  trace: () => NOOP_TRACE,
  async shutdown() {},
};
