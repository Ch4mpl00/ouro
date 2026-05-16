// Provider-agnostic tracing interfaces. The Session and Engine speak ONLY
// these types — concrete backends (Langfuse, etc.) live behind an adapter
// that returns objects matching these shapes. Replacing the backend means
// adding a new adapter and swapping it in `createEngine`; no Session edits.
//
// The shape mirrors the standard trace → (generation|span) hierarchy used
// by most LLM-observability tools: one trace per session, one generation
// per LLM call, one span per tool call.

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

export interface Span {
  update(data: { input?: unknown }): void;
  end(opts: SpanEndOpts): void;
}

export interface Generation {
  end(opts: GenerationEndOpts): void;
}

export interface TraceUpdate {
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
  modelParameters?: Record<string, string | number | boolean | string[] | null>;
  input?: unknown;
}

export interface SpanStartOpts {
  name: string;
  input?: unknown;
}

export interface Trace {
  update(data: TraceUpdate): void;
  generation(opts: GenerationStartOpts): Generation;
  span(opts: SpanStartOpts): Span;
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
// Session free of `?.` chains.
const NOOP_SPAN: Span = {
  update() {},
  end() {},
};
const NOOP_GENERATION: Generation = {
  end() {},
};
const NOOP_TRACE: Trace = {
  update() {},
  generation: () => NOOP_GENERATION,
  span: () => NOOP_SPAN,
};

export const nullTracer: Tracer = {
  trace: () => NOOP_TRACE,
  async shutdown() {},
};
